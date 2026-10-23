//! The module registry (specification §3.1, §3.2): module ownership,
//! lifecycle events, cross-module command routing with deadlock
//! rejection, and ordered shutdown.

use crate::bus::{core_events, EventBus, NoisyEvent};
use crate::callback::CallbackHub;
use crate::error::ModuleError;
use crate::module::{
    Command, HostCtx, MethodCall, ModuleContext, ModuleDescriptor, NoisyModule,
};
use crate::scheduler::{ModuleTask, ShutdownStatus};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// Default per-module command channel capacity (specification §3.1:
/// capacity is configurable; the configurable knob arrives with the
/// engine's config in a later phase).
pub const DEFAULT_COMMAND_CAPACITY: usize = 32;

/// Default bus capacity.
pub const DEFAULT_BUS_CAPACITY: usize = 1024;

/// Caller identity used for calls originating outside any module (engine
/// internals, tests). A pseudo-module; never tracked in call chains.
pub const ENGINE_CALLER: &str = "noisy";

/// Default timeout for `call_async` attempts.
pub const ASYNC_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Errors while loading a module.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// A module with this name is already loaded.
    #[error("module `{0}` is already loaded")]
    Duplicate(String),

    /// A depends entry is not in the loaded set.
    #[error("module `{module}` depends on unloaded module `{depends_on}`")]
    MissingDependency {
        module: String,
        depends_on: String,
    },

    /// The descriptor failed validation.
    #[error(transparent)]
    Descriptor(#[from] ModuleError),
}

/// Result of an orderly registry shutdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownReport {
    /// Modules that stopped cleanly.
    pub stopped: Vec<String>,
    /// Modules that did not stop within the timeout (detached).
    pub stragglers: Vec<String>,
}

/// Route table + in-flight call chain tracking shared by all host contexts.
#[derive(Debug, Clone, Default)]
pub(crate) struct CallRouter {
    routes: Arc<RwLock<HashMap<String, mpsc::Sender<Command>>>>,
    /// caller -> the callee it is synchronously waiting on.
    blocked: Arc<Mutex<HashMap<String, String>>>,
    next_call_id: Arc<AtomicU64>,
}

impl CallRouter {
    fn route(&self, name: &str) -> Option<mpsc::Sender<Command>> {
        self.routes.read().unwrap().get(name).cloned()
    }

    pub(crate) fn insert(&self, name: impl Into<String>, tx: mpsc::Sender<Command>) {
        self.routes.write().unwrap().insert(name.into(), tx);
    }

    /// Clears all routes; command streams close and pending calls see
    /// `ModuleUnavailable`.
    pub(crate) fn clear(&self) {
        self.routes.write().unwrap().clear();
        self.blocked.lock().unwrap().clear();
    }

    /// Walks the synchronous call chain from `target` following
    /// blocked links, and checks whether it reaches `caller` — the
    /// re-entrance deadlock rule (specification §6.1).
    pub(crate) fn blocked_chain(&self, target: &str, caller: &str) -> bool {
        let blocked = self.blocked.lock().unwrap();
        let mut current = target.to_string();
        for _ in 0..=blocked.len() {
            match blocked.get(&current) {
                Some(next) if next.as_str() == caller => return true,
                Some(next) => current = next.clone(),
                None => return false,
            }
        }
        false
    }

