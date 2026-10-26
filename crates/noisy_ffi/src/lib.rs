//! The C ABI for dynamic modules (specification §6.2): the Rust mirror of
//! `include/noisy/module_c.h`, the host function table a plugin receives,
//! and the plugin loader.
//!
//! Correlation: `noisy_ctx*` is opaque module state created by
//! `noisy_module_create`. Host services receive it but resolve their own
//! per-module context through a thread-local the actor sets for the
//! duration of every dispatch — valid because the contract forbids modules
//! from spawning threads, so host calls only happen inside actor dispatch.

pub mod host;
pub mod plugin;

pub use plugin::{LoadError, PluginModule};

use std::os::raw::{c_char, c_int};

/// Engine ABI version; `noisy_abi_version()` must match exactly.
pub const NOISY_ABI_VERSION: u32 = 1;

/// Host function: publish `"{module}.{kind}"` on the bus.
pub type HostEmit =
    unsafe extern "C" fn(*mut NoisyCtx, *const c_char, *const c_char) -> c_int;

/// Host function: invoke a language-native callback token.
pub type HostInvokeCallback =
    unsafe extern "C" fn(*mut NoisyCtx, u64, *const c_char) -> c_int;

/// Host function: connect a slot for the calling module.
pub type HostSubscribe = unsafe extern "C" fn(*mut NoisyCtx, *const c_char) -> c_int;

/// Host function: fire an asynchronous command.
pub type HostCallAsync = unsafe extern "C" fn(
    *mut NoisyCtx,
    *const c_char,
    *const c_char,
    *const c_char,
) -> c_int;

/// Host function: answer the current command (exactly once per command).
pub type HostReply =
    unsafe extern "C" fn(*mut NoisyCtx, c_int, *const c_char) -> c_int;

/// Host function: structured log entry.
pub type HostLog = unsafe extern "C" fn(
    *mut NoisyCtx,
    c_int,
    *const c_char,
    *const c_char,
) -> c_int;

/// Host function: epoch milliseconds.
pub type HostNowMs = unsafe extern "C" fn(*mut NoisyCtx) -> u64;

/// The host function table given to `noisy_module_create` (see
/// `include/noisy/module_c.h`).
#[repr(C)]
pub struct NoisyHost {
    pub emit: HostEmit,
    pub invoke_callback: HostInvokeCallback,
    pub subscribe: HostSubscribe,
    pub call_async: HostCallAsync,
    pub reply: HostReply,
    pub log: HostLog,
    pub now_ms: HostNowMs,
    /// C int flipping 0 → 1 at engine shutdown; read atomically (relaxed).
    pub shutdown_flag: *const c_int,
}

/// Module export: `noisy_abi_version`.
pub type AbiVersionFn = unsafe extern "C" fn() -> *const c_char;
/// Module export: `noisy_module_descriptor`.
pub type DescriptorFn = unsafe extern "C" fn() -> *const c_char;
/// Module export: `noisy_module_create`.
pub type CreateFn =
    unsafe extern "C" fn(host: NoisyHost) -> *mut NoisyCtx;
/// Module export: `noisy_module_command`.
pub type CommandFn = unsafe extern "C" fn(
    *mut NoisyCtx,
    *const c_char,
    *const c_char,
) -> c_int;
/// Module export: `noisy_module_tick` (optional).
pub type TickFn = unsafe extern "C" fn(*mut NoisyCtx, u64) -> c_int;
/// Module export: `noisy_module_event`.
pub type EventFn = unsafe extern "C" fn(
    *mut NoisyCtx,
    *const c_char,
    *const c_char,
) -> c_int;
/// Module export: `noisy_module_reply`.
pub type ReplyFn = unsafe extern "C" fn(
    *mut NoisyCtx,
    u64,
    *const c_char,
) -> c_int;
/// Module export: `noisy_module_stop`.
pub type StopFn = unsafe extern "C" fn(*mut NoisyCtx, u64) -> c_int;
/// Module export: `noisy_module_destroy`.
pub type DestroyFn = unsafe extern "C" fn(*mut NoisyCtx);

/// Opaque `noisy_ctx*` — module state created by `noisy_module_create`;
/// the engine only stores and passes it back.
pub struct NoisyCtx {
    _private: [u8; 0],
}

/// Reads a C string pointer into owned UTF-8; `None` for null.
pub(crate) fn cstr(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees the pointer is a valid C string for
    // the duration of the call.
    Some(
        unsafe { std::ffi::CStr::from_ptr(ptr) }
            .to_string_lossy()
            .to_string(),
    )
}
