//! Phase 4 acceptance for the `log` module: method calls publish the
//! canonical `log.entry` event; `set_level` validates.

use noisy_core::registry::ModuleRegistry;
use noisy_log::LogModule;
use noisy_core::NoisyEvent;
use serde_json::json;
use std::time::Duration;

const REGULAR_TIMEOUT: Duration = Duration::from_secs(2);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(100);

#[tokio::test(flavor = "multi_thread")]
async fn log_methods_publish_canonical_entry_events() {
    let mut registry = ModuleRegistry::new();
    let mut rx = registry.bus().subscribe();
    registry
        .load(Box::new(LogModule::new()), json!({}))
        .await
        .expect("log module loads");

    registry
        .call(
            "log",
            "warn",
            json!({ "msg": "something odd", "fields": { "id": 3 } }),
            REGULAR_TIMEOUT,
        )
        .await
        .expect("warn replies");

    let event = next_named(&mut rx, "log.entry").await.expect("entry event");
    assert_eq!(event.source, "log");
    assert_eq!(event.payload["module"], "log", "the log module itself logged");
    assert_eq!(event.payload["level"], "warn");
    assert_eq!(event.payload["msg"], "something odd");
    assert_eq!(event.payload["fields"]["id"], 3);
    assert!(event.payload["timestamp"].as_u64().is_some());

    // An unknown method and an invalid level surface readable errors.
    let missing = registry
        .call("log", "trace", json!({ "msg": "nope" }), REGULAR_TIMEOUT)
        .await
        .expect_err("unknown method");
    assert!(missing.to_string().contains("unknown method"), "{missing}");

    let bad_level = registry
        .call("log", "set_level", json!({ "level": "chatty" }), REGULAR_TIMEOUT)
        .await
        .expect_err("invalid level");
    assert!(bad_level.to_string().contains("unknown level"), "{bad_level}");

    registry
        .call("log", "set_level", json!({ "level": "error" }), REGULAR_TIMEOUT)
        .await
        .expect("valid level");

    let report = registry.shutdown(SHUTDOWN_TIMEOUT).await;
    assert_eq!(report.stopped, vec!["log".to_string()], "clean stop");
}

async fn next_named(
    rx: &mut tokio::sync::broadcast::Receiver<NoisyEvent>,
    name: &str,
) -> Option<NoisyEvent> {
    while let Ok(event) = rx.recv().await {
        if event.name == name {
            return Some(event);
        }
    }
    None
}
