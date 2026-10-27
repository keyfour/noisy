//! Phase 1 acceptance tests (see docs/IMPLEMENTATION_PLAN.md §Phase 1):
//! event exchange on the bus, registry lifecycle with clean shutdown and
//! per-module timeout, cross-module calls with deadlock rejection,
//! async-call delivery, and callback token routing.

use noisy_core::bus::core_events;
use noisy_core::module::{Command, LogLevel, ModuleContext, ModuleDescriptor, NoisyModule};
use noisy_core::registry::{ModuleRegistry, RegistryError};
use noisy_core::{ModuleError, NoisyEvent};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;

const REGULAR_TIMEOUT: Duration = Duration::from_secs(2);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(50);

// ──────────────────────────── mock modules ────────────────────────────────

/// Emits `blip.tick` on a fixed interval until shutdown; answers `count`
/// with the number of ticks emitted so far.
struct BlipEmitter {
    interval: Duration,
}

impl NoisyModule for BlipEmitter {
    fn descriptor(&self) -> ModuleDescriptor {
        ModuleDescriptor::new("blip").signals(["tick"]).method("count")
    }

    fn start(self: Box<Self>, mut ctx: ModuleContext) -> JoinHandle<()> {
        tokio::spawn(async move {
            let interval = self.interval;
            let shutdown = ctx.shutdown.clone();
            let mut count: u64 = 0;
            loop {
                tokio::select! {
                    Some(cmd) = ctx.commands.recv() => {
                        if let Command::MethodCall(call) = cmd {
                            let method = call.method().to_string();
                            match method.as_str() {
                                "count" => call.reply(Ok(json!({ "count": count }))),
                                other => call.reply(Err(ModuleError::unknown_method("blip", other))),
                            }
                        }
                    }
                    _ = tokio::time::sleep(interval) => {
                        count += 1;
                        ctx.host.emit("tick", json!({ "n": count }));
                    }
                    _ = shutdown.cancelled() => break,
                }
            }
            ctx.drain().await;
        })
    }
}

/// Counts incoming `blip.*` events via a connected slot; answers `count`.
struct BlipCounter {
    count: Arc<AtomicU64>,
}

impl NoisyModule for BlipCounter {
    fn descriptor(&self) -> ModuleDescriptor {
        ModuleDescriptor::new("counter")
            .signals(["watched"])
            .method("count")
            .depends(["blip"])
    }

    fn start(self: Box<Self>, mut ctx: ModuleContext) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut stream = ctx.host.on("blip.");
            let shutdown = ctx.shutdown.clone();
            loop {
                tokio::select! {
                    Some(cmd) = ctx.commands.recv() => {
                        if let Command::MethodCall(call) = cmd {
                            let method = call.method().to_string();
                            match method.as_str() {
                                "count" => {
                                    let n = self.count.load(Ordering::SeqCst);
                                    call.reply(Ok(json!({ "count": n })));
                                }
                                other => call.reply(Err(ModuleError::unknown_method("counter", other))),
                            }
                        }
                    }
                    Some(_event) = stream.next() => {
                        self.count.fetch_add(1, Ordering::SeqCst);
                    }
                    _ = shutdown.cancelled() => break,
                }
            }
            ctx.drain().await;
        })
    }
}

/// Replies to `ping` but is deliberately slow to shut down.
struct SlowDrainer;

impl NoisyModule for SlowDrainer {
    fn descriptor(&self) -> ModuleDescriptor {
        ModuleDescriptor::new("slow").method("ping")
    }

    fn start(self: Box<Self>, mut ctx: ModuleContext) -> JoinHandle<()> {
        tokio::spawn(async move {
            let shutdown = ctx.shutdown.clone();
            loop {
                tokio::select! {
                    Some(cmd) = ctx.commands.recv() => {
                        if let Command::MethodCall(call) = cmd {
                            let method = call.method().to_string();
                            match method.as_str() {
                                "ping" => call.reply(Ok(json!({ "pong": true }))),
                                other => call.reply(Err(ModuleError::unknown_method("slow", other))),
                            }
                        }
                    }
                    _ = shutdown.cancelled() => {
                        // Simulates a module that ignores cancellation briefly.
                        tokio::time::sleep(Duration::from_millis(150)).await;
                        break;
                    }
                }
            }
            ctx.drain().await;
        })
    }
}

