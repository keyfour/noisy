//! The module model: descriptors, the module trait, the runtime context,
//! and the host services (specification §3.3, §3.4, §6.1).
//!
//! Both module paths — the in-tree Rust trait and the dynamic C ABI —
//! surface the same conceptual model. This module implements the Rust side;
//! `noisy_ffi` implements the C ABI over the same JSON vocabulary.

use crate::bus::{EventBus, EventStream, NoisyEvent};
use crate::callback::{CallbackHub, CallbackToken};
use crate::error::ModuleError;
use crate::registry::CallRouter;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// A method declared by a module with ordered parameter names. Positional
/// Lua arguments are packed into these names by the binding layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MethodDecl {
    /// Method name, ASCII identifier.
    pub name: String,
    /// Ordered parameter names; optional parameters may be omitted by
    /// callers. The trailing `callback` parameter receives an opaque token
    /// when a function is passed (specification §3.1).
    #[serde(default)]
    pub params: Vec<String>,
}

impl MethodDecl {
    /// Declares a method without parameters.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            params: Vec::new(),
        }
    }

    /// Builder: sets parameter names.
    pub fn params(mut self, params: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.params = params.into_iter().map(Into::into).collect();
        self
    }
}

/// The public type of a module — the replacement for compiled metadata
/// (specification §3.3). The engine validates it at load and derives all
/// bindings from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleDescriptor {
    /// Unique module name, ASCII identifier.
    pub name: String,
    /// Module version (SemVer-strict per specification §9).
    pub version: String,
    /// Signal kinds; the bus delivers them as `"{name}.{kind}"`.
    #[serde(default)]
    pub signals: Vec<String>,
    /// Methods this module serves.
    #[serde(default)]
    pub methods: Vec<MethodDecl>,
    /// Other modules this one depends on, validated at boot.
    #[serde(default)]
    pub depends: Vec<String>,
}

impl ModuleDescriptor {
    /// Starts a descriptor with a name and a default version.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: "0.0.0".into(),
            signals: Vec::new(),
            methods: Vec::new(),
            depends: Vec::new(),
        }
    }

    /// Builder: sets the version.
    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    /// Builder: declares signal kinds.
    pub fn signals(mut self, signals: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.signals = signals.into_iter().map(Into::into).collect();
        self
    }

    /// Builder: declares a method without parameters.
    pub fn method(mut self, name: impl Into<String>) -> Self {
        self.methods.push(MethodDecl::new(name));
        self
    }

    /// Builder: declares a method with ordered parameters.
    pub fn method_params(
        mut self,
        name: impl Into<String>,
        params: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.methods.push(MethodDecl::new(name).params(params));
        self
    }

    /// Builder: declares dependencies on other modules.
    pub fn depends(mut self, depends: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.depends = depends.into_iter().map(Into::into).collect();
        self
    }

    /// Looks up a method declaration.
    pub fn method_decl(&self, name: &str) -> Option<&MethodDecl> {
        self.methods.iter().find(|m| m.name == name)
    }

    /// Fully-qualified signal names this module may emit.
    pub fn full_signal_names(&self) -> impl Iterator<Item = String> + '_ {
        self.signals.iter().map(move |s| format!("{}.{}", self.name, s))
    }

    /// Validates naming and uniqueness rules:
    /// identifiers are `[A-Za-z][A-Za-z0-9_]*`, signal and method names are
    /// unique, parameter names are identifiers.
    pub fn validate(&self) -> Result<(), ModuleError> {
        check_identifier(&self.name, "module name").map_err(ModuleError::Descriptor)?;
        if self.name == "noisy" {
            return Err(ModuleError::Descriptor(
                "module name `noisy` is reserved by the core".into(),
            ));
        }
        if self.version.trim().is_empty() {
            return Err(ModuleError::Descriptor(format!(
                "module `{}`: empty version",
                self.name
            )));
        }
        let mut seen_signals = HashSet::new();
        for signal in &self.signals {
            check_identifier(signal, "signal").map_err(ModuleError::Descriptor)?;
            if !seen_signals.insert(signal.as_str()) {
                return Err(ModuleError::Descriptor(format!(
                    "module `{}`: duplicate signal `{signal}`",
                    self.name
                )));
            }
        }
        let mut seen_methods = HashSet::new();
        for method in &self.methods {
            check_identifier(&method.name, "method").map_err(ModuleError::Descriptor)?;
            if !seen_methods.insert(method.name.as_str()) {
                return Err(ModuleError::Descriptor(format!(
                    "module `{}`: duplicate method `{}`",
                    self.name, method.name
                )));
            }
            let mut seen_params = HashSet::new();
            for param in &method.params {
                check_identifier(param, "parameter").map_err(ModuleError::Descriptor)?;
                if !seen_params.insert(param.as_str()) {
                    return Err(ModuleError::Descriptor(format!(
                        "module `{}`: duplicate parameter `{param}` on method `{}`",
                        self.name, method.name
                    )));
                }
            }
        }
        let mut seen_deps = HashSet::new();
        for dep in &self.depends {
            check_identifier(dep, "dependency").map_err(ModuleError::Descriptor)?;
            if !seen_deps.insert(dep.as_str()) {
                return Err(ModuleError::Descriptor(format!(
                    "module `{}`: duplicate dependency `{dep}`",
                    self.name
                )));
            }
        }
        if self.depends.iter().any(|d| d == &self.name) {
            return Err(ModuleError::Descriptor(format!(
                "module `{}`: depends on itself",
                self.name
            )));
        }
        Ok(())
    }
}

