//! The Lua extension layer (specification §3.1, §5): one `mlua::Lua`
//! instance owned by a dedicated thread, a job inbox as the only way in,
//! descriptor-generated module namespaces, and the `noisy` global.
//!
//! Threading model:
//!
//! - The Tokio world never touches Lua. Bus events and callback invocations
//!   are forwarded into a single job inbox; the Lua thread drains it with
//!   `Handle::block_on`.
//! - Calls *from* Lua into modules run on the Lua thread via
//!   `Handle::block_on(host.call(...))`. Replies come from module tasks on
//!   the runtime's worker threads, so the Lua thread never deadlocks; work
//!   that would re-enter Lua is queued back into the inbox (there is no
//!   synchronous path into Lua), which is the no-re-entrancy guarantee.
//! - Timer callbacks (Phase 4) arrive as [`CallbackHub`] tokens, i.e. the
//!   same `LuaJob::InvokeCallback` path — one inbox, one delivery rule.
//!
//! Every Lua execution goes through a protected entry point; errors become
//! `noisy.script_error { path, line, message }` events and the loop
//! continues.

use crate::bus::{event_matches, EventBus, NoisyEvent};
use crate::callback::{CallbackEnvelope, CallbackToken};
use crate::module::{HostCtx, MethodDecl, ModuleDescriptor};
use mlua::{Function, Lua, LuaOptions, MultiValue, StdLib, Table, Value as LuaValue};
use serde_json::{Map, Value};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// Default timeout for module calls issued from Lua.
pub const LUA_CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Job inbox capacity.
const INBOX_CAPACITY: usize = 1024;

/// Everything crossing into the Lua thread.
#[derive(Debug)]
pub enum LuaJob {
    /// A bus event delivered to matching `noisy.on` subscriptions.
    BusEvent(NoisyEvent),
    /// A module invoked a Lua callback token.
    InvokeCallback(CallbackEnvelope),
    /// Evaluate a source chunk (application entry, plugins).
    RunSource {
        /// Chunk name used in error messages.
        name: String,
        /// Lua source.
        source: String,
        /// Set when the caller awaits completion.
        reply: Option<oneshot::Sender<Result<(), String>>>,
    },
    /// Re-evaluate a script in place (hot reload, Phase 9).
    ReloadScript {
        /// Script to reload.
        path: PathBuf,
    },
    /// Evaluate an expression and reply with its JSON value.
    Eval {
        /// Lua expression.
        code: String,
        /// Reply channel.
        reply: oneshot::Sender<Result<Value, String>>,
    },
    /// Stop the dispatch loop.
    Shutdown,
}

/// Dependencies injected into the runtime.
pub struct LuaDeps {
    /// The shared bus.
    pub bus: EventBus,
    /// Engine-level host context (caller identity `noisy`).
    pub host: HostCtx,
    /// Engine root token: cancelling it stops the loop.
    pub root: CancellationToken,
    /// Descriptors of loaded modules — the namespace source.
    pub descriptors: Vec<ModuleDescriptor>,
    /// Version reported by `noisy.version()`.
    pub version: String,
    /// Embedded standard library (`require` names → sources).
    pub stdlib: BTreeMap<String, String>,
    /// Directories searched by `require` after the embedded stdlib.
    pub script_roots: Vec<PathBuf>,
}

impl LuaDeps {
    /// Deps with an empty stdlib and no script roots (tests fill these in).
    pub fn new(
        bus: EventBus,
        host: HostCtx,
        root: CancellationToken,
        descriptors: Vec<ModuleDescriptor>,
    ) -> Self {
        Self {
            bus,
            host,
            root,
            descriptors,
            version: env!("CARGO_PKG_VERSION").to_string(),
            stdlib: BTreeMap::new(),
            script_roots: Vec::new(),
        }
    }
}

/// The embedded standard library shipped with the core (specification
/// §5.1). Contents grow in Phase 6; the loader mechanism is the Phase 2
/// deliverable.
pub fn embedded_stdlib() -> BTreeMap<String, String> {
    [("util".to_string(), include_str!("../lua/util.lua").to_string())]
        .into_iter()
        .collect()
}

/// Cloneable handle to the dispatch thread.
#[derive(Clone)]
pub struct LuaHandle {
    tx: mpsc::Sender<LuaJob>,
}

