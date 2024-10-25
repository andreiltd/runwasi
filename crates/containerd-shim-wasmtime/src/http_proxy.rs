// Heavily inspired by wasmtime serve command:
// https://github.com/bytecodealliance/wasmtime/blob/main/src/commands/serve.rs

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{bail, Result};
use containerd_shim_wasm::container::RuntimeContext;
use hyper::server::conn::http1;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use wasmtime::component::ResourceTable;
use wasmtime::Store;
use wasmtime_wasi_http::bindings::http::types::Scheme;
use wasmtime_wasi_http::bindings::ProxyPre;
use wasmtime_wasi_http::body::HyperOutgoingBody;
use wasmtime_wasi_http::io::TokioIo;
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpView};

use crate::instance::{envs_from_ctx, WasiPreview2Ctx};

const DEFAULT_ADDR: SocketAddr =
    SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::new(0, 0, 0, 0)), 8080);

const DEFAULT_BACKLOG: u32 = 100;

type Request = hyper::Request<hyper::body::Incoming>;

pub(crate) async fn serve_conn(
    ctx: &impl RuntimeContext,
    instance: ProxyPre<WasiPreview2Ctx>,
    cancel: CancellationToken,
) -> Result<()> {
    let mut env = envs_from_ctx(ctx).into_iter().collect::<HashMap<_, _>>();

    // Consume env variables for Proxy server settings before passing it to handler
    let addr = env
        .remove("WASMTIME_HTTP_PROXY_SOCKET_ADDR")
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_ADDR);
    let backlog = env
        .remove("WASMTIME_HTTP_BACKLOG")
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_BACKLOG);

    let socket = match addr {
        SocketAddr::V4(_) => tokio::net::TcpSocket::new_v4()?,
        SocketAddr::V6(_) => tokio::net::TcpSocket::new_v6()?,
    };

    // Conditionally enable `SO_REUSEADDR` depending on the current
    // platform. On Unix we want this to be able to rebind an address in
    // the `TIME_WAIT` state which can happen then a server is killed with
    // active TCP connections and then restarted. On Windows though if
    // `SO_REUSEADDR` is specified then it enables multiple applications to
    // bind the port at the same time which is not something we want. Hence
    // this is conditionally set based on the platform (and deviates from
    // Tokio's default from always-on).
    socket.set_reuseaddr(!cfg!(windows))?;
    socket.bind(addr)?;

    let listener = socket.listen(backlog)?;
    let tracker = TaskTracker::new();

    log::info!("Serving HTTP on http://{}/", listener.local_addr()?);

    let env = env.into_iter().collect();
    let handler = ProxyHandler::new(instance, env);

    loop {
        tokio::select! {
            // listen to cancellation requests
            _ = cancel.cancelled() => {
                break;
            }
            res = listener.accept() => {
                let (stream, _) = res?;
                log::debug!("New connection");

                let stream = TokioIo::new(stream);
                let h = handler.clone();

                tracker.spawn(async {
                    if let Err(e) = http1::Builder::new()
                        .keep_alive(true)
                        .serve_connection(
                            stream,
                            hyper::service::service_fn(move |req| {
                                let handler = h.clone();
                                async move { handler.handle_request(req).await }
                            }),
                        )
                        .await
                    {
                        log::error!("error: {e:?}");
                    }
                });
            }
        }
    }

    tracker.close();
    tracker.wait().await;

    Ok(())
}

struct ProxyHandlerInner {
    instance_pre: ProxyPre<WasiPreview2Ctx>,
    next_id: AtomicU64,
    env: Vec<(String, String)>,
}

impl ProxyHandlerInner {
    fn next_req_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }
}

#[derive(Clone)]
struct ProxyHandler(Arc<ProxyHandlerInner>);

impl ProxyHandler {
    fn new(instance_pre: ProxyPre<WasiPreview2Ctx>, env: Vec<(String, String)>) -> Self {
        Self(Arc::new(ProxyHandlerInner {
            instance_pre,
            env,
            next_id: AtomicU64::from(0),
        }))
    }

    fn wasi_store_for_request(&self, req_id: u64) -> Store<WasiPreview2Ctx> {
        let engine = self.0.instance_pre.engine();
        let mut builder = wasmtime_wasi::WasiCtxBuilder::new();

        builder.envs(&self.0.env);
        builder.env("REQUEST_ID", req_id.to_string());

        let ctx = WasiPreview2Ctx {
            wasi_ctx: builder.build(),
            wasi_http: WasiHttpCtx::new(),
            resource_table: ResourceTable::default(),
        };

        Store::new(engine, ctx)
    }

    async fn handle_request(&self, req: Request) -> Result<hyper::Response<HyperOutgoingBody>> {
        let inner = &self.0;
        let (sender, receiver) = tokio::sync::oneshot::channel();

        let req_id = inner.next_req_id();

        log::info!(
            "Request {req_id} handling {} to {}",
            req.method(),
            req.uri()
        );

        let mut store = self.wasi_store_for_request(req_id);

        let req = store.data_mut().new_incoming_request(Scheme::Http, req)?;
        let out = store.data_mut().new_response_outparam(sender)?;
        let proxy = inner.instance_pre.instantiate_async(&mut store).await?;

        let task = tokio::spawn(async move {
            if let Err(e) = proxy
                .wasi_http_incoming_handler()
                .call_handle(store, req, out)
                .await
            {
                log::error!("[{req_id}] :: {:#?}", e);
                return Err(e);
            }

            Ok(())
        });

        match receiver.await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(e)) => Err(e.into()),
            Err(_) => {
                // An error in the receiver (`RecvError`) only indicates that the
                // task exited before a response was sent (i.e., the sender was
                // dropped); it does not describe the underlying cause of failure.
                // Instead we retrieve and propagate the error from inside the task
                // which should more clearly tell the user what went wrong. Note
                // that we assume the task has already exited at this point so the
                // `await` should resolve immediately.
                let e = match task.await {
                    Ok(e) => {
                        e.expect_err("if the receiver has an error, the task must have failed")
                    }
                    Err(e) => e.into(),
                };

                bail!("guest never invoked `response-outparam::set` method: {e:?}")
            }
        }
    }
}