/// Module `a`: `ping { depth }` — synchronous hop into `b` until depth 0.
/// A synchronous hop back into a module already waiting somewhere in this
/// caller's chain must be rejected by the core's reentrance rule.
struct PingPongA;

impl NoisyModule for PingPongA {
    fn descriptor(&self) -> ModuleDescriptor {
        ModuleDescriptor::new("a").method_params("ping", ["depth"])
    }

    fn start(self: Box<Self>, mut ctx: ModuleContext) -> JoinHandle<()> {
        tokio::spawn(async move {
            let shutdown = ctx.shutdown.clone();
            loop {
                tokio::select! {
                    Some(cmd) = ctx.commands.recv() => {
                        if let Command::MethodCall(call) = cmd {
                            let method = call.method().to_string();
                            match method.as_str() {
                                "ping" => {
                                    let depth = call.params()["depth"].as_u64().unwrap_or(0);
                                    let result: Result<Value, ModuleError> = if depth == 0 {
                                        Ok(json!({ "done": true }))
                                    } else {
                                        ctx.host
                                            .call("b", "ping", json!({ "depth": depth - 1 }), REGULAR_TIMEOUT)
                                            .await
                                    };
                                    call.reply(result);
                                }
                                other => call.reply(Err(ModuleError::unknown_method("a", other))),
                            }
                        }
                    }
                    _ = shutdown.cancelled() => break,
                }
            }
            ctx.drain().await;
        })
    }
}

/// Module `b`: the mirror of `a`.
struct PingPongB;

impl NoisyModule for PingPongB {
    fn descriptor(&self) -> ModuleDescriptor {
        ModuleDescriptor::new("b").method_params("ping", ["depth"])
    }

    fn start(self: Box<Self>, mut ctx: ModuleContext) -> JoinHandle<()> {
        tokio::spawn(async move {
            let shutdown = ctx.shutdown.clone();
            loop {
                tokio::select! {
                    Some(cmd) = ctx.commands.recv() => {
                        if let Command::MethodCall(call) = cmd {
                            let method = call.method().to_string();
                            match method.as_str() {
                                "ping" => {
                                    let depth = call.params()["depth"].as_u64().unwrap_or(0);
                                    let result: Result<Value, ModuleError> = if depth == 0 {
                                        Ok(json!({ "done": true }))
                                    } else {
                                        ctx.host
                                            .call("a", "ping", json!({ "depth": depth - 1 }), REGULAR_TIMEOUT)
                                            .await
                                    };
                                    call.reply(result);
                                }
                                other => call.reply(Err(ModuleError::unknown_method("b", other))),
                            }
                        }
                    }
                    _ = shutdown.cancelled() => break,
                }
            }
            ctx.drain().await;
        })
    }
}

/// Module `x`: `fire { text }` — calls `y.echo` asynchronously; when the
/// async reply lands on its command stream it emits `x.async_reply`.
struct AsyncEchoX;

impl NoisyModule for AsyncEchoX {
    fn descriptor(&self) -> ModuleDescriptor {
        ModuleDescriptor::new("x").signals(["async_reply"]).method_params("fire", ["text"])
    }

    fn start(self: Box<Self>, mut ctx: ModuleContext) -> JoinHandle<()> {
        tokio::spawn(async move {
            let shutdown = ctx.shutdown.clone();
            loop {
                tokio::select! {
                    Some(cmd) = ctx.commands.recv() => {
                        match cmd {
                            Command::MethodCall(call) => match call.method().to_string().as_str() {
                                "fire" => {
                                    let text = call.params()["text"].as_str().unwrap_or("").to_string();
                                    let call_id = ctx
                                        .host
                                        .call_async("y", "echo", json!({ "text": text }))
                                        .expect("y is loaded");
                                    call.reply(Ok(json!({ "call_id": call_id })));
                                }
                                other => call.reply(Err(ModuleError::unknown_method("x", other))),
                            },
                            Command::AsyncReply { call_id, result } => {
                                ctx.host.emit(
                                    "async_reply",
                                    json!({ "call_id": call_id, "result": result.ok() }),
                                );
                            }
                        }
                    }
                    _ = shutdown.cancelled() => break,
                }
            }
            ctx.drain().await;
        })
    }
}

/// Module `y`: echoes its parameters back.
struct AsyncEchoY;

