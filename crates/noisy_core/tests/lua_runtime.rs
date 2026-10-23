//! Phase 2 acceptance tests (see docs/IMPLEMENTATION_PLAN.md §Phase 2):
//! the 100-event subscribe/unsubscribe/emit harness, script error
//! isolation, descriptor-generated namespaces with argument packing and
//! callback tokens, the `noisy` global, and the embedded stdlib loader.

use noisy_core::lua_runtime::{LuaHandle, LuaRuntime};
use noisy_core::module::{Command, ModuleContext, ModuleDescriptor, NoisyModule};
use noisy_core::registry::ModuleRegistry;
use noisy_core::{CallbackToken, EventStream, ModuleError, NoisyEvent};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;
use tokio::task::JoinHandle;

const TIMEOUT: Duration = Duration::from_secs(5);

// ──────────────────────────── mock module ─────────────────────────────────

/// `gadget`: one mock module exercising every binding shape — method calls
/// with and without parameters, events, and callback tokens.
struct Gadget;

impl NoisyModule for Gadget {
    fn descriptor(&self) -> ModuleDescriptor {
        ModuleDescriptor::new("gadget")
            .signals(["tick", "boom"])
            .method_params("watch", ["name", "options", "callback"])
            .method_params("trigger", ["name", "payload"])
            .method_params("spray", ["count"])
            .method_params("buzzer", ["count"])
            .method("boom")
            .method("ping")
    }

    fn start(self: Box<Self>, mut ctx: ModuleContext) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut watches: HashMap<String, (Value, CallbackToken)> = HashMap::new();
            let shutdown = ctx.shutdown.clone();
            loop {
                tokio::select! {
                    Some(cmd) = ctx.commands.recv() => {
                        let Command::MethodCall(call) = cmd else { continue };
                        let method = call.method().to_string();
                        let params = call.params().clone();
                        match method.as_str() {
                            "watch" => {
                                let name = params["name"].as_str().unwrap_or_default().to_string();
                                let token = params["callback"].as_u64().unwrap_or_default();
                                watches.insert(name.clone(), (params["options"].clone(), token));
                                call.reply(Ok(json!({
                                    "watching": name,
                                    "opt": params["options"]["opt"],
                                })));
                            }
                            "trigger" => {
                                let name = params["name"].as_str().unwrap_or_default();
                                let payload = params["payload"].clone();
                                let invoked = match watches.get(name) {
                                    Some((_, token)) => ctx.host.invoke_callback(*token, payload),
                                    None => false,
                                };
                                call.reply(Ok(json!({ "invoked": invoked })));
                            }
                            "spray" => {
                                let count = params["count"].as_u64().unwrap_or_default();
                                for i in 1..=count {
                                    ctx.host.emit(&format!("t{i}"), json!({ "n": i }));
                                }
                                call.reply(Ok(json!({ "sprayed": count })));
                            }
                            "buzzer" => {
                                let count = params["count"].as_u64().unwrap_or_default();
                                for i in 1..=count {
                                    ctx.host.emit("tick", json!({ "n": i }));
                                }
                                call.reply(Ok(json!({ "buzzed": count })));
                            }
                            "boom" => {
                                ctx.host.emit("boom", json!({}));
                                call.reply(Ok(json!({ "boom": true })));
                            }
                            "ping" => call.reply(Ok(json!({ "pong": true }))),
                            other => call.reply(Err(ModuleError::unknown_method("gadget", other))),
                        }
                    }
                    _ = shutdown.cancelled() => break,
                }
            }
            ctx.drain().await;
        })
    }
}

// ──────────────────────────── harness ─────────────────────────────────────

async fn harness() -> (ModuleRegistry, LuaRuntime, EventStream) {
    let mut registry = ModuleRegistry::new();
    registry
        .load(Box::new(Gadget), json!({}))
        .await
        .expect("gadget loads");
    let tap = registry.bus().stream("*", "tap");
    let runtime = LuaRuntime::start(registry.lua_deps());
    (registry, runtime, tap)
}