impl LuaHandle {
    /// Queues a script evaluation and awaits its completion.
    pub async fn run_source(&self, name: impl Into<String>, source: impl Into<String>) -> Result<(), String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(LuaJob::RunSource {
                name: name.into(),
                source: source.into(),
                reply: Some(reply),
            })
            .await
            .map_err(|_| "lua dispatch thread is gone".to_string())?;
        rx.await.map_err(|_| "lua dispatch thread is gone".to_string())?
    }

    /// Reads and evaluates a script file on the Lua thread.
    pub async fn load_script(&self, path: impl Into<PathBuf>) -> Result<(), String> {
        let path = path.into();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "script".into());
        let source = std::fs::read_to_string(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        self.run_source(name, source).await
    }

    /// Evaluates a Lua expression on the dispatch thread; the value crosses
    /// back as JSON.
    pub async fn eval(&self, code: impl Into<String>) -> Result<Value, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(LuaJob::Eval {
                code: code.into(),
                reply,
            })
            .await
            .map_err(|_| "lua dispatch thread is gone".to_string())?;
        rx.await.map_err(|_| "lua dispatch thread is gone".to_string())?
    }

    /// Queues a hot-reload request (Phase 9).
    pub async fn reload(&self, path: impl Into<PathBuf>) -> Result<(), String> {
        self.tx
            .send(LuaJob::ReloadScript { path: path.into() })
            .await
            .map_err(|_| "lua dispatch thread is gone".to_string())
    }

    /// Stops the dispatch loop.
    pub async fn shutdown(&self) -> Result<(), String> {
        self.tx
            .send(LuaJob::Shutdown)
            .await
            .map_err(|_| "lua dispatch thread is gone".to_string())
    }
}

/// The Lua runtime: spawns the dispatch thread and the inbox forwarders.
pub struct LuaRuntime {
    handle: LuaHandle,
    join: Option<std::thread::JoinHandle<()>>,
}