    /// Synchronous call `caller -> target` with timeout.
    pub async fn call(
        &self,
        caller: &str,
        target: &str,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, ModuleError> {
        let Some(tx) = self.route(target) else {
            return Err(ModuleError::unavailable(target));
        };
        if caller != ENGINE_CALLER
            && self.blocked_chain(target, caller)
        {
            return Err(ModuleError::Reentrant {
                from: caller.to_string(),
                to: target.to_string(),
            });
        }

        let (reply_tx, reply_rx) = oneshot::channel();
        let call = MethodCall::new(method.to_string(), params, Some(reply_tx));
        tx.send(Command::MethodCall(call))
            .await
            .map_err(|_| ModuleError::unavailable(target))?;

        let tracked = caller != ENGINE_CALLER;
        if tracked {
            self.blocked
                .lock()
                .unwrap()
                .insert(caller.to_string(), target.to_string());
        }

        let outcome = tokio::time::timeout(timeout, reply_rx).await;

        if tracked {
            self.blocked.lock().unwrap().remove(caller);
        }

        match outcome {
            Err(_) => Err(ModuleError::timeout(timeout.as_millis() as u64)),
            Ok(Err(_)) => Err(ModuleError::unavailable(target)),
            Ok(Ok(result)) => result,
        }
    }

    /// Fire-and-forget call; the reply arrives on the caller's command
    /// stream as [`Command::AsyncReply`]. Never blocks the caller and is
    /// never inserted into the blocked chain.
    pub fn call_async(
        &self,
        caller: &str,
        target: &str,
        method: &str,
        params: Value,
    ) -> Result<u64, ModuleError> {
        let Some(tx) = self.route(target) else {
            return Err(ModuleError::unavailable(target));
        };
        let Some(reply_to) = self.route(caller) else {
            return Err(ModuleError::unavailable(caller));
        };

        let call_id = self.next_call_id.fetch_add(1, Ordering::Relaxed);

        let target = target.to_string();
        let method = method.to_string();
        tokio::spawn(async move {
            let (reply_tx, reply_rx) = oneshot::channel();
            let attempt = tx
                .send(Command::MethodCall(MethodCall::new(method, params, Some(reply_tx))))
                .await;

            let result = match attempt {
                Err(_) => Err(ModuleError::unavailable(target)),
                Ok(()) => match tokio::time::timeout(ASYNC_CALL_TIMEOUT, reply_rx).await {
                    Err(_) => Err(ModuleError::timeout(ASYNC_CALL_TIMEOUT.as_millis() as u64)),
                    Ok(Err(_)) => Err(ModuleError::unavailable(target)),
                    Ok(Ok(result)) => result,
                },
            };

            let _ = reply_to.send(Command::AsyncReply { call_id, result }).await;
        });

        Ok(call_id)
    }

    /// The count of registered routes.
    #[cfg(test)]
    fn route_count(&self) -> usize {
        self.routes.read().unwrap().len()
    }
}

struct RegistryEntry {
    descriptor: ModuleDescriptor,
    task: ModuleTask,
    /// Kept so shutdown can close the module's command stream by dropping.
    #[allow(dead_code)]
    _commands: mpsc::Sender<Command>,
}

/// Owns all running modules (specification §3.1).
pub struct ModuleRegistry {
    bus: EventBus,
    root: CancellationToken,
    router: CallRouter,
    callbacks: CallbackHub,
    entries: HashMap<String, RegistryEntry>,
}

impl ModuleRegistry {
    /// Creates a registry over a fresh bus with default capacities.
    pub fn new() -> Self {
        Self::with_bus(EventBus::new(DEFAULT_BUS_CAPACITY))
    }

    /// Creates a registry over the given bus; the bus stays shared with
    /// every host context.
    pub fn with_bus(bus: EventBus) -> Self {
        Self {
            bus,
            root: crate::scheduler::new_root_token(),
            router: CallRouter::default(),
            callbacks: CallbackHub::new(),
            entries: HashMap::new(),
        }
    }

    /// The shared bus.
    pub fn bus(&self) -> &EventBus {
        &self.bus
    }

    /// The root cancellation token — the engine's lifecycle control.
    pub fn root_token(&self) -> &CancellationToken {
        &self.root
    }

    /// Names of loaded modules.
    pub fn module_names(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }

    /// The descriptor of a loaded module.
    pub fn descriptor(&self, name: &str) -> Option<ModuleDescriptor> {
        self.entries.get(name).map(|e| e.descriptor.clone())
    }

    /// The callback hub — the Lua runtime (later phase) registers language
    /// handlers here.
    pub fn callbacks(&self) -> &CallbackHub {
        &self.callbacks
    }

    /// Emits a core event (`source = "noisy"`).
    pub fn emit_core(&self, kind: &'static str, payload: impl Into<Value>) -> bool {
        let kind = kind.strip_prefix("noisy.").unwrap_or(kind);
        self.bus.emit(NoisyEvent::new(ENGINE_CALLER, kind, payload))
    }

