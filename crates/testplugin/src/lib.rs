//! The ABI conformance test module (`probe`): a third-party Rust module
//! compiled as a `cdylib` exporting the C ABI — the plugin path exists
//! before any C++ SDK does (implementation plan §Phase 3).
//!
//! It mirrors `include/noisy/module_c.h` locally, exactly the way a
//! hand-written third-party module would. Contract rules respected: no
//! locks, no threads; all data crosses as JSON strings; host calls happen
//! only inside dispatched callbacks.
#![allow(clippy::missing_safety_doc)]

use serde_json::{json, Value};
use std::ffi::{c_char, CString};
use std::os::raw::{c_int, c_void};
use std::sync::OnceLock;

/// Must mirror `include/noisy/module_c.h`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NoisyHost {
    pub emit: unsafe extern "C" fn(*mut NoisyCtx, *const c_char, *const c_char) -> c_int,
    pub invoke_callback:
        unsafe extern "C" fn(*mut NoisyCtx, u64, *const c_char) -> c_int,
    pub subscribe: unsafe extern "C" fn(*mut NoisyCtx, *const c_char) -> c_int,
    pub call_async: unsafe extern "C" fn(
        *mut NoisyCtx,
        *const c_char,
        *const c_char,
        *const c_char,
    ) -> c_int,
    pub reply:
        unsafe extern "C" fn(*mut NoisyCtx, c_int, *const c_char) -> c_int,
    pub log: unsafe extern "C" fn(
        *mut NoisyCtx,
        c_int,
        *const c_char,
        *const c_char,
    ) -> c_int,
    pub now_ms: unsafe extern "C" fn(*mut NoisyCtx) -> u64,
    pub shutdown_flag: *const c_int,
}

/// Opaque module state; the host passes the returned pointer back into
/// every call. Internally the module casts it to its own [`State`].
pub struct NoisyCtx {
    _private: [u8; 0],
}

const LOG_INFO: c_int = 1;

const DESCRIPTOR_JSON: &str = r#"{
  "name": "probe",
  "version": "0.1.0",
  "signals": ["pinged", "call_answered"],
  "methods": [
    { "name": "echo", "params": ["text"] },
    { "name": "now" },
    { "name": "tick_count" },
    { "name": "last_call" },
    { "name": "call_self" },
    { "name": "events" }
  ],
  "depends": []
}"#;

struct State {
    host: NoisyHost,
    events_received: u64,
    ticks: u64,
    last_call: Option<(u64, Value)>,
}

impl State {
    fn from_ctx<'a>(ctx: *mut NoisyCtx) -> &'a mut State {
        // SAFETY: the pointer was created by `Box::into_raw` in `create`.
        unsafe { &mut *(ctx as *mut State) }
    }

    fn reply(&self, ctx: *mut NoisyCtx, value: Value) {
        let reply_json = CString::new(value.to_string()).unwrap();
        // SAFETY: contract call; the string lives through the call.
        unsafe { (self.host.reply)(ctx, 0, reply_json.as_ptr()) };
    }

    fn reply_error(&self, ctx: *mut NoisyCtx, message: &str) {
        let reply_json = CString::new(message).unwrap();
        unsafe { (self.host.reply)(ctx, 1, reply_json.as_ptr()) };
    }

    fn log(&self, ctx: *mut NoisyCtx, message: &str) {
        let msg = CString::new(message).unwrap();
        unsafe {
            (self.host.log)(ctx, LOG_INFO, msg.as_ptr(), std::ptr::null());
        }
    }

    fn emit(&self, ctx: *mut NoisyCtx, kind: &str, payload: Value) {
        let kind = CString::new(kind).unwrap();
        let payload = CString::new(payload.to_string()).unwrap();
        // SAFETY: contract call; the host copies strings, not lifetime.
        unsafe { (self.host.emit)(ctx, kind.as_ptr(), payload.as_ptr()) };
    }
}

/// Leak-once C string for the static ABI version.
fn abi_version_ptr() -> *const c_char {
    static ABI: OnceLock<CString> = OnceLock::new();
    ABI.get_or_init(|| CString::new("1").unwrap()).as_ptr()
}

/// Leak-once C string for the static descriptor JSON.
fn descriptor_ptr() -> *const c_char {
    static DESCRIPTOR: OnceLock<CString> = OnceLock::new();
    DESCRIPTOR
        .get_or_init(|| CString::new(DESCRIPTOR_JSON).unwrap())
        .as_ptr()
}

// ───────────────────────────── module exports ──────────────────────────────

#[no_mangle]
pub extern "C" fn noisy_abi_version() -> *const c_char {
    abi_version_ptr()
}

#[no_mangle]
pub extern "C" fn noisy_module_descriptor() -> *const c_char {
    descriptor_ptr()
}

