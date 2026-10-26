//! The plugin loader and the per-dynamic-module actor (specification §6.2,
//! §8): libloading resolves the versioned exports, the descriptor is
//! validated exactly like an in-tree module's, and the loaded module is
//! wrapped into a [`PluginModule`] — an actor delivering
//! `command / event / tick / reply` one at a time.

use crate::host::{build_noisy_host, dispatch, HostCell};
use crate::{
    AbiVersionFn, CommandFn, CreateFn, DestroyFn, DescriptorFn, EventFn, NoisyCtx, ReplyFn,
    StopFn, TickFn, NOISY_ABI_VERSION,
};
use noisy_core::module::{
    Command, MethodCall, ModuleContext, ModuleDescriptor, NoisyModule,
};
use noisy_core::{ModuleError, NoisyEvent};
use std::ffi::CString;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// All resolved exports together with the library keeping them alive.
pub(crate) struct LoadedExports {
    pub create: CreateFn,
    pub command: CommandFn,
    pub event: EventFn,
    pub reply: ReplyFn,
    pub stop: StopFn,
    pub destroy: DestroyFn,
    /// Optional heartbeat (`noisy_module_tick`).
    pub tick: Option<TickFn>,
    _library: libloading::Library,
}

/// A loaded dynamic module, runnable through the engine's module contract.
pub struct PluginModule {
    name: String,
    descriptor: ModuleDescriptor,
    exports: Arc<LoadedExports>,
    /// 0 disables the optional tick.
    pub tick_interval_ms: u64,
}

/// The budget given to `noisy_module_stop` by the actor.
pub(crate) const STOP_TIMEOUT_MS: u64 = 2000;

/// Errors while loading a dynamic module — every message is readable.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// The `.so` could not be opened or is not a valid plugin.
    #[error("cannot load `{0}`: {1}")]
    Open(String, String),
    /// A required export is missing.
    #[error("`{0}`: missing export `{1}`")]
    MissingExport(String, &'static str),
    /// The ABI version does not match the engine exactly.
    #[error("ABI version mismatch in `{0}`: plugin is {1}, engine is {2}")]
    AbiMismatch(String, String, String),
    /// The descriptor JSON could not be parsed or validated.
    #[error("invalid descriptor in `{0}`: {1}")]
    Descriptor(String, String),
}

impl PluginModule {
    /// Loads a `.so`, validates the ABI version and the descriptor, and
    /// returns a module ready for [`noisy_core::registry::ModuleRegistry`]
    /// loading. The library stays loaded as long as the module (and its
    /// actor) does.
    pub fn load(path: &Path, tick_interval_ms: u64) -> Result<Self, LoadError> {
        let path_str = path.display().to_string();
        // SAFETY: opening a shared object; every symbol is typed against
        // the contract in `include/noisy/module_c.h` and the `Library` is
        // kept alive alongside the resolved function pointers.
        let library = unsafe { libloading::Library::new(path) }
            .map_err(|e| LoadError::Open(path_str.clone(), e.to_string()))?;

        let abi_version: AbiVersionFn = resolve(&library, &path_str, "noisy_abi_version", false)?
            .expect("required export");
        let descriptor_ptr: DescriptorFn =
            resolve(&library, &path_str, "noisy_module_descriptor", false)?
                .expect("required export");

        // SAFETY: no-argument exports typed against the C contract.
        let abi = unsafe { crate::cstr(abi_version()) }.unwrap_or_default();
        if abi != NOISY_ABI_VERSION.to_string() {
            return Err(LoadError::AbiMismatch(
                path_str,
                if abi.is_empty() {
                    "<unreadable>".to_string()
                } else {
                    abi
                },
                NOISY_ABI_VERSION.to_string(),
            ));
        }
        let descriptor_raw = unsafe { crate::cstr(descriptor_ptr()) }.unwrap_or_default();
        let descriptor_value: serde_json::Value = serde_json::from_str(&descriptor_raw).map_err(
            |e| LoadError::Descriptor(path_str.clone(), format!("descriptor is not valid JSON: {e}")),
        )?;
        let descriptor: ModuleDescriptor = serde_json::from_value(descriptor_value)
            .map_err(|e| LoadError::Descriptor(path_str.clone(), format!("descriptor shape: {e}")))?;
        descriptor
            .validate()
            .map_err(|e| LoadError::Descriptor(path_str.clone(), e.to_string()))?;

        Ok(Self {
            exports: Arc::new(LoadedExports {
                create: resolve(&library, &path_str, "noisy_module_create", false)?
                    .expect("required export"),
                command: resolve(&library, &path_str, "noisy_module_command", false)?
                    .expect("required export"),
                event: resolve(&library, &path_str, "noisy_module_event", false)?
                    .expect("required export"),
                reply: resolve(&library, &path_str, "noisy_module_reply", false)?
                    .expect("required export"),
                stop: resolve(&library, &path_str, "noisy_module_stop", false)?
                    .expect("required export"),
                destroy: resolve(&library, &path_str, "noisy_module_destroy", false)?
                    .expect("required export"),
                tick: resolve(&library, &path_str, "noisy_module_tick", true)?,
                _library: library,
            }),
            name: descriptor.name.clone(),
            descriptor,
            tick_interval_ms,
        })
    }
}