fn check_identifier(name: &str, what: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let starts_ok = chars.next().map(|c| c.is_ascii_alphabetic()).unwrap_or(false);
    let rest_ok = chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if starts_ok && rest_ok {
        return Ok(());
    }
    Err(format!("invalid {what} `{name}`: expected `[A-Za-z][A-Za-z0-9_]*`"))
}

/// A method invocation delivered to a module. The receiver replies exactly
/// once with [`MethodCall::reply`]; a missing reply surfaces to the caller
/// as [`ModuleError::ModuleUnavailable`] after its timeout.
#[derive(Debug)]
pub struct MethodCall {
    method: String,
    params: Value,
    reply: Option<tokio::sync::oneshot::Sender<Result<Value, ModuleError>>>,
}

impl MethodCall {
    pub(crate) fn new(
        method: impl Into<String>,
        params: Value,
        reply: Option<tokio::sync::oneshot::Sender<Result<Value, ModuleError>>>,
    ) -> Self {
        Self {
            method: method.into(),
            params,
            reply,
        }
    }

    /// A call without a replier, for direct `serve`-level tests across the
    /// workspace (hidden from the public docs; the registry is the real
    /// constructor of method calls).
    #[doc(hidden)]
    pub fn for_tests(method: impl Into<String>, params: Value) -> Self {
        Self::new(method, params, None)
    }

    /// The invoked method name.
    pub fn method(&self) -> &str {
        &self.method
    }

    /// The call parameters (JSON object built from declared parameter names).
    pub fn params(&self) -> &Value {
        &self.params
    }

    /// Replies to the caller.
    pub fn reply(self, result: Result<Value, ModuleError>) {
        if let Some(tx) = self.reply {
            let _ = tx.send(result);
        }
    }

    /// Whether this call expects a reply.
    pub fn expects_reply(&self) -> bool {
        self.reply.is_some()
    }
}

/// Everything the module actor can receive, one message at a time
/// (specification §3.4).
#[derive(Debug)]
pub enum Command {
    /// A method invocation addressed to this module.
    MethodCall(MethodCall),
    /// The reply to an earlier [`HostCtx::call_async`] issued by this module.
    AsyncReply {
        /// The id returned by `call_async`.
        call_id: u64,
        /// The reply outcome.
        result: Result<Value, ModuleError>,
    },
}

/// The in-tree module contract (specification §3.4). Third-party Rust modules
/// compile as `cdylib`s over the C ABI instead.
pub trait NoisyModule: Send + 'static {
    /// The module's public type.
    fn descriptor(&self) -> ModuleDescriptor;

    /// Runs the module until the shutdown token in the context fires.
    /// Commands are delivered one at a time through the context; a
    /// responding module replies with
    /// [`MethodCall::reply`] or lets the caller time out.
    fn start(self: Box<Self>, ctx: ModuleContext) -> JoinHandle<()>;
}

/// Alias of the module command stream for descriptor clarity.
pub type CommandReceiver = mpsc::Receiver<Command>;

/// Everything a running module receives from the engine (specification
/// §3.4). Public fields mirror the specification; `next_command` and
/// `drain` operate on the command stream.
pub struct ModuleContext {
    /// The command stream: one message at a time.
    pub commands: CommandReceiver,
    /// Host services (emit, calls, slots, logging, callbacks).
    pub host: HostCtx,
    /// Child cancellation token of the engine's root token.
    pub shutdown: CancellationToken,
    /// Per-module configuration snapshot taken at load time.
    pub config: Value,
}

impl ModuleContext {
    pub(crate) fn new(commands: CommandReceiver, host: HostCtx, shutdown: CancellationToken, config: Value) -> Self {
        Self {
            commands,
            host,
            shutdown,
            config,
        }
    }

    /// The next command, or `None` once every sender has been dropped
    /// (registry shutdown) — the moment a module finishes its drain phase.
    pub async fn next_command(&mut self) -> Option<Command> {
        self.commands.recv().await
    }