    /// Loads a module: validates its descriptor, uniqueness, and depends
    /// set, wires the command channel and host context, spawns the module
    /// actor, and emits `noisy.service_started` (specification §3.2).
    pub async fn load(&mut self, module: Box<dyn NoisyModule>, config: Value) -> Result<(), RegistryError> {
        let descriptor = module.descriptor();
        descriptor.validate()?;
        let name = descriptor.name.clone();

        if self.entries.contains_key(&name) {
            return Err(RegistryError::Duplicate(name.clone()));
        }
        for dep in &descriptor.depends {
            if !self.entries.contains_key(dep) {
                return Err(RegistryError::MissingDependency {
                    module: name.clone(),
                    depends_on: dep.clone(),
                });
            }
        }

        let (commands_tx, commands_rx) = mpsc::channel::<Command>(DEFAULT_COMMAND_CAPACITY);
        self.router.insert(name.clone(), commands_tx.clone());
        let shutdown = self.root.child_token();
        let host = HostCtx::new(
            name.clone(),
            self.bus.clone(),
            self.router.clone(),
            self.callbacks.clone(),
        );
        let ctx = ModuleContext::new(commands_rx, host, shutdown, config);

        let join = module.start(ctx);

        tracing::info!(module = %name, "module started");
        self.emit_core(
            core_events::SERVICE_STARTED,
            json!({ "module": name.clone() }),
        );

        self.entries.insert(
            name.clone(),
            RegistryEntry {
                descriptor,
                task: ModuleTask::new(name.clone(), join),
                _commands: commands_tx,
            },
        );
        Ok(())
    }

    /// Engine/test-level synchronous call into a module (`caller = noisy`).
    /// Also the production entry point for boot-time calls and, later,
    /// for the Lua runtime's method dispatch.
    pub async fn call(
        &self,
        module: &str,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, ModuleError> {
        self.router
            .call(ENGINE_CALLER, module, method, params, timeout)
            .await
    }

    /// Ordered shutdown (specification §3.1): cancel the root token (which
    /// cancels every module child token), close command streams so modules
    /// can drain, then join each task with a per-module timeout.
    /// Stragglers are detached and reported with `noisy.shutdown_timeout`.
    pub async fn shutdown(&mut self, timeout: Duration) -> ShutdownReport {
        self.root.cancel();
        self.emit_core(core_events::SHUTDOWN, json!({}));
        self.router.clear(); // closes command streams: modules finish drain

        let drained: Vec<(String, ModuleTask)> = self
            .entries
            .drain()
            .map(|(name, entry)| (name, entry.task))
            .collect();

        let mut report = ShutdownReport {
            stopped: Vec::new(),
            stragglers: Vec::new(),
        };

        for (name, task) in drained {
            match task.join_with_timeout(timeout).await {
                ShutdownStatus::Completed => {
                    self.emit_core(core_events::SERVICE_STOPPED, json!({ "module": name }));
                    report.stopped.push(name);
                }
                ShutdownStatus::TimedOut => {
                    self.emit_core(
                        core_events::SHUTDOWN_TIMEOUT,
                        json!({ "module": name }),
                    );
                    report.stragglers.push(name);
                }
            }
        }
        report
    }
}

impl Default for ModuleRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocked_chain_detects_cycles() {
        let router = CallRouter::default();
        // a is waiting on b; b is waiting on c
        router.blocked.lock().unwrap().insert("a".into(), "b".into());
        router.blocked.lock().unwrap().insert("b".into(), "c".into());

        // c calling a would re-enter a while a waits (transitively) on c
        assert!(router.blocked_chain("a", "c"));
        assert!(router.blocked_chain("a", "b"));
        assert!(router.blocked_chain("b", "c"));

        // no cycle: a calling c is fine (c waits on nothing), and neither
        // a calling b's chain nor unrelated pairs re-enter
        assert!(!router.blocked_chain("c", "a"));
        assert!(!router.blocked_chain("b", "a"));
        assert!(!router.blocked_chain("c", "b"));
        assert!(!router.blocked_chain("x", "a"));
    }

    #[test]
    fn route_registration_and_clear() {
        let router = CallRouter::default();
        let (tx, _rx) = mpsc::channel::<Command>(4);
        router.insert("m", tx);
        assert_eq!(router.route_count(), 1);
        router.clear();
        assert_eq!(router.route_count(), 0);
        assert!(router.route("m").is_none());
    }
}