/// Resolves one symbol. An `optional` symbol resolves to `None` instead of
/// an error; required symbols are `expect`ed by the loader.
/// SAFETY: symbols are typed against the C contract and the `Library`
/// outlives each resolved pointer.
fn resolve<T: Copy>(
    library: &libloading::Library,
    path: &str,
    name: &'static str,
    optional: bool,
) -> Result<Option<T>, LoadError> {
    let symbol: Result<T, libloading::Error> = unsafe { library.get(name.as_bytes()) }.map(|s| *s);
    match symbol {
        Ok(value) => Ok(Some(value)),
        Err(_) if optional => Ok(None),
        Err(_) => Err(LoadError::MissingExport(path.to_string(), name)),
    }
}

// ───────────────────────────── the actor ──────────────────────────────────

impl NoisyModule for PluginModule {
    fn descriptor(&self) -> ModuleDescriptor {
        self.descriptor.clone()
    }

    fn start(self: Box<Self>, mut ctx: ModuleContext) -> JoinHandle<()> {
        // Destructure before moving into the task.
        let PluginModule {
            name,
            exports,
            tick_interval_ms,
            descriptor: _,
        } = *self;
        let exports: Arc<LoadedExports> = exports;

        tokio::spawn(async move {
            let (events_tx, mut events_rx) = mpsc::unbounded_channel();
            let cell = Arc::new(HostCell::new(ctx.host.clone(), events_tx));

            // `noisy_module_create` runs inside a dispatch: init-time host
            // calls (subscribe, log, ...) must resolve the cell.
            let created = create_plugin(&exports, &cell).await;
            if created == 0 {
                tracing::error!(
                    module = %name,
                    "noisy_module_create returned null; the plugin will not serve"
                );
                return;
            }
            cell.ctx.store(created as *mut NoisyCtx, Ordering::Release);

            // The optional heartbeat (Tokio's interval fires the first
            // tick immediately, then at the interval). The tick function
            // is copy-captured separately so the per-loop wait future does
            // not hold a shared borrow.
            let tick_fn = exports.tick;
            let mut tick_wait = (tick_interval_ms > 0)
                .then(|| tokio::time::interval(Duration::from_millis(tick_interval_ms)));

            // Pinned once: the shutdown and command arms borrow `ctx`, so
            // the cancellation future must not.
            let shutdown_token = ctx.shutdown.clone();
            let shutdown_done = shutdown_token.cancelled();
            tokio::pin!(shutdown_done);

            loop {
                let tick_due = async {
                    match &mut tick_wait {
                        Some(interval) => interval.tick().await,
                        None => std::future::pending::<tokio::time::Instant>().await,
                    }
                };

                tokio::select! {
                    biased;

                    _ = &mut shutdown_done => break,

                    // Events ahead of commands: a busy command stream must
                    // not starve a subscribed module's event delivery.
                    event = events_rx.recv() => match event {
                        Some(event) => serve_event(&exports, &cell, &event).await,
                        None => break,
                    },

                    command = ctx.next_command() => match command {
                        Some(Command::MethodCall(call)) => {
                            serve_command(&name, &exports, &cell, call).await;
                        }
                        Some(Command::AsyncReply { call_id, result }) => {
                            let reply_text = match &result {
                                Ok(value) => value.to_string(),
                                Err(err) => {
                                    serde_json::json!({ "error": err.to_string() }).to_string()
                                }
                            };
                            call_reply(&exports, &cell, call_id, &reply_text).await;
                        }
                        // Command stream closed and drained after shutdown.
                        None => break,
                    },

                    _ = tick_due => {
                        if let Some(tick) = tick_fn {
                            let now = cell.host.now_ms();
                            dispatch(&cell, move |cell| {
                                unsafe { (tick)(cell.ctx(), now) }
                            })
                            .await;
                        }
                    },
                }
            }

            // Cleanup (lifecycle §3.2): release module resources, then free
            // the module state itself.
            cell.shutdown_flag.store(1, Ordering::Release);
            ctx.drain().await;
            let exports2 = exports.clone();
            dispatch(&cell, move |cell| {
                unsafe { (exports2.stop)(cell.ctx(), STOP_TIMEOUT_MS) }
            })
            .await;
            call_destroy(&exports, &cell).await;
        })
    }
}

