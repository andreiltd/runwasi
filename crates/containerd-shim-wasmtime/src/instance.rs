use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;

use anyhow::{bail, Context, Result};
use containerd_shim_wasm::container::{
    Engine, Entrypoint, Instance, RuntimeContext, Stdio, WasmBinaryType,
};
use containerd_shim_wasm::sandbox::WasmLayer;
use tokio_util::sync::CancellationToken;
use wasi_preview1::WasiP1Ctx;
use wasi_preview2::bindings::Command;
use wasmtime::component::types::ComponentItem;
use wasmtime::component::{self, Component, ResourceTable};
use wasmtime::{Config, Module, Precompiled, Store};
use wasmtime_wasi::preview1::{self as wasi_preview1};
use wasmtime_wasi::{self as wasi_preview2};
use wasmtime_wasi_http::bindings::ProxyPre;
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpView};

use crate::http_proxy::serve_conn;

pub type WasmtimeInstance = Instance<WasmtimeEngine<DefaultConfig>>;

/// Represents the WASI API that the component is targeting.
enum ComponentTarget<'a> {
    /// A component that targets WASI command-line interface.
    Command,
    /// A component that targets WASI http/proxy  interface.
    HttpProxy,
    /// Core function. The `&'a str` represents function to call.
    Core(&'a str),
}

impl<'a> ComponentTarget<'a> {
    fn new<'b, I>(exports: I, func: &'a str) -> Self
    where
        I: IntoIterator<Item = (&'b str, ComponentItem)> + 'b,
    {
        // This is heuristic but seems to work
        exports
            .into_iter()
            .find_map(|(name, _)| {
                if name.starts_with("wasi:http/incoming-handler") {
                    Some(Self::HttpProxy)
                } else if name.starts_with("wasi:cli/run") {
                    Some(Self::Command)
                } else {
                    None
                }
            })
            .unwrap_or(Self::Core(func))
    }
}

#[derive(Clone)]
pub struct WasmtimeEngine<T: WasiConfig> {
    engine: wasmtime::Engine,
    cancel: CancellationToken,
    config_type: PhantomData<T>,
}

#[derive(Clone)]
pub struct DefaultConfig {}

impl WasiConfig for DefaultConfig {
    fn new_config() -> Config {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true); // enable component linking
        config
    }
}

pub trait WasiConfig: Clone + Sync + Send + 'static {
    fn new_config() -> Config;
}

impl<T: WasiConfig> Default for WasmtimeEngine<T> {
    fn default() -> Self {
        let mut config = T::new_config();
        config.async_support(true); // must be on
        Self {
            engine: wasmtime::Engine::new(&config)
                .context("failed to create wasmtime engine")
                .unwrap(),
            cancel: CancellationToken::new(),
            config_type: PhantomData,
        }
    }
}

pub struct WasiPreview2Ctx {
    pub(crate) wasi_ctx: wasi_preview2::WasiCtx,
    pub(crate) wasi_http: WasiHttpCtx,
    pub(crate) resource_table: ResourceTable,
}

impl WasiPreview2Ctx {
    pub fn new(ctx: &impl RuntimeContext) -> Result<Self> {
        Ok(Self {
            wasi_ctx: wasi_builder(ctx)?.build(),
            wasi_http: WasiHttpCtx::new(),
            resource_table: ResourceTable::default(),
        })
    }
}

/// This impl is required to use wasmtime_wasi::preview2::WasiView trait.
impl wasi_preview2::WasiView for WasiPreview2Ctx {
    fn table(&mut self) -> &mut ResourceTable {
        &mut self.resource_table
    }

    fn ctx(&mut self) -> &mut wasi_preview2::WasiCtx {
        &mut self.wasi_ctx
    }
}

impl WasiHttpView for WasiPreview2Ctx {
    fn table(&mut self) -> &mut wasmtime::component::ResourceTable {
        &mut self.resource_table
    }

    fn ctx(&mut self) -> &mut wasmtime_wasi_http::WasiHttpCtx {
        &mut self.wasi_http
    }
}