#[no_mangle]
pub extern "C" fn noisy_module_create(host: NoisyHost) -> *mut NoisyCtx {
    let state = Box::new(State {
        host,
        events_received: 0,
        ticks: 0,
        last_call: None,
    });
    let ctx = Box::into_raw(state) as *mut NoisyCtx;

    // Init-time host calls resolve through the ctx the engine just received.
    let state = State::from_ctx(ctx);
    let pattern = CString::new("app.").unwrap();
    unsafe { (state.host.subscribe)(ctx, pattern.as_ptr()) };

    state.log(ctx, "probe initialized; subscribed to app. events");
    ctx
}

#[no_mangle]
pub unsafe extern "C" fn noisy_module_command(
    ctx: *mut NoisyCtx,
    method: *const c_char,
    params_json: *const c_char,
) -> c_int {
    let state = State::from_ctx(ctx);
    let method = unsafe { std::ffi::CStr::from_ptr(method).to_string_lossy() };
    let params: Value = unsafe { std::ffi::CStr::from_ptr(params_json) }
        .to_string_lossy()
        .parse()
        .unwrap_or(Value::Null);

    match method.as_ref() {
        "echo" => {
            let text = params["text"].as_str().unwrap_or_default();
            let now = unsafe { (state.host.now_ms)(ctx) };
            state.reply(ctx, json!({ "echo": text, "at": now }));
        }
        "now" => {
            let now = unsafe { (state.host.now_ms)(ctx) };
            state.reply(ctx, json!({ "now_ms": now }));
        }
        "tick_count" => state.reply(ctx, json!({ "ticks": state.ticks })),
        "last_call" => {
            let last = match &state.last_call {
                Some((id, value)) => json!({ "id": id, "value": value }),
                None => Value::Null,
            };
            state.reply(ctx, last);
        }
        "events" => state.reply(ctx, json!({ "events": state.events_received })),
        // Exercises async calls and the reply path end to end:
        // call_async → the reply arrives later as noisy_module_reply.
        "call_self" => {
            let module = CString::new("probe").unwrap();
            let target = CString::new("echo").unwrap();
            let params = CString::new(json!({ "text": "self" }).to_string()).unwrap();
            let call_id = unsafe {
                (state.host.call_async)(ctx, module.as_ptr(), target.as_ptr(), params.as_ptr())
            };
            state.reply(ctx, json!({ "queued": true, "id": call_id }));
        }
        other => state.reply_error(ctx, &format!("no such method: {other}")),
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn noisy_module_tick(ctx: *mut NoisyCtx, _now_ms: u64) -> c_int {
    State::from_ctx(ctx).ticks += 1;
    0
}

#[no_mangle]
pub unsafe extern "C" fn noisy_module_event(
    ctx: *mut NoisyCtx,
    name: *const c_char,
    payload_json: *const c_char,
) -> c_int {
    let state = State::from_ctx(ctx);
    let name = unsafe { std::ffi::CStr::from_ptr(name).to_string_lossy() };
    // The module does not interpret payload content; it relays events.
    let _payload: Value = unsafe { std::ffi::CStr::from_ptr(payload_json) }
        .to_string_lossy()
        .parse()
        .unwrap_or(Value::Null);

    state.events_received += 1;
    let received = name.to_string();
    state.log(
        ctx,
        &format!("probe: received {received} (events={})", state.events_received),
    );
    state.emit(
        ctx,
        "pinged",
        json!({ "relay": received, "seq": state.events_received }),
    );
    state.log(
        ctx,
        &format!("probe: pinged emitted for {name} (events={})", state.events_received),
    );
    0
}

#[no_mangle]
pub unsafe extern "C" fn noisy_module_reply(
    ctx: *mut NoisyCtx,
    call_token: u64,
    reply_json: *const c_char,
) -> c_int {
    let state = State::from_ctx(ctx);
    let reply: Value = unsafe { std::ffi::CStr::from_ptr(reply_json) }
        .to_string_lossy()
        .parse()
        .unwrap_or(Value::Null);

    match reply.get("error") {
        Some(message) => {
            let message = message.as_str().unwrap_or("unknown error");
            state.reply(ctx, json!({ "failed": message }));
            state.reply_error(ctx, message);
        }
        None => {
            state.log(ctx, &format!("probe: async call {call_token} answered"));
            state.emit(ctx, "call_answered", json!({ "id": call_token }));
            state.last_call = Some((call_token, reply));
        }
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn noisy_module_stop(_ctx: *mut NoisyCtx, _timeout_ms: u64) -> c_int {
    // Nothing to release while running: the module keeps no other threads.
    0
}

#[no_mangle]
pub unsafe extern "C" fn noisy_module_destroy(ctx: *mut NoisyCtx) {
    // SAFETY: one `Box::into_raw` in `create`, destroyed exactly once.
    unsafe { drop(Box::from_raw(ctx as *mut c_void as *mut State)) };
}