/// Runs `noisy_module_create` and returns the module context pointer as a
/// `usize` (0 = null — the plugin is unusable).
async fn create_plugin(exports: &Arc<LoadedExports>, cell: &Arc<HostCell>) -> usize {
    let exports = exports.clone();
    dispatch(cell, move |cell| {
        // SAFETY: contract call; host services are wired to this cell.
        unsafe { (exports.create)(build_noisy_host(cell)) as usize }
    })
    .await
}

async fn serve_command(
    name: &str,
    exports: &Arc<LoadedExports>,
    cell: &Arc<HostCell>,
    call: MethodCall,
) {
    let method = call.method().to_string();
    let params_json = call.params().to_string();
    *cell.pending.lock().unwrap() = Some(call);

    let method_name = method.clone();
    let exports = exports.clone();
    let rc = dispatch(cell, move |cell| unsafe {
        let mut rc: std::os::raw::c_int = -1;
        if let Ok(method_c) = CString::new(method.as_str()) {
            if let Ok(params) = CString::new(params_json.as_str()) {
                // SAFETY: contract call; `reply()` lands in this cell.
                rc = (exports.command)(cell.ctx(), method_c.as_ptr(), params.as_ptr());
            }
        }
        rc
    })
    .await;

    // A command must be answered exactly once; a handler that returned
    // without replying failed like any dropped MethodCall.
    let unreplied = cell.pending.lock().unwrap().take();
    if let Some(call) = unreplied {
        call.reply(Err(ModuleError::failed(
            name,
            format!("command `{method_name}` returned {rc} without calling reply"),
        )));
    }
}

async fn serve_event(exports: &Arc<LoadedExports>, cell: &Arc<HostCell>, event: &NoisyEvent) {
    let (Ok(name), Ok(payload)) = (
        CString::new(event.name.as_str()),
        CString::new(event.payload.to_string()),
    ) else {
        return; // interior NUL — cannot cross the C boundary
    };
    let exports = exports.clone();
    dispatch(cell, move |cell| {
        // SAFETY: contract call.
        unsafe { (exports.event)(cell.ctx(), name.as_ptr(), payload.as_ptr()) }
    })
    .await;
}

async fn call_reply(exports: &Arc<LoadedExports>, cell: &Arc<HostCell>, call_id: u64, reply: &str) {
    let Ok(reply) = CString::new(reply) else { return };
    let exports = exports.clone();
    dispatch(cell, move |cell| {
        // SAFETY: contract call.
        unsafe { (exports.reply)(cell.ctx(), call_id, reply.as_ptr()) }
    })
    .await;
}

async fn call_destroy(exports: &Arc<LoadedExports>, cell: &Arc<HostCell>) {
    let exports = exports.clone();
    dispatch(cell, move |cell| {
        // SAFETY: contract call; after this the engine drops its cell.
        unsafe { (exports.destroy)(cell.ctx()) }
    })
    .await;
}
