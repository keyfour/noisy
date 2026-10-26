//! The engine side of the host function table: every service a dynamic
//! module may call from inside its dispatched callbacks, wired to the
//! engine's [`HostCtx`](noisy_core::HostCtx) services.

use crate::{cstr, NoisyCtx, NoisyHost};
use noisy_core::{HostCtx, LogLevel, MethodCall, ModuleError, NoisyEvent};
use serde_json::Value;
use std::os::raw::{c_char, c_int};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicI32, AtomicPtr, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// Engine-side state of one loaded dynamic module, identified by a raw
/// stable pointer stored in a thread-local for the duration of each actor
/// dispatch (see crate docs). All fields are shared because host services
/// may run on any Tokio blocking thread while a command is in flight.
pub struct HostCell {
    /// The module-scoped host context (carries the module's identity).
    pub host: HostCtx,
    /// 0 while running, 1 once the actor observes shutdown.
    pub shutdown_flag: AtomicI32,
    /// Events forward into the actor loop through this channel.
    pub events_tx: mpsc::UnboundedSender<NoisyEvent>,
    /// The command currently being served; `reply()` takes it.
    pub pending: Mutex<Option<MethodCall>>,
    /// The module's `noisy_ctx*`, set right after `create`.
    pub ctx: AtomicPtr<NoisyCtx>,
}

impl HostCell {
    pub fn new(host: HostCtx, events_tx: mpsc::UnboundedSender<NoisyEvent>) -> Self {
        Self {
            host,
            shutdown_flag: AtomicI32::new(0),
            events_tx,
            pending: Mutex::new(None),
            ctx: AtomicPtr::new(null_mut()),
        }
    }

    /// The module context the engine passes back into every module call.
    pub fn ctx(&self) -> *mut NoisyCtx {
        self.ctx.load(Ordering::Acquire)
    }

    /// Answers the command currently in flight (`reply()` — exactly once
    /// per command, by contract). `status == 0` means success with
    /// `reply_json` as the payload; otherwise `reply_json` is the error
    /// message. Returns -1 when nothing is pending (a plugin bug).
    pub fn take_pending_call(&self, status: c_int, reply_json: *const c_char) -> c_int {
        let reply_text = cstr(reply_json).unwrap_or_default();
        let module = self.host.module_name().to_string();
        let mut pending = self.pending.lock().unwrap();
        match pending.take() {
            Some(call) => {
                let result = if status == 0 {
                    match serde_json::from_str::<Value>(&reply_text) {
                        Ok(value) => Ok(value),
                        Err(_) => Err(ModuleError::failed(
                            module,
                            format!("reply is not valid JSON: {reply_text}"),
                        )),
                    }
                } else {
                    Err(ModuleError::failed(module, reply_text))
                };
                call.reply(result);
                0
            }
            None => -1,
        }
    }
}

thread_local! {
    /// The cell whose module is currently being dispatched on this thread.
    static CURRENT: std::cell::Cell<*mut HostCell> =
        const { std::cell::Cell::new(std::ptr::null_mut()) };
}

/// Sets the cell for the duration of a single FFI dispatch on this thread
/// and runs `call` on a blocking thread (module handlers may run long).
pub(crate) async fn dispatch<T, F>(cell: &Arc<HostCell>, call: F) -> T
where
    F: FnOnce(&HostCell) -> T + Send + 'static,
    T: Send + 'static,
{
    let queued_cell = cell.clone();
    tokio::task::spawn_blocking(move || {
        CURRENT.with(|slot| slot.set(Arc::as_ptr(&queued_cell) as *mut HostCell));
        let out = call(&queued_cell);
        CURRENT.with(|slot| slot.set(std::ptr::null_mut()));
        out
    })
    .await
    .expect("actor dispatch panicked")
}

/// The cell for the host call currently in flight on this thread.
fn current_cell() -> Option<&'static HostCell> {
    let ptr = CURRENT.with(|slot| slot.get());
    if ptr.is_null() {
        return None;
    }
    // SAFETY: set by `dispatch` at the moment the host call happens; the
    // actor keeps the `Arc<HostCell>` alive for the whole dispatch.
    Some(unsafe { &*ptr })
}

