//! noisy Core — the frozen infrastructure of the noisy framework.
//!
//! Provides the event bus, the module descriptor model, the module trait,
//! the module registry with lifecycle management, and the Tokio task
//! coordination helpers. See `docs/SPECIFICATION.md` §3.
//!
//! Phase 1 scope: Rust-only. The Lua runtime and the dynamic C ABI plugin
//! loader (§3.1 / §6.2) arrive in later phases and layer on top of these
//! primitives unchanged.

pub mod bus;
pub mod callback;
pub mod error;
pub mod lua_runtime;
pub mod module;
pub mod registry;
pub mod scheduler;

pub use bus::{core_events, EventBus, EventStream, NoisyEvent};
pub use callback::{CallbackEnvelope, CallbackHub, CallbackToken};
pub use error::ModuleError;
pub use lua_runtime::{embedded_stdlib, LuaDeps, LuaHandle, LuaJob, LuaRuntime, LUA_CALL_TIMEOUT};
pub use module::{Command, HostCtx, LogLevel, MethodCall, MethodDecl, ModuleContext, ModuleDescriptor, NoisyModule};
pub use registry::{
    ModuleRegistry, RegistryError, ShutdownReport, DEFAULT_BUS_CAPACITY, DEFAULT_COMMAND_CAPACITY, ENGINE_CALLER,
};
pub use scheduler::{ModuleTask, ShutdownStatus};