impl NoisyModule for AsyncEchoY {
    fn descriptor(&self) -> ModuleDescriptor {
        ModuleDescriptor::new("y").method_params("echo", ["text"])
    }

    fn start(self: Box<Self>, mut ctx: ModuleContext) -> JoinHandle<()> {
        tokio::spawn(async move {
            let shutdown = ctx.shutdown.clone();
            loop {
                tokio::select! {
                    Some(cmd) = ctx.commands.recv() => {
                        if let Command::MethodCall(call) = cmd {
                            match call.method().to_string().as_str() {
                                "echo" => {
                                    let params = call.params().clone();
                                    call.reply(Ok(params));
                                }
                                other => call.reply(Err(ModuleError::unknown_method("y", other))),
                            }
                        }
                    }
                    _ = shutdown.cancelled() => break,
                }
            }
            ctx.drain().await;
        })
    }
}

/// Method `fire { token, value }` invokes a language-native callback token.
struct CallbackUser;

impl NoisyModule for CallbackUser {
    fn descriptor(&self) -> ModuleDescriptor {
        ModuleDescriptor::new("cbuser").method_params("fire", ["token", "value"])
    }

    fn start(self: Box<Self>, mut ctx: ModuleContext) -> JoinHandle<()> {
        tokio::spawn(async move {
            let shutdown = ctx.shutdown.clone();
            loop {
                tokio::select! {
                    Some(cmd) = ctx.commands.recv() => {
                        if let Command::MethodCall(call) = cmd {
                            match call.method().to_string().as_str() {
                                "fire" => {
                                    let token = call.params()["token"].as_u64().unwrap_or(0);
                                    let value = call.params()["value"].clone();
                                    let invoked = ctx.host.invoke_callback(token, value);
                                    call.reply(Ok(json!({ "invoked": invoked })));
                                }
                                other => call.reply(Err(ModuleError::unknown_method("cbuser", other))),
                            }
                        }
                    }
                    _ = shutdown.cancelled() => break,
                }
            }
            ctx.drain().await;
        })
    }
}

/// A module that receives but never replies.
struct Deaf;

impl NoisyModule for Deaf {
    fn descriptor(&self) -> ModuleDescriptor {
        ModuleDescriptor::new("deaf").method("stall")
    }

    fn start(self: Box<Self>, mut ctx: ModuleContext) -> JoinHandle<()> {
        tokio::spawn(async move {
            let shutdown = ctx.shutdown.clone();
            // Calls are parked here for later analysis and never replied to
            // while parked — the caller must hit its own timeout.
            let mut parked: Vec<noisy_core::MethodCall> = Vec::new();            loop {
                tokio::select! {
                    Some(cmd) = ctx.commands.recv() => {
                        if let Command::MethodCall(call) = cmd {
                            if call.method() == "stall" {
                                parked.push(call); // holds the reply channel open
                            } else {
                                let method = call.method().to_string();
                                call.reply(Err(ModuleError::unknown_method("deaf", method)));
                            }
                        }
                    }
                    _ = shutdown.cancelled() => break,
                }
            }
            drop(parked);
            ctx.drain().await;
        })
    }
}

/// Logs at one level when poked.
struct Logger;

impl NoisyModule for Logger {
    fn descriptor(&self) -> ModuleDescriptor {
        ModuleDescriptor::new("logger").method("poke")
    }

    fn start(self: Box<Self>, mut ctx: ModuleContext) -> JoinHandle<()> {
        tokio::spawn(async move {
            let shutdown = ctx.shutdown.clone();
            loop {
                tokio::select! {
                    Some(cmd) = ctx.commands.recv() => {
                        if let Command::MethodCall(call) = cmd {
                            match call.method().to_string().as_str() {
                                "poke" => {
                                    ctx.host
                                        .log(LogLevel::Warn, "something odd", Some(json!({ "id": 3 })));
                                    call.reply(Ok(json!({})));
                                }
                                other => call.reply(Err(ModuleError::unknown_method("logger", other))),
                            }
                        }
                    }
                    _ = shutdown.cancelled() => break,
                }
            }
            ctx.drain().await;
        })
    }
}

// ───────────────────────────── helpers ────────────────────────────────────