/// Polls an expression on the Lua thread until it satisfies `check`.
async fn until(lua: &LuaHandle, code: &str, mut check: impl FnMut(&Value) -> bool, what: &str) {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        if let Ok(value) = lua.eval(code).await {
            if check(&value) {
                return;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Reads events from the tap until one with `name` arrives.
async fn next_named(tap: &mut EventStream, name: &str) -> NoisyEvent {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let event = tokio::time::timeout_at(deadline, tap.next())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {name}"))
            .expect("tap stays open");
        if event.name == name {
            return event;
        }
    }
}

// ──────────────────────────── tests ───────────────────────────────────────

#[tokio::test]
async fn hundred_event_subscriptions_unsubscribe_and_lua_emit() {
    let (mut registry, runtime, mut tap) = harness().await;
    let lua = runtime.handle();

    lua.run_source(
        "app.lua",
        r#"
        ids = {}
        hits = {}
        for i = 1, 100 do
            ids[i] = noisy.on("gadget.t" .. i, function(event)
                hits[#hits + 1] = event.payload.n
                if event.source ~= "gadget" or event.kind ~= ("t" .. event.payload.n) then
                    error("bad event shape")
                end
            end)
        end
        "#,
    )
    .await
    .expect("script runs");

    // one hundred subscribed events, all delivered, in order, with the
    // payload crossing the JSON boundary intact
    registry
        .call("gadget", "spray", json!({ "count": 100 }), TIMEOUT)
        .await
        .expect("spray replies");
    until(&lua, "return #hits", |v| v == &json!(100), "100 hits").await;
    let hits = lua.eval("return hits").await.expect("hits array");
    let expected: Vec<Value> = (1..=100).map(|n| json!(n)).collect();
    assert_eq!(hits, Value::Array(expected));

    // unsubscribing cleanly stops delivery
    lua.eval("for _, id in ipairs(ids) do assert(noisy.off(id)) end")
        .await
        .expect("all offs succeed");
    registry
        .call("gadget", "spray", json!({ "count": 100 }), TIMEOUT)
        .await
        .expect("second spray replies");
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(lua.eval("return #hits").await.unwrap(), json!(100));

    // Lua-side events reach the bus
    lua.eval(r#"return noisy.emit("app.custom", { ok = true, n = 7 })"#)
        .await
        .expect("emit returns true");
    let event = next_named(&mut tap, "app.custom").await;
    assert_eq!(event.source, "app");
    assert_eq!(event.kind, "custom");
    assert_eq!(event.payload, json!({ "ok": true, "n": 7 }));

    registry.shutdown(TIMEOUT).await;
    runtime.join().expect("lua thread exits cleanly");
}

#[tokio::test]
async fn failing_handler_emits_script_error_and_the_loop_continues() {
    let (mut registry, runtime, mut tap) = harness().await;
    let lua = runtime.handle();

    lua.run_source(
        "app.lua",
        r#"
        ticks = 0
        noisy.on("gadget.tick", function(event)
            ticks = ticks + 1
            if event.payload.n == 2 then
                error("boom at handler")
            end
        end)
        "#,
    )
    .await
    .expect("script runs");

    registry
        .call("gadget", "buzzer", json!({ "count": 3 }), TIMEOUT)
        .await
        .expect("buzzer replies");

    // the loop survives the handler error and delivers everything else
    until(&lua, "return ticks", |v| v == &json!(3), "3 ticks").await;

    let error = next_named(&mut tap, "noisy.script_error").await;
    assert_eq!(error.payload["path"], "app.lua");
    assert!(error.payload["line"].as_u64().unwrap_or(0) > 0);
    assert!(
        error.payload["message"]
            .as_str()
            .unwrap_or_default()
            .contains("boom at handler"),
        "unexpected message: {}",
        error.payload["message"]
    );

    // and later events still flow
    registry
        .call("gadget", "buzzer", json!({ "count": 1 }), TIMEOUT)
        .await
        .expect("second buzzer replies");
    until(&lua, "return ticks", |v| v == &json!(4), "4th tick").await;

    registry.shutdown(TIMEOUT).await;
    runtime.join().expect("lua thread exits cleanly");
}

#[tokio::test]
async fn namespace_generator_round_trips_args_and_callbacks() {
    let (mut registry, runtime, _tap) = harness().await;
    let lua = runtime.handle();

    // (name, options, callback): positional packing of scalars and tables,
    // the function argument crossing as a callback token
    lua.run_source(
        "app.lua",
        r#"
        got = nil
        local reply = gadget.watch("sensor", { opt = 7 }, function(ev)
            got = ev
        end)
        packed = { watching = reply.watching, opt = reply.opt }
        gadget.trigger("sensor", { hello = "world", n = 42 })
        "#,
    )
    .await
    .expect("script runs");

    until(&lua, "return got", |v| v.is_object(), "callback fired").await;
    let got = lua.eval("return got").await.unwrap();
    assert_eq!(got, json!({ "hello": "world", "n": 42 }));

    let packed = lua.eval("return packed").await.unwrap();
    assert_eq!(packed, json!({ "watching": "sensor", "opt": 7 }));

    // a sync call with no parameters returns the JSON reply as a table
    let pong = lua.eval("return gadget.ping()").await.unwrap();
    assert_eq!(pong, json!({ "pong": true }));

    registry.shutdown(TIMEOUT).await;
    runtime.join().expect("lua thread exits cleanly");
}

#[tokio::test]
async fn method_calls_work_inside_handlers() {
    let (mut registry, runtime, _tap) = harness().await;
    let lua = runtime.handle();

    lua.run_source(
        "app.lua",
        r#"
        from_handler = nil
        noisy.on("gadget.tick", function()
            local reply = gadget.ping()
            from_handler = reply.pong
        end)
        "#,
    )
    .await
    .expect("script runs");

    registry
        .call("gadget", "buzzer", json!({ "count": 1 }), TIMEOUT)
        .await
        .expect("buzzer replies");
    until(&lua, "return from_handler", |v| v == &json!(true), "handler call").await;

    registry.shutdown(TIMEOUT).await;
    runtime.join().expect("lua thread exits cleanly");
}

#[tokio::test]
async fn noisy_use_validates_loaded_modules() {
    let (_registry, runtime, _tap) = harness().await;
    let lua = runtime.handle();

    let ok = lua.eval(r#"return noisy.use("gadget")"#).await.unwrap();
    assert_eq!(ok, json!(true));

    let err = lua
        .eval(r#"return noisy.use("ghost")"#)
        .await
        .expect_err("unknown module is rejected");
    assert!(err.contains("not loaded"), "unexpected error: {err}");
    assert!(err.contains("gadget"), "error lists loaded modules: {err}");

    runtime.join().expect("lua thread exits cleanly");
}

#[tokio::test]
async fn embedded_stdlib_and_version() {
    let (_registry, runtime, _tap) = harness().await;
    let lua = runtime.handle();

    let result = lua
        .eval(
            r#"
            local util = require("util")
            return { trim = util.trim("  noisy  "), starts = util.starts_with("noisy", "no") }
            "#,
        )
        .await
        .unwrap();
    assert_eq!(result, json!({ "trim": "noisy", "starts": true }));

    // require is cached through package.loaded (same table both times)
    let cached = lua
        .eval(r#"return require("util") == require("util")"#)
        .await
        .unwrap();
    assert_eq!(cached, json!(true));

    let missing = lua
        .eval(r#"require("ghost")"#)
        .await
        .expect_err("unknown require fails");
    assert!(missing.contains("not found"), "unexpected error: {missing}");

    let version = lua.eval("return noisy.version()").await.unwrap();
    assert_eq!(version, json!(env!("CARGO_PKG_VERSION")));

    runtime.join().expect("lua thread exits cleanly");
}

#[tokio::test]
async fn sandbox_removes_io_and_restricts_os() {
    let (_registry, runtime, _tap) = harness().await;
    let lua = runtime.handle();

    assert_eq!(lua.eval("return io").await.unwrap(), Value::Null);
    assert_eq!(lua.eval("return dofile").await.unwrap(), Value::Null);
    assert_eq!(lua.eval("return loadfile").await.unwrap(), Value::Null);

    for gone in ["execute", "exit", "getenv", "remove", "rename"] {
        assert_eq!(
            lua.eval(format!("return os.{gone}")).await.unwrap(),
            Value::Null,
            "os.{gone} must be removed"
        );
    }
    assert_eq!(
        lua.eval(r#"return type(os.clock)"#).await.unwrap(),
        json!("function")
    );

    runtime.join().expect("lua thread exits cleanly");
}