    /// Drain phase helper: consumes everything still buffered in the
    /// command stream, auto-failing pending `MethodCall`s with
    /// [`ModuleError::ShuttingDown`] so waiting callers see a precise
    /// reason instead of a timeout. Returns the number drained.
    pub async fn drain(&mut self) -> usize {
        let mut count = 0;
        while let Some(cmd) = self.commands.recv().await {
            if let Command::MethodCall(call) = cmd {
                call.reply(Err(ModuleError::ShuttingDown));
            }
            count += 1;
        }
        count
    }
}

/// Log level used by [`HostCtx::log`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    /// Lower-case string form.
    pub fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }
}

/// Host services provided to every running module (specification §6.1).
/// Cheap to clone; each clone carries the owning module's identity.
#[derive(Debug, Clone)]
pub struct HostCtx {
    module: String,
    bus: EventBus,
    router: CallRouter,
    callbacks: CallbackHub,
}

impl HostCtx {
    pub(crate) fn new(module: String, bus: EventBus, router: CallRouter, callbacks: CallbackHub) -> Self {
        Self {
            module,
            bus,
            router,
            callbacks,
        }
    }

    /// The owning module's name.
    pub fn module_name(&self) -> &str {
        &self.module
    }

    /// Current wall-clock time as epoch milliseconds.
    pub fn now_ms(&self) -> u64 {
        crate::bus::now_ms()
    }

    /// Publishes `"{module}.{kind}"` onto the bus. Returns `false` when no
    /// subscriber was listening.
    pub fn emit(&self, kind: &str, payload: impl Into<Value>) -> bool {
        self.bus.emit(NoisyEvent::new(self.module.clone(), kind, payload))
    }

    /// Publishes a structured log entry as the canonical `log.entry` bus
    /// event (source `log`; the owning module travels in the `module`
    /// field of the payload). The `log` module subscribes to it and mirrors
    /// entries to the console under its level filter (specification §4.6).
    pub fn log(&self, level: LogLevel, msg: &str, fields: Option<Value>) {
        self.bus.emit(NoisyEvent::new(
            "log",
            "entry",
            json!({
                "module": self.module.clone(),
                "level": level.as_str(),
                "msg": msg,
                "fields": fields.unwrap_or(json!({})),
                "timestamp": self.now_ms(),
            }),
        ));
    }

    /// Synchronous request/reply to another module with a timeout.
    /// Rejected with [`ModuleError::Reentrant`] when the target module is
    /// already waiting inside this caller's synchronous call chain.
    pub async fn call(
        &self,
        module: &str,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, ModuleError> {
        self.router.call(&self.module, module, method, params, timeout).await
    }

    /// Fires a command at another module; the reply arrives later on this
    /// module's command stream as [`Command::AsyncReply`] with the
    /// returned call id. Use this for anything recursive or long-running.
    pub fn call_async(&self, module: &str, method: &str, params: Value) -> Result<u64, ModuleError> {
        self.router.call_async(&self.module, module, method, params)
    }

    /// Connects a slot: subscribes this module to matching bus events,
    /// delivered through the stream with lag handling (specification
    /// §3.1). Dropping the stream disconnects the slot (the `off(id)`
    /// equivalent for modules).
    pub fn on(&self, pattern: impl Into<String>) -> EventStream {
        self.bus.stream(pattern, self.module.clone())
    }

    /// Invokes the language-native handler behind `token` (e.g. a Lua
    /// closure passed as a method argument). Returns `false` when nothing
    /// is registered behind the token.
    pub fn invoke_callback(&self, token: CallbackToken, payload: impl Into<Value>) -> bool {
        self.callbacks.invoke(token, payload)
    }

    /// The shared callback hub (token allocation and routing).
    pub fn callback_hub(&self) -> &CallbackHub {
        &self.callbacks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptors_validate_names_and_rejects_reserved() {
        ModuleDescriptor::new("good_name")
            .signals(["a", "b"])
            .method("m")
            .method_params("m2", ["x", "y"])
            .validate()
            .expect("valid descriptor");

        for bad in ["", "1abc", "has-dash", "has.dot", "has space"] {
            let err = ModuleDescriptor::new(bad).validate().unwrap_err();
            assert!(matches!(err, ModuleError::Descriptor(_)), "{bad}: {err:?}");
        }

        let err = ModuleDescriptor::new("noisy").validate().unwrap_err();
        assert!(err.to_string().contains("reserved"));

        let mut dup_signal = ModuleDescriptor::new("m");
        dup_signal.signals = vec!["a".into(), "a".into()];
        let err = dup_signal.validate().unwrap_err();
        assert!(err.to_string().contains("duplicate signal"));

        let err = ModuleDescriptor::new("m")
            .method("f")
            .method("f")
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("duplicate method"));
    }
}

