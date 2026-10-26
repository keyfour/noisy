//! Plugin loader conformance against the real `.so` (specification §6.2):
//! ABI validation, descriptor validation, command round trips on the
//! registry, event delivery, the async-reply path, and clean shutdown.
//!
//! The test module is the workspace's `testplugin` cdylib; `cargo test`
//! builds the whole workspace, so `target/debug/libtestplugin.so` exists
//! by the time this runs.

use noisy_core::bus::NoisyEvent;
use noisy_core::registry::ModuleRegistry;
use noisy_ffi::plugin::LoadError;
use noisy_ffi::PluginModule;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(2);

/// A fresh registry with the `probe` plugin loaded (no tick).
async fn probe_registry() -> ModuleRegistry {
    ensure_plugins_built();
    let mut registry = ModuleRegistry::new();
    let module =
        PluginModule::load(&plugin_so("testplugin"), 0).expect("probe plugin loads");
    registry
        .load(Box::new(module), json!({}))
        .await
        .expect("probe plugin starts");
    registry
}

/// `target/debug/lib<name>.so`, relative to this crate.
fn plugin_so(name: &str) -> PathBuf {
    workspace_root()
        .join("target/debug")
        .join(format!("lib{name}.so"))
}

/// Builds the workspace's cdylibs once per test process: `cargo test` does
/// not produce cdylib artifacts on its own.
fn ensure_plugins_built() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let missing =
            !plugin_so("testplugin").is_file() || !plugin_so("badplugin").is_file();
        if !missing {
            return;
        }
        let output = std::process::Command::new("cargo")
            .args(["build", "-p", "testplugin", "-p", "badplugin"])
            .current_dir(workspace_root())
            .output()
            .expect("cargo build spawns");
        assert!(
            output.status.success(),
            "building the fixture cdylibs failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    });
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/noisy_ffi sits two levels under the workspace")
        .to_path_buf()
}

/// Waits until one additional bus receiver is active: the plugin's
/// create-time event forwarder (the test's own receiver makes the
/// expected count 2). Without this, an event emitted before the
/// forwarder subscribed would be simply lost.
async fn wait_for_forwarder(bus: &noisy_core::EventBus) {
    let deadline = std::time::Instant::now() + TIMEOUT;
    while bus.receiver_count() < 2 {
        assert!(
            std::time::Instant::now() < deadline,
            "the plugin's event forwarder never subscribed"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn descriptor_from_the_plugin_descriptor() {
    let registry = probe_registry().await;
    let descriptor = registry
        .descriptor("probe")
        .expect("probe registered under its descriptor name");
    assert_eq!(descriptor.version, "0.1.0");
    assert!(descriptor.signals.contains(&"pinged".to_string()));
    assert!(
        descriptor.methods.iter().any(|m| m.name == "echo"),
        "descriptor declares echo"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn command_round_trip() {
    let registry = probe_registry().await;
    let reply = registry
        .call("probe", "echo", json!({ "text": "hi" }), TIMEOUT)
        .await
        .expect("echo answers");
    assert_eq!(reply["echo"], json!("hi"));
    // The module exercises its host context: the reply carries now_ms.
    assert!(reply["at"].as_u64().is_some(), "echo carries a timestamp");
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_method_fails_readably() {
    let registry = probe_registry().await;
    let err = registry
        .call("probe", "nope", json!({}), TIMEOUT)
        .await
        .expect_err("nope is not a method");
    assert!(
        err.to_string().contains("no such method"),
        "readable failure, got: {err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn events_reach_the_plugin() {
    let registry = probe_registry().await;
    let mut rx = registry.bus().subscribe();
    wait_for_forwarder(registry.bus()).await;

    // The probe subscribed to `app.` at create; the relayed event comes
    // back on the bus.
    registry
        .bus()
        .emit(NoisyEvent::new("app", "poke", json!({ "n": 1 })));
    let mut relayed = false;
    for _ in 0..100 {
        match tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
            Ok(Ok(event)) if event.name == "probe.pinged" => {
                relayed = true;
                break;
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            _ => {}
        }
    }
    assert!(relayed, "the plugin relays received events as probe.pinged");
    // And the module's own counter proves noisy_module_event was called.
    let count = registry
        .call("probe", "events", json!({}), TIMEOUT)
        .await
        .expect("events answers");
    assert_eq!(count["events"], json!(1));
}

#[tokio::test(flavor = "multi_thread")]
async fn async_reply_arrives_through_the_command_stream() {
    let registry = probe_registry().await;
    // call_self makes the plugin issue `call_async` on itself; the reply
    // comes back to the plugin as Command::AsyncReply, which the actor
    // forwards as `noisy_module_reply`.
    registry
        .call("probe", "call_self", json!({}), TIMEOUT)
        .await
        .expect("call_self answers immediately");
    let deadline = std::time::Instant::now() + TIMEOUT;
    loop {
        let last = registry
            .call("probe", "last_call", json!({}), TIMEOUT)
            .await
            .expect("last_call answers");
        if last.is_null() {
            assert!(
                std::time::Instant::now() < deadline,
                "async reply never arrived"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
            continue;
        }
        assert_eq!(last["value"]["echo"], json!("self"));
        break;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_stops_the_plugin_cleanly() {
    let mut registry = probe_registry().await;
    let report = registry.shutdown(TIMEOUT).await;
    assert!(
        report.stopped.contains(&"probe".to_string()),
        "probe stopped: {report:?}"
    );
    assert!(report.stragglers.is_empty(), "no stragglers: {report:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_abi_plugin_is_rejected_readably() {
    ensure_plugins_built();
    let err = match PluginModule::load(&plugin_so("badplugin"), 0) {
        Err(err) => err,
        Ok(_) => panic!("badplugin must not load"),
    };
    match err {
        LoadError::AbiMismatch(path, found, expected) => {
            assert!(path.contains("libbadplugin.so"), "path in message: {path}");
            assert_eq!(found, "2");
            assert_eq!(expected, "1");
        }
        other => panic!("expected AbiMismatch, got: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_plugin_is_reported_readable() {
    let err = match PluginModule::load(&plugin_so("nonexistent"), 0) {
        Err(err) => err,
        Ok(_) => panic!("missing library must not load"),
    };
    assert!(
        err.to_string().contains("cannot load"),
        "readable failure, got: {err}"
    );
}