/// Loads modules on a fresh registry; returns the registry and a bus tap.
async fn setup(
    modules: Vec<Box<dyn NoisyModule>>,
) -> (ModuleRegistry, tokio::sync::broadcast::Receiver<NoisyEvent>) {
    let mut registry = ModuleRegistry::new();
    let rx = registry.bus().subscribe();
    for module in modules {
        registry.load(module, json!({})).await.expect("module loads");
    }
    (registry, rx)
}

// ──────────────────────────── the tests ───────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn modules_exchange_events_and_shut_down_cleanly() {
    let (mut registry, mut rx) =
        setup(vec![Box::new(BlipEmitter { interval: Duration::from_millis(20) }), Box::new(BlipCounter { count: Arc::new(AtomicU64::new(0)) })])
            .await;

    // the core emitted the lifecycle event for the first loaded module
    let started = rx.recv().await.unwrap();
    assert_eq!(started.name, core_events::SERVICE_STARTED);
    assert_eq!(started.payload["module"], json!("blip"));

    // cross-module events flow: the counter is fed by the emitter's slots
    tokio::time::sleep(Duration::from_millis(90)).await;
    let n = registry
        .call("counter", "count", json!({}), REGULAR_TIMEOUT)
        .await
        .expect("count replies");
    assert!(n["count"].as_u64().unwrap() >= 2, "counter saw blip ticks");

    // clean shutdown under token cancel: both join inside the timeout
    let report = tokio::time::timeout(
        Duration::from_secs(5),
        registry.shutdown(SHUTDOWN_TIMEOUT),
    )
    .await
    .expect("shutdown does not hang");
    assert_eq!(report.stragglers.len(), 0, "no stragglers expected");
    assert!(report.stopped.contains(&"blip".to_string()));
    assert!(report.stopped.contains(&"counter".to_string()));

    // service_stopped events were emitted (HashMap order is arbitrary, so
    // skim the buffered lifecycle traffic until we see one)
    let mut stopped = rx.recv().await.unwrap();
    while stopped.name != core_events::SERVICE_STOPPED {
        stopped = rx.recv().await.unwrap();
    }
    assert_eq!(stopped.name, core_events::SERVICE_STOPPED);
}

