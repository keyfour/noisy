//! Tokio task coordination (specification §3.1 "Scheduler" and §3.2).
//!
//! The engine owns one root [`CancellationToken`]; the registry derives a
//! child token per module, so cancelling the root propagates to every
//! module simultaneously and no module can block shutdown indefinitely.

use std::time::Duration;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Creates a fresh root cancellation token for an engine/instance.
pub fn new_root_token() -> CancellationToken {
    CancellationToken::new()
}

/// A spawned module task, tracked by the registry for orderly shutdown.
#[derive(Debug)]
pub struct ModuleTask {
    name: String,
    join: Option<JoinHandle<()>>,
}

/// Outcome of joining a module task with a timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownStatus {
    /// Task completed before the timeout.
    Completed,
    /// The task did not finish within the timeout; the handle was dropped,
    /// detaching the task. Any leaked work stays isolated to that module
    /// until process exit (specification §9 keeps this out of the public
    /// contract).
    TimedOut,
}

impl ModuleTask {
    /// Wraps a spawned module task.
    pub fn new(name: impl Into<String>, join: JoinHandle<()>) -> Self {
        Self {
            name: name.into(),
            join: Some(join),
        }
    }

    /// The module this task belongs to.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Awaits the task with a per-module timeout; on timeout the handle is
    /// dropped (detached) and the registry keeps going (specification
    /// §3.1: no module can block shutdown indefinitely).
    pub async fn join_with_timeout(mut self, timeout: Duration) -> ShutdownStatus {
        match self.join.take() {
            None => ShutdownStatus::Completed,
            Some(join) => match tokio::time::timeout(timeout, join).await {
                Ok(_) => ShutdownStatus::Completed,
                Err(_) => ShutdownStatus::TimedOut,
            },
        }
    }
}
