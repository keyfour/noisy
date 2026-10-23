//! Error types shared across the core (specification §3.1, §6.1).

use thiserror::Error;

/// Errors returned by host services and carried through replies.
#[derive(Debug, Clone, Error)]
pub enum ModuleError {
    /// The addressed method does not exist on the target module.
    #[error("unknown method `{method}` on module `{module}`")]
    UnknownMethod { module: String, method: String },

    /// The addressed module is not loaded (or stopped accepting commands).
    #[error("module `{0}` is not available")]
    ModuleUnavailable(String),

    /// The call did not complete within its timeout.
    #[error("call timed out after {timeout_ms} ms")]
    Timeout { timeout_ms: u64 },

    /// The synchronous call would re-enter a module that is already waiting
    /// in this caller's synchronous call chain — rejected to prevent
    /// deadlocks (specification §6.1); use `call_async` instead.
    #[error("re-entrant call rejected: `{from}` would re-enter `{to}` through an in-flight call chain")]
    Reentrant { from: String, to: String },

    /// The engine is shutting down; calls and pending commands fail fast.
    #[error("engine is shutting down")]
    ShuttingDown,

    /// The module rejected the call with its own message.
    #[error("module `{module}` failed: {message}")]
    Failed { module: String, message: String },

    /// A payload could not be produced or decoded.
    #[error("invalid payload: {0}")]
    Payload(String),

    /// A module descriptor is invalid.
    #[error("invalid descriptor: {0}")]
    Descriptor(String),
}

impl ModuleError {
    /// Builds [`ModuleError::UnknownMethod`].
    pub fn unknown_method(module: impl Into<String>, method: impl Into<String>) -> Self {
        Self::UnknownMethod {
            module: module.into(),
            method: method.into(),
        }
    }

    /// Builds [`ModuleError::Timeout`] from a millisecond value.
    pub fn timeout(timeout_ms: u64) -> Self {
        Self::Timeout { timeout_ms }
    }

    /// Builds [`ModuleError::ModuleUnavailable`].
    pub fn unavailable(module: impl Into<String>) -> Self {
        Self::ModuleUnavailable(module.into())
    }

    /// Builds [`ModuleError::Failed`].
    pub fn failed(module: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Failed {
            module: module.into(),
            message: message.into(),
        }
    }

    /// Builds [`ModuleError::Descriptor`].
    pub fn descriptor(message: impl Into<String>) -> Self {
        Self::Descriptor(message.into())
    }
}