impl LuaRuntime {
    /// Starts the runtime: one dedicated thread owning the `mlua::Lua`
    /// instance, plus Tokio forwarders for bus events and callback tokens.
    pub fn start(deps: LuaDeps) -> Self {
        let (tx, rx) = mpsc::channel::<LuaJob>(INBOX_CAPACITY);

        // The Lua runtime is the production consumer of callback tokens.
        let (sink_tx, sink_rx) = mpsc::channel::<CallbackEnvelope>(INBOX_CAPACITY);
        deps.host.callback_hub().set_sink(sink_tx);

        // Forward bus events into the inbox.
        let bus = deps.bus.clone();
        let root = deps.root.clone();
        let event_tx = tx.clone();
        tokio::spawn(async move {
            let mut stream = bus.stream("*", "lua");
            loop {
                tokio::select! {
                    _ = root.cancelled() => break,
                    maybe = stream.next() => match maybe {
                        Some(event) => {
                            if event_tx.send(LuaJob::BusEvent(event)).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    },
                }
            }
        });

        // Forward callback invocations into the inbox.
        let root = deps.root.clone();
        let callback_tx = tx.clone();
        tokio::spawn(async move {
            let mut sink = sink_rx;
            loop {
                tokio::select! {
                    _ = root.cancelled() => break,
                    maybe = sink.recv() => match maybe {
                        Some(envelope) => {
                            if callback_tx.send(LuaJob::InvokeCallback(envelope)).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    },
                }
            }
        });

        let rt = Handle::current();
        let handle = LuaHandle { tx };
        let join = std::thread::Builder::new()
            .name("noisy-lua".into())
            .spawn(move || dispatch_loop(deps, rx, rt))
            .expect("failed to spawn the noisy-lua thread");

        Self {
            handle,
            join: Some(join),
        }
    }

    /// A cloneable handle for submitting jobs.
    pub fn handle(&self) -> LuaHandle {
        self.handle.clone()
    }

    /// Joins the dispatch thread (returns after shutdown). Sends a wake-up
    /// job first: the runtime's own handle keeps the inbox open, so without
    /// this the loop would wait forever on `recv`.
    pub fn join(mut self) -> std::thread::Result<()> {
        let join = self.join.take();
        let _ = self.handle.tx.try_send(LuaJob::Shutdown);
        match join {
            Some(join) => join.join(),
            None => Ok(()),
        }
    }
}

/// Debug guard proving every Lua access happens on the owning thread
/// (Phase 2 acceptance; enabled in CI through debug assertions).
struct LuaThreadGuard {
    owner: std::thread::ThreadId,
}

impl LuaThreadGuard {
    fn new() -> Self {
        Self {
            owner: std::thread::current().id(),
        }
    }

    fn assert_owner(&self) {
        debug_assert_eq!(
            self.owner,
            std::thread::current().id(),
            "Lua state accessed off the noisy-lua thread"
        );
    }
}

/// One `noisy.on` subscription.
struct Subscription {
    pattern: String,
    handler: Function,
}

/// Mutable Lua-side state, confined to the dispatch thread.
struct Inner {
    subscriptions: BTreeMap<u64, Subscription>,
    next_subscription: u64,
    callbacks: HashMap<CallbackToken, Function>,
    used: HashSet<String>,
    current_chunk: Option<String>,
}

type Shared = Rc<RefCell<Inner>>;

fn dispatch_loop(deps: LuaDeps, rx: mpsc::Receiver<LuaJob>, rt: Handle) {
    let lua = match Lua::new_with(
        StdLib::COROUTINE | StdLib::TABLE | StdLib::STRING | StdLib::UTF8 | StdLib::MATH | StdLib::PACKAGE | StdLib::OS,
        LuaOptions::default(),
    ) {
        Ok(lua) => lua,
        Err(err) => {
            tracing::error!(error = %err, "failed to initialize the Lua state");
            return;
        }
    };

    if let Err(err) = sandbox_stdlib(&lua) {
        tracing::error!(error = %err, "failed to sandbox the Lua stdlib");
        return;
    }

    let state: Shared = Rc::new(RefCell::new(Inner {
        subscriptions: BTreeMap::new(),
        next_subscription: 1,
        callbacks: HashMap::new(),
        used: HashSet::new(),
        current_chunk: None,
    }));

    if let Err(err) = install_noisy(&lua, &state, &deps) {
        tracing::error!(error = %err, "failed to install the noisy global");
        return;
    }
    for descriptor in &deps.descriptors {
        if let Err(err) = install_namespace(&lua, &state, descriptor, &deps.host, &rt) {
            tracing::error!(
                module = %descriptor.name,
                error = %err,
                "failed to generate the Lua namespace"
            );
            return;
        }
    }
    if let Err(err) = install_require(&lua, &deps) {
        tracing::error!(error = %err, "failed to install require");
        return;
    }

    tracing::info!(modules = deps.descriptors.len(), "lua runtime ready");

    let guard = LuaThreadGuard::new();
    let mut rx = rx;
    loop {
        guard.assert_owner();
        let Some(job) = rt.block_on(rx.recv()) else { break };
        match job {
            LuaJob::BusEvent(event) => dispatch_event(&lua, &state, &deps.bus, &event),
            LuaJob::InvokeCallback(envelope) => invoke_token(&lua, &state, &deps.bus, envelope),
            LuaJob::RunSource { name, source, reply } => {
                let result = run_source(&lua, &state, &deps.bus, &name, &source);
                if let Some(reply) = reply {
                    let _ = reply.send(result);
                }
            }
            LuaJob::ReloadScript { path } => {
                // Hot reload arrives in Phase 9; the job shape is already
                // part of the inbox contract.
                emit_script_error(&deps.bus, &path.to_string_lossy(), 0,
                    "hot reload is not implemented until Phase 9");
            }
            LuaJob::Eval { code, reply } => {
                let result = eval_chunk(&lua, &code);
                let _ = reply.send(result);
            }
            LuaJob::Shutdown => break,
        }
    }
    tracing::info!("lua dispatch loop stopped");
}

/// Opens the safe subset of the stdlib and removes unsafe escapes:
/// `io` is never opened; `os` loses its process/environment functions;
/// `dofile`/`loadfile` are dropped (scripts load through `require`).
fn sandbox_stdlib(lua: &Lua) -> Result<(), mlua::Error> {
    let globals = lua.globals();
    globals.raw_set("dofile", LuaValue::Nil)?;
    globals.raw_set("loadfile", LuaValue::Nil)?;

    let os: Table = globals.raw_get("os")?;
    for name in ["execute", "exit", "getenv", "remove", "rename", "setlocale", "tmpname"] {
        os.raw_set(name, LuaValue::Nil)?;
    }
    Ok(())
}

/// Installs the `noisy` global (specification §5, LUA_API.md).
fn install_noisy(lua: &Lua, state: &Shared, deps: &LuaDeps) -> Result<(), mlua::Error> {
    let table = lua.create_table()?;

    {
        let state = state.clone();
        let on = lua.create_function(move |_, (pattern, handler): (String, Function)| {
            let mut inner = state.borrow_mut();
            let id = inner.next_subscription;
            inner.next_subscription += 1;
            inner.subscriptions.insert(
                id,
                Subscription {
                    pattern,
                    handler,
                },
            );
            Ok(id)
        })?;
        table.set("on", on)?;
    }

    {
        let state = state.clone();
        let off = lua.create_function(move |_, id: u64| {
            Ok(state.borrow_mut().subscriptions.remove(&id).is_some())
        })?;
        table.set("off", off)?;
    }

    {
        let bus = deps.bus.clone();
        let emit = lua.create_function(move |_, (name, payload): (String, Option<LuaValue>)| {
            let Some((source, kind)) = split_event_name(&name) else {
                return Err(mlua::Error::external(
                    "noisy.emit: event name must look like `source.kind`",
                ));
            };
            let payload = match payload {
                None | Some(LuaValue::Nil) => Value::Object(Map::new()),
                Some(value) => lua_to_json(value).map_err(mlua::Error::external)?,
            };
            Ok(bus.emit(NoisyEvent::new(source, kind, payload)))
        })?;
        table.set("emit", emit)?;
    }

    {
        let state = state.clone();
        let loaded: Vec<String> = deps.descriptors.iter().map(|d| d.name.clone()).collect();
        // noisy.use("serial", "net", ...) — variadic module names
        let r#use = lua.create_function(move |_, args: MultiValue| -> Result<bool, mlua::Error> {
            let mut names = Vec::new();
            for value in args.iter() {
                match value {
                    LuaValue::String(s) => names.push(
                        s.to_str()
                            .map_err(|_| mlua::Error::external("noisy.use: names must be strings"))?
                            .to_string(),
                    ),
                    _ => {
                        return Err(mlua::Error::external(
                            "noisy.use: expects one or more module name strings",
                        ))
                    }
                }
            }
            if names.is_empty() {
                return Err(mlua::Error::external("noisy.use: no module names given"));
            }
            let mut inner = state.borrow_mut();
            for name in &names {
                if !loaded.contains(name) {
                    return Err(mlua::Error::external(format!(
                        "noisy.use: module `{name}` is not loaded (available: {})",
                        loaded.join(", ")
                    )));
                }
                inner.used.insert(name.clone());
            }
            Ok(true)
        })?;
        table.set("use", r#use)?;
    }

    {
        let version = deps.version.clone();
        table.set(
            "version",
            lua.create_function(move |_, ()| Ok(version.clone()))?,
        )?;
    }

    {
        let reload = lua.create_function(|_, _path: String| -> Result<bool, mlua::Error> {
            Err(mlua::Error::external(
                "noisy.reload: hot reload is not implemented until Phase 9",
            ))
        })?;
        table.set("reload", reload)?;
    }

    {
        let root = deps.root.clone();
        let shutdown = lua.create_function(move |_, ()| {
            root.cancel();
            Ok(true)
        })?;
        table.set("shutdown", shutdown)?;
    }

    lua.globals().set("noisy", table)
}

/// Generates one module's global namespace from its descriptor: every
/// declared method becomes a Lua function that packs positional arguments
/// into the declared parameter names, converts function arguments into
/// callback tokens, performs the call, and converts the JSON reply back
/// (specification §3.3).
fn install_namespace(
    lua: &Lua,
    state: &Shared,
    descriptor: &ModuleDescriptor,
    host: &HostCtx,
    rt: &Handle,
) -> Result<(), mlua::Error> {
    let namespace = lua.create_table()?;
    for decl in &descriptor.methods {
        let function = make_method_function(lua, state, &descriptor.name, decl, host, rt)?;
        namespace.set(decl.name.as_str(), function)?;
    }
    lua.globals().set(descriptor.name.as_str(), namespace)
}

fn make_method_function(
    lua: &Lua,
    state: &Shared,
    module: &str,
    decl: &MethodDecl,
    host: &HostCtx,
    rt: &Handle,
) -> Result<Function, mlua::Error> {
    let module = module.to_string();
    let decl = decl.clone();
    let host = host.clone();
    let rt = rt.clone();
    let state = state.clone();

    lua.create_function(move |lua, args: MultiValue| {
        let mut params = Map::new();
        for (index, param) in decl.params.iter().enumerate() {
            let Some(value) = args.iter().nth(index) else {
                break; // trailing optional parameters may be omitted
            };
            let json = match value {
                LuaValue::Function(function) => {
                    // a Lua function argument becomes an opaque callback token
                    let token = host.callback_hub().allocate_token();
                    state.borrow_mut().callbacks.insert(token, function.clone());
                    Value::from(token)
                }
                other => lua_to_json(other.clone()).map_err(mlua::Error::external)?,
            };
            params.insert(param.clone(), json);
        }

        let call = rt.block_on(host.call(
            &module,
            &decl.name,
            Value::Object(params),
            LUA_CALL_TIMEOUT,
        ));

        match call {
            Ok(reply) => Ok(json_to_lua(lua, &reply)?),
            Err(err) => Err(mlua::Error::external(format!(
                "call to {module}.{} failed: {err}",
                decl.name
            ))),
        }
    })
}

/// Installs `require`: embedded stdlib first, then script roots, with
/// `package.loaded` bookkeeping (specification §3.1, §5.1).
fn install_require(lua: &Lua, deps: &LuaDeps) -> Result<(), mlua::Error> {
    let stdlib = deps.stdlib.clone();
    let roots = deps.script_roots.clone();

    let require = lua.create_function(move |lua, name: String| {
        let package: Table = lua.globals().raw_get("package")?;
        let loaded: Table = package.raw_get("loaded")?;
        if let Ok(value) = loaded.raw_get(name.as_str()) {
            if value != LuaValue::Nil {
                return Ok(value);
            }
        }

        let (chunk_name, source) = if let Some(source) = stdlib.get(&name) {
            (format!("lua/{name}.lua"), source.clone())
        } else {
            let mut path = None;
            for root in &roots {
                let candidate = root.join(format!("{name}.lua"));
                if candidate.is_file() {
                    path = Some(candidate);
                    break;
                }
            }
            match path {
                Some(path) => {
                    let source = std::fs::read_to_string(&path)
                        .map_err(|err| mlua::Error::external(format!("{}: {err}", path.display())))?;
                    (path.to_string_lossy().to_string(), source)
                }
                None => {
                    return Err(mlua::Error::external(format!(
                        "require: module `{name}` not found (embedded stdlib: {})",
                        stdlib.keys().cloned().collect::<Vec<_>>().join(", ")
                    )))
                }
            }
        };

        let result: LuaValue = lua
            .load(source)
            .set_name(chunk_name)
            .eval()
            .map_err(mlua::Error::external)?;
        let result = if result == LuaValue::Nil {
            LuaValue::Boolean(true)
        } else {
            result
        };
        loaded.raw_set(name.as_str(), result.clone())?;
        Ok(result)
    })?;

    lua.globals().set("require", require)
}

fn dispatch_event(lua: &Lua, state: &Shared, bus: &EventBus, event: &NoisyEvent) {
    let matching: Vec<u64> = state
        .borrow()
        .subscriptions
        .iter()
        .filter(|(_, sub)| event_matches(&sub.pattern, event))
        .map(|(id, _)| *id)
        .collect();

    for id in matching {
        let handler = state
            .borrow()
            .subscriptions
            .get(&id)
            .map(|sub| sub.handler.clone());
        let Some(handler) = handler else { continue };

        let table = event_to_table(lua, event);
        if let Err(err) = handler.call::<()>(table) {
            let (path, line, message) = split_script_error(&err);
            emit_script_error(bus, &path, line, &message);
        }
    }
}

fn invoke_token(lua: &Lua, state: &Shared, bus: &EventBus, envelope: CallbackEnvelope) {
    let handler = state.borrow().callbacks.get(&envelope.token).cloned();
    let Some(handler) = handler else {
        emit_script_error(
            bus,
            &state.borrow().current_chunk.clone().unwrap_or_default(),
            0,
            &format!("unknown callback token {}", envelope.token),
        );
        return;
    };

    let payload = json_to_lua(lua, &envelope.payload);
    if let Err(err) = handler.call::<()>(payload) {
        let (path, line, message) = split_script_error(&err);
        emit_script_error(bus, &path, line, &message);
    }
}

fn run_source(lua: &Lua, state: &Shared, bus: &EventBus, name: &str, source: &str) -> Result<(), String> {
    state.borrow_mut().current_chunk = Some(name.to_string());
    let result = lua.load(source).set_name(name).exec();
    state.borrow_mut().current_chunk = None;

    match result {
        Ok(()) => Ok(()),
        Err(err) => {
            let (path, line, message) = split_script_error(&err);
            emit_script_error(bus, &path, line, &message);
            Err(format!("{path}:{line}: {message}"))
        }
    }
}

fn eval_chunk(lua: &Lua, code: &str) -> Result<Value, String> {
    let value: LuaValue = lua
        .load(code)
        .set_name("eval")
        .eval()
        .map_err(|err| err.to_string())?;
    lua_to_json(value)
}

fn event_to_table(lua: &Lua, event: &NoisyEvent) -> Table {
    let table = lua.create_table().expect("table allocation");
    table.set("source", event.source.as_str()).expect("set source");
    table.set("kind", event.kind.as_str()).expect("set kind");
    table.set("name", event.name.as_str()).expect("set name");
    let payload = json_to_lua(lua, &event.payload).expect("payload conversion");
    table.set("payload", payload).expect("set payload");
    table.set("timestamp", event.timestamp).expect("set timestamp");
    table
}

/// Splits a fully qualified event name into `(source, kind)`.
pub fn split_event_name(name: &str) -> Option<(String, String)> {
    let (source, kind) = name.split_once('.')?;
    if source.is_empty() || kind.is_empty() {
        return None;
    }
    Some((source.to_string(), kind.to_string()))
}

/// Extracts `(path, line, message)` from an mlua error string of the form
/// `[string "app.lua"]:12: message`.
fn split_script_error(err: &mlua::Error) -> (String, u64, String) {
    let rendered = match err {
        mlua::Error::RuntimeError(message) => message.clone(),
        mlua::Error::CallbackError { cause, .. } => cause.to_string(),
        other => other.to_string(),
    };
    // try `[string "path"]:line: message` first, then `path:line: message`
    for (prefix, closing) in [("[string \"", "\"]:"), ("", ":")] {
        if let Some(rest) = rendered.strip_prefix(prefix) {
            if let Some((path, tail)) = rest.split_once(closing) {
                if let Some((line, message)) = tail.split_once(':') {
                    if let Ok(line) = line.trim().parse::<u64>() {
                        let path = path.rsplit('/').next().unwrap_or(path).to_string();
                        return (path, line, message.trim().to_string());
                    }
                }
            }
        }
    }
    ("<script>".to_string(), 0, rendered)
}

fn emit_script_error(bus: &EventBus, path: &str, line: u64, message: &str) {
    tracing::warn!(path, line, message, "script error");
    bus.emit(NoisyEvent::new(
        "noisy",
        "script_error",
        serde_json::json!({ "path": path, "line": line, "message": message }),
    ));
}

// ───────────────────── JSON ⇄ Lua boundary (the single converter)

/// Converts a Lua value into JSON at the language boundary.
pub fn lua_to_json(value: LuaValue) -> Result<Value, String> {
    match value {
        LuaValue::Nil => Ok(Value::Null),
        LuaValue::Boolean(b) => Ok(Value::Bool(b)),
        LuaValue::Integer(i) => Ok(Value::from(i)),
        LuaValue::Number(n) => Ok(Value::from(n)),
        LuaValue::String(s) => match s.to_str() {
            Ok(text) => Ok(Value::from(text.to_string())),
            Err(_) => Err(
                "non-UTF-8 strings must use the base64 payload convention (specification §3.5)"
                    .to_string(),
            ),
        },
        LuaValue::Table(table) => {
            // Pair iteration order is unspecified, so classify after
            // collecting: a pure 1..=n integer key set is an array, all
            // string keys is an object, anything else is an error.
            #[derive(Debug)]
            enum Key {
                Int(i64),
                Str(String),
            }
            let mut entries: Vec<(Key, Value)> = Vec::new();
            for pair in table.pairs::<LuaValue, LuaValue>() {
                let (key, value) = pair.map_err(|e| e.to_string())?;
                let json = lua_to_json(value)?;
                let key = match key {
                    LuaValue::Integer(i) => Key::Int(i),
                    LuaValue::String(s) => {
                        Key::Str(s.to_str().map_err(|_| "non-UTF-8 table key")?.to_string())
                    }
                    other => {
                        return Err(format!(
                            "unsupported table key type: {}",
                            other.type_name()
                        ))
                    }
                };
                entries.push((key, json));
            }

            if entries.is_empty() {
                return Ok(Value::Object(Map::new()));
            }
            if entries.iter().all(|(k, _)| matches!(k, Key::Int(_))) {
                let mut indexed: Vec<(i64, Value)> = entries
                    .into_iter()
                    .map(|(k, v)| match k {
                        Key::Int(i) => (i, v),
                        Key::Str(_) => unreachable!(),
                    })
                    .collect();
                indexed.sort_by_key(|(i, _)| *i);
                let sequential = indexed
                    .iter()
                    .enumerate()
                    .all(|(position, (key, _))| *key == position as i64 + 1);
                if !sequential {
                    return Err("integer table keys must form the sequence 1..=n".to_string());
                }
                return Ok(Value::Array(
                    indexed.into_iter().map(|(_, v)| v).collect(),
                ));
            }
            if entries.iter().all(|(k, _)| matches!(k, Key::Str(_))) {
                let map: Map<String, Value> = entries
                    .into_iter()
                    .map(|(k, v)| match k {
                        Key::Str(s) => (s, v),
                        Key::Int(_) => unreachable!(),
                    })
                    .collect();
                return Ok(Value::Object(map));
            }
            Err("mixed table keys are not convertible".to_string())
        }
        LuaValue::Function(_) => Err(
            "function values must be passed as the trailing callback parameter".to_string(),
        ),
        other => Err(format!(
            "unsupported Lua value of type {}",
            other.type_name()
        )),
    }
}

/// Converts JSON into a Lua value at the language boundary.
pub fn json_to_lua(lua: &Lua, json: &Value) -> Result<LuaValue, mlua::Error> {
    let value = match json {
        Value::Null => LuaValue::Nil,
        Value::Bool(b) => LuaValue::Boolean(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                LuaValue::Integer(i)
            } else {
                LuaValue::Number(n.as_f64().unwrap_or_default())
            }
        }
        Value::String(s) => LuaValue::String(lua.create_string(s.as_str())?),
        Value::Array(items) => {
            let table = lua.create_table()?;
            for (index, item) in items.iter().enumerate() {
                table.raw_set(index + 1, json_to_lua(lua, item)?)?;
            }
            LuaValue::Table(table)
        }
        Value::Object(map) => {
            let table = lua.create_table()?;
            for (key, item) in map {
                table.raw_set(key.as_str(), json_to_lua(lua, item)?)?;
            }
            LuaValue::Table(table)
        }
    };
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_names_split_on_the_first_dot() {
        assert_eq!(
            split_event_name("serial.data_received"),
            Some(("serial".into(), "data_received".into()))
        );
        assert_eq!(split_event_name("a.b.c"), Some(("a".into(), "b.c".into())));
        assert_eq!(split_event_name("nodot"), None);
        assert_eq!(split_event_name(".leading"), None);
        assert_eq!(split_event_name("trailing."), None);
    }

    #[test]
    fn script_errors_extract_path_and_line() {
        let err = mlua::Error::RuntimeError(
            "[string \"app.lua\"]:12: attempt to index a nil value".into(),
        );
        assert_eq!(
            split_script_error(&err),
            ("app.lua".to_string(), 12, "attempt to index a nil value".to_string())
        );

        let err = mlua::Error::external("plain failure");
        assert_eq!(
            split_script_error(&err),
            ("<script>".to_string(), 0, "plain failure".to_string())
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    fn guard_panics_off_the_lua_thread() {
        let guard = LuaThreadGuard::new();
        guard.assert_owner(); // the owning thread always passes

        let other = std::thread::spawn(move || guard.assert_owner());
        let result = other.join();
        assert!(
            result.is_err(),
            "the guard must panic when used off the owning thread"
        );
    }
}