#[tokio::test(flavor = "multi_thread")]
async fn slow_module_is_force_joined_and_reported() {
    let (mut registry, _) = setup(vec![Box::new(SlowDrainer)]).await;

    let pong = registry
        .call("slow", "ping", json!({}), REGULAR_TIMEOUT)
        .await
        .expect("ping replies while running");
    assert_eq!(pong["pong"], true);

    let report = registry.shutdown(Duration::from_millis(50)).await;
    assert_eq!(report.stopped.len(), 0);
    assert_eq!(report.stragglers, vec!["slow".to_string()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn duplicate_module_names_are_rejected() {
    struct AnotherSlow;
    impl NoisyModule for AnotherSlow {
        fn descriptor(&self) -> ModuleDescriptor {
            ModuleDescriptor::new("slow")
        }
        fn start(self: Box<Self>, _ctx: ModuleContext) -> JoinHandle<()> {
            tokio::spawn(async {})
        }
    }

    let (mut registry, _) = setup(vec![Box::new(SlowDrainer)]).await;
    let err = registry
        .load(Box::new(AnotherSlow), json!({}))
        .await
        .expect_err("duplicate names are rejected");
    assert!(matches!(err, RegistryError::Duplicate(ref name) if name == "slow"));
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_dependency_is_rejected() {
    struct NeedsGhost;
    impl NoisyModule for NeedsGhost {
        fn descriptor(&self) -> ModuleDescriptor {
            ModuleDescriptor::new("needy").depends(["ghost"])
        }
        fn start(self: Box<Self>, _ctx: ModuleContext) -> JoinHandle<()> {
            tokio::spawn(async {})
        }
    }

    let mut registry = ModuleRegistry::new();
    let err = registry
        .load(Box::new(NeedsGhost), json!({}))
        .await
        .expect_err("depends on unloaded module");
    assert!(matches!(err, RegistryError::MissingDependency { ref depends_on, .. } if depends_on == "ghost"));
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_methods_and_unloaded_modules_return_precise_errors() {
    let (registry, _) = setup(vec![Box::new(SlowDrainer)]).await;

    let err = registry
        .call("slow", "nope", json!({}), REGULAR_TIMEOUT)
        .await
        .expect_err("unknown method");
    assert!(
        matches!(err, ModuleError::UnknownMethod { ref module, ref method } if module == "slow" && method == "nope"),
        "got: {err:?}"
    );

    let err = registry
        .call("ghost", "ping", json!({}), REGULAR_TIMEOUT)
        .await
        .expect_err("unloaded module");
    assert!(matches!(err, ModuleError::ModuleUnavailable(ref m) if m == "ghost"));
}

#[tokio::test(flavor = "multi_thread")]
async fn call_timeout_fires_when_module_never_replies() {
    let (mut registry, _) = setup(vec![Box::new(Deaf)]).await;

    let err = tokio::time::timeout(
        Duration::from_secs(5),
        registry.call("deaf", "stall", json!({}), Duration::from_millis(60)),
    )
    .await
    .expect("the router returns in time")
    .expect_err("timeout error");
    assert!(matches!(err, ModuleError::Timeout { timeout_ms: 60 }));

    registry.shutdown(SHUTDOWN_TIMEOUT).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mutual_recursion_through_sync_calls_is_rejected() {
    let (mut registry, _) = setup(vec![Box::new(PingPongA), Box::new(PingPongB)]).await;

    let err = tokio::time::timeout(
        Duration::from_secs(5),
        registry.call("a", "ping", json!({ "depth": 3 }), REGULAR_TIMEOUT),
    )
    .await
    .expect("the rejection arrives instead of spinning")
    .expect_err("rejected");

    assert!(
        matches!(err, ModuleError::Reentrant { ref from, ref to } if from == "b" && to == "a"),
        "precise re-enter error, got: {err:?}"
    );

    registry.shutdown(SHUTDOWN_TIMEOUT).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn call_async_completes_and_delivers_the_reply() {
    let (mut registry, mut rx) = setup(vec![Box::new(AsyncEchoX), Box::new(AsyncEchoY)]).await;

    let ack = registry
        .call("x", "fire", json!({ "text": "hello" }), REGULAR_TIMEOUT)
        .await
        .expect("fire replies with the call id");
    let call_id = ack["call_id"].as_u64().expect("call id present");

    // the reply arrives on x's command stream and x re-emits it as a signal
    // (skim buffered lifecycle events first)
    let mut event = rx.recv().await.unwrap();
    while event.name != "x.async_reply" {
        event = rx.recv().await.unwrap();
    }
    assert_eq!(event.payload["call_id"], call_id);
    assert_eq!(event.payload["result"]["text"], "hello");

    registry.shutdown(SHUTDOWN_TIMEOUT).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn callback_tokens_round_trip() {
    let (mut registry, _) = setup(vec![Box::new(CallbackUser)]).await;

    let mut rx = registry.callbacks().register(42);

    let result = registry
        .call(
            "cbuser",
            "fire",
            json!({ "token": 42, "value": { "n": 5 } }),
            REGULAR_TIMEOUT,
        )
        .await
        .expect("fire replies");
    assert_eq!(result["invoked"], true);

    let envelope = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("round trip completes")
        .expect("envelope");
    assert_eq!(envelope.token, 42);
    assert_eq!(envelope.payload["n"], 5);

    let probe = registry
        .call(
            "cbuser",
            "fire",
            json!({ "token": 999, "value": {} }),
            REGULAR_TIMEOUT,
        )
        .await
        .expect("fire replies");
    assert_eq!(probe["invoked"], false, "unknown tokens report failure");

    registry.shutdown(SHUTDOWN_TIMEOUT).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn host_log_events_appear_on_the_bus() {
    let (mut registry, mut rx) = setup(vec![Box::new(Logger)]).await;

    registry
        .call("logger", "poke", json!({}), REGULAR_TIMEOUT)
        .await
        .expect("poke ok");

    let mut event = rx.recv().await.unwrap();
    while event.name != "log.entry" {
        event = rx.recv().await.unwrap();
    }
    assert_eq!(event.source, "log");
    assert_eq!(event.payload["module"], "logger");
    assert_eq!(event.payload["level"], "warn");
    assert_eq!(event.payload["msg"], "something odd");
    assert_eq!(event.payload["fields"]["id"], 3);
    assert!(event.payload["timestamp"].as_u64().unwrap_or(0) > 0);

    registry.shutdown(SHUTDOWN_TIMEOUT).await;
}