fn host_of(_ctx: *mut NoisyCtx) -> Option<&'static HostCell> {
    // The ctx argument is opaque module state; correlation is thread-local.
    current_cell()
}

fn json_arg(ptr: *const c_char) -> Option<Value> {
    serde_json::from_str(&cstr(ptr)?).ok()
}

// ───────────────────────────── host services ──────────────────────────────

unsafe extern "C" fn host_emit(
    _ctx: *mut NoisyCtx,
    kind: *const c_char,
    payload_json: *const c_char,
) -> c_int {
    let Some(cell) = host_of(_ctx) else { return -1 };
    let Some(kind) = cstr(kind) else { return -1 };
    let Some(payload) = json_arg(payload_json) else { return -1 };
    if cell.host.emit(&kind, payload) {
        1
    } else {
        0
    }
}

unsafe extern "C" fn host_invoke_callback(
    _ctx: *mut NoisyCtx,
    token: u64,
    payload_json: *const c_char,
) -> c_int {
    let Some(cell) = host_of(_ctx) else { return -1 };
    let Some(payload) = json_arg(payload_json) else { return -1 };
    if cell.host.invoke_callback(token, payload) {
        0
    } else {
        -1
    }
}

unsafe extern "C" fn host_subscribe(
    _ctx: *mut NoisyCtx,
    pattern: *const c_char,
) -> c_int {
    let Some(cell) = host_of(_ctx) else { return -1 };
    let Some(pattern) = cstr(pattern) else { return -1 };
    let stream = cell.host.on(pattern);
    let tx = cell.events_tx.clone();
    tokio::spawn(async move {
        let mut stream = stream;
        while let Some(event) = stream.next().await {
            if tx.send(event).is_err() {
                break;
            }
        }
    });
    0
}

unsafe extern "C" fn host_call_async(
    _ctx: *mut NoisyCtx,
    module: *const c_char,
    method: *const c_char,
    params_json: *const c_char,
) -> c_int {
    let Some(cell) = host_of(_ctx) else { return -1 };
    let (Some(module), Some(method)) = (cstr(module), cstr(method)) else {
        return -1;
    };
    let Some(params) = json_arg(params_json) else { return -1 };
    match cell.host.call_async(&module, &method, params) {
        Ok(call_id) => call_id as c_int,

        Err(_) => -1,
    }
}

unsafe extern "C" fn host_reply(
    _ctx: *mut NoisyCtx,
    status: c_int,
    reply_json: *const c_char,
) -> c_int {
    let Some(cell) = host_of(_ctx) else { return -1 };
    cell.take_pending_call(status, reply_json)
}

unsafe extern "C" fn host_log(
    _ctx: *mut NoisyCtx,
    level: c_int,
    msg: *const c_char,
    fields_json: *const c_char,
) -> c_int {
    let Some(cell) = host_of(_ctx) else { return -1 };
    let Some(msg) = cstr(msg) else { return -1 };
    let level = match level {
        0 => LogLevel::Debug,
        1 => LogLevel::Info,
        2 => LogLevel::Warn,
        _ => LogLevel::Error,
    };
    let fields = json_arg(fields_json);
    cell.host.log(level, &msg, fields);
    0
}

unsafe extern "C" fn host_now_ms(_ctx: *mut NoisyCtx) -> u64 {
    let Some(cell) = host_of(_ctx) else { return 0 };
    cell.host.now_ms()
}

/// Builds the function table handed to `noisy_module_create`.
pub(crate) fn build_noisy_host(cell: &HostCell) -> NoisyHost {
    NoisyHost {
        emit: host_emit,
        invoke_callback: host_invoke_callback,
        subscribe: host_subscribe,
        call_async: host_call_async,
        reply: host_reply,
        log: host_log,
        now_ms: host_now_ms,
        // The flag lives inside `HostCell`, whose `Arc` outlives the plugin.
        shutdown_flag: std::ptr::addr_of!(cell.shutdown_flag) as *const c_int,
    }
}