impl<T: WasiConfig> Engine for WasmtimeEngine<T> {
    fn name() -> &'static str {
        "wasmtime"
    }

    fn run_wasi(&self, ctx: &impl RuntimeContext, stdio: Stdio) -> Result<i32> {
        log::info!("setting up wasi");
        let Entrypoint {
            source,
            func,
            arg0: _,
            name: _,
        } = ctx.entrypoint();

        let wasm_bytes = &source.as_bytes()?;
        self.execute(ctx, wasm_bytes, func, stdio).into_error_code()
    }

    fn precompile(&self, layers: &[WasmLayer]) -> Result<Vec<Option<Vec<u8>>>> {
        let mut compiled_layers = Vec::<Option<Vec<u8>>>::with_capacity(layers.len());

        for layer in layers {
            if self.engine.detect_precompiled(&layer.layer).is_some() {
                log::info!("Already precompiled");
                compiled_layers.push(None);
                continue;
            }

            use WasmBinaryType::*;

            let compiled_layer = match WasmBinaryType::from_bytes(&layer.layer) {
                Some(Module) => self.engine.precompile_module(&layer.layer)?,
                Some(Component) => self.engine.precompile_component(&layer.layer)?,
                None => {
                    log::warn!("Unknow WASM binary type");
                    continue;
                }
            };

            compiled_layers.push(Some(compiled_layer));
        }

        Ok(compiled_layers)
    }

    fn can_precompile(&self) -> Option<String> {
        let mut hasher = DefaultHasher::new();
        self.engine
            .precompile_compatibility_hash()
            .hash(&mut hasher);
        Some(hasher.finish().to_string())
    }
}

impl<T> WasmtimeEngine<T>
where
    T: std::clone::Clone + Sync + WasiConfig + Send + 'static,
{
    /// Execute a wasm module.
    ///
    /// This function adds wasi_preview1 to the linker and can be utilized
    /// to execute a wasm module that uses wasi_preview1.
    fn execute_module(
        &self,
        ctx: &impl RuntimeContext,
        module: Module,
        func: &String,
        stdio: Stdio,
    ) -> Result<i32> {
        log::debug!("execute module");

        let ctx = wasi_builder(ctx)?.build_p1();
        let mut store = Store::new(&self.engine, ctx);
        let mut module_linker = wasmtime::Linker::new(&self.engine);

        log::debug!("init linker");
        wasi_preview1::add_to_linker_async(&mut module_linker, |wasi_ctx: &mut WasiP1Ctx| {
            wasi_ctx
        })?;

        wasmtime_wasi::runtime::in_tokio(async move {
            log::info!("instantiating instance");
            let instance: wasmtime::Instance =
                module_linker.instantiate_async(&mut store, &module).await?;

            log::info!("getting start function");
            let start_func = instance
                .get_func(&mut store, func)
                .context("module does not have a WASI start function")?;

            log::debug!("running start function {func:?}");

            stdio.redirect()?;

            start_func
                .call_async(&mut store, &[], &mut [])
                .await
                .into_error_code()
        })
    }

    async fn execute_component_async(
        &self,
        ctx: &impl RuntimeContext,
        component: Component,
        func: String,
        stdio: Stdio,
    ) -> Result<i32> {
        log::info!("instantiating component");

        let target = ComponentTarget::new(
            component.component_type().exports(&self.engine),
            func.as_str(),
        );

        stdio.redirect()?;

        // This is a adapter logic that converts wasip1 `_start` function to wasip2 `run` function.
        let status = match target {
            ComponentTarget::HttpProxy => {
                let mut linker = component::Linker::new(&self.engine);
                wasmtime_wasi_http::add_to_linker_async(&mut linker)?;

                let pre = linker.instantiate_pre(&component)?;
                let instance = ProxyPre::new(pre)?;

                log::info!("starting HTTP server");
                let cancel = self.cancel.clone();
                serve_conn(ctx, instance, cancel).await
            }
            ComponentTarget::Command => {
                let wasi_ctx = WasiPreview2Ctx::new(ctx)?;
                let (mut store, linker) = store_for_context(&self.engine, wasi_ctx)?;

                let command = Command::instantiate_async(&mut store, &component, &linker).await?;

                command
                    .wasi_cli_run()
                    .call_run(&mut store)
                    .await?
                    .map_err(|_| {
                        anyhow::anyhow!(
                            "failed to run component targeting `wasi:cli/command` world"
                        )
                    })
            }
            ComponentTarget::Core(func) => {
                let wasi_ctx = WasiPreview2Ctx::new(ctx)?;
                let (mut store, linker) = store_for_context(&self.engine, wasi_ctx)?;

                let pre = linker.instantiate_pre(&component)?;
                let instance = pre.instantiate_async(&mut store).await?;

                log::info!("getting component exported function {func:?}");
                let start_func = instance.get_func(&mut store, func).context(format!(
                    "component does not have exported function {func:?}"
                ))?;

                log::debug!("running exported function {func:?} {start_func:?}");
                start_func.call_async(&mut store, &[], &mut []).await
            }
        };

        status.into_error_code()
    }

    /// Execute a wasm component.
    ///
    /// This function adds wasi_preview2 to the linker and can be utilized
    /// to execute a wasm component that uses wasi_preview2.
    fn execute_component(
        &self,
        ctx: &impl RuntimeContext,
        component: Component,
        func: String,
        stdio: Stdio,
    ) -> Result<i32> {
        log::debug!("loading wasm component");

        wasmtime_wasi::runtime::in_tokio(async move {
            let mut done = false;
            let exec = self.execute_component_async(ctx, component, func, stdio);

            tokio::pin!(exec);

            loop {
                tokio::select! {
                    status = &mut exec => {
                        return status;
                    }
                    _ = wait_for_signal(), if !done => {
                        self.cancel.cancel();
                        done = true;
                    }
                    sig = wait_for_signal(), if done => {
                        return Ok(128 + sig?)
                    }
                }
            }
        })
    }

    fn execute(
        &self,
        ctx: &impl RuntimeContext,
        wasm_binary: &[u8],
        func: String,
        stdio: Stdio,
    ) -> Result<i32> {
        match WasmBinaryType::from_bytes(wasm_binary) {
            Some(WasmBinaryType::Module) => {
                log::debug!("loading wasm module");
                let module = Module::from_binary(&self.engine, wasm_binary)?;
                self.execute_module(ctx, module, &func, stdio)
            }
            Some(WasmBinaryType::Component) => {
                let component = Component::from_binary(&self.engine, wasm_binary)?;
                self.execute_component(ctx, component, func, stdio)
            }
            None => match &self.engine.detect_precompiled(wasm_binary) {
                Some(Precompiled::Module) => {
                    log::info!("using precompiled module");
                    let module = unsafe { Module::deserialize(&self.engine, wasm_binary) }?;
                    self.execute_module(ctx, module, &func, stdio)
                }
                Some(Precompiled::Component) => {
                    log::info!("using precompiled component");
                    let component = unsafe { Component::deserialize(&self.engine, wasm_binary) }?;
                    self.execute_component(ctx, component, func, stdio)
                }
                None => {
                    bail!("invalid precompiled module")
                }
            },
        }
    }
}

pub(crate) fn envs_from_ctx(ctx: &impl RuntimeContext) -> Vec<(String, String)> {
    ctx.envs()
        .iter()
        .map(|v| {
            let (key, value) = v.split_once('=').unwrap_or((v.as_str(), ""));
            (key.to_string(), value.to_string())
        })
        .collect()
}

fn store_for_context<T: wasi_preview2::WasiView>(
    engine: &wasmtime::Engine,
    ctx: T,
) -> Result<(Store<T>, component::Linker<T>)> {
    let store = Store::new(engine, ctx);

    log::debug!("init linker");
    let mut linker = component::Linker::new(engine);
    wasi_preview2::add_to_linker_async(&mut linker)?;

    Ok((store, linker))
}

fn wasi_builder(ctx: &impl RuntimeContext) -> Result<wasi_preview2::WasiCtxBuilder, anyhow::Error> {
    // TODO: make this more configurable (e.g. allow the user to specify the
    // preopened directories and their permissions)
    // https://github.com/containerd/runwasi/issues/413
    let file_perms = wasi_preview2::FilePerms::all();
    let dir_perms = wasi_preview2::DirPerms::all();
    let envs = envs_from_ctx(ctx);

    let mut builder = wasi_preview2::WasiCtxBuilder::new();
    builder
        .args(ctx.args())
        .envs(&envs)
        .inherit_stdio()
        .inherit_network()
        .allow_tcp(true)
        .allow_udp(true)
        .allow_ip_name_lookup(true)
        .preopened_dir("/", "/", dir_perms, file_perms)?;
    Ok(builder)
}

async fn wait_for_signal() -> Result<i32> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigquit = signal(SignalKind::quit())?;
        let mut sigterm = signal(SignalKind::terminate())?;

        tokio::select! {
            _ = sigquit.recv() => { Ok(libc::SIGINT) }
            _ = sigterm.recv() => { Ok(libc::SIGTERM) }
            _ = tokio::signal::ctrl_c() => { Ok(libc::SIGINT) }
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await;
        Ok(1)
    }
}

pub trait IntoErrorCode {
    fn into_error_code(self) -> Result<i32>;
}

impl IntoErrorCode for Result<i32> {
    fn into_error_code(self) -> Result<i32> {
        self.or_else(|err| match err.downcast_ref::<wasmtime_wasi::I32Exit>() {
            Some(value) => Ok(value.process_exit_code()),
            _ => Err(err),
        })
    }
}

impl IntoErrorCode for Result<()> {
    fn into_error_code(self) -> Result<i32> {
        self.map(|_| 0).into_error_code()
    }
}
