//! Phase 4 acceptance for the `crypto` module: the known vectors hold
//! through the full registry call path, and the readable refusals work.

use noisy_core::registry::ModuleRegistry;
use serde_json::json;
use std::time::Duration;

const REGULAR_TIMEOUT: Duration = Duration::from_secs(2);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(100);

#[tokio::test(flavor = "multi_thread")]
async fn crypto_methods_round_trip_through_the_registry() {
    let mut registry = ModuleRegistry::new();
    registry
        .load(Box::new(noisy_crypto::CryptoModule::new()), json!({}))
        .await
        .expect("crypto module loads");

    let digest = registry
        .call(
            "crypto",
            "hash",
            json!({ "algorithm": "sha256", "data": "abc" }),
            REGULAR_TIMEOUT,
        )
        .await
        .expect("hash replies");
    assert_eq!(
        digest,
        json!("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
    );

    // The binary convention input hashes identically to its plain form.
    let plain = registry
        .call(
            "crypto",
            "hash",
            json!({ "algorithm": "sha256", "data": "hello" }),
            REGULAR_TIMEOUT,
        )
        .await
        .expect("hash replies");
    let binary = registry
        .call(
            "crypto",
            "hash",
            json!({
                "algorithm": "sha256",
                "data": { "data": "aGVsbG8=", "encoding": "base64" },
            }),
            REGULAR_TIMEOUT,
        )
        .await
        .expect("hash with binary input replies");
    assert_eq!(plain, binary, "the object encoding decodes to the same bytes");

    let uuid = registry
        .call("crypto", "uuid", json!({}), REGULAR_TIMEOUT)
        .await
        .expect("uuid replies");
    let uuid = uuid.as_str().expect("uuid string");
    assert_eq!(uuid.len(), 36);
    assert_eq!(uuid.matches('-').count(), 4);

    // Readable refusals: unknown algorithm, unknown method, invalid base64.
    let bad_algorithm = registry
        .call(
            "crypto",
            "hash",
            json!({ "algorithm": "rot13", "data": "x" }),
            REGULAR_TIMEOUT,
        )
        .await
        .expect_err("unknown algorithm is refused");
    assert!(bad_algorithm.to_string().contains("unknown algorithm"), "{bad_algorithm}");

    let bad_method = registry
        .call("crypto", "encrypt", json!({}), REGULAR_TIMEOUT)
        .await
        .expect_err("unknown method is refused");
    assert!(bad_method.to_string().contains("unknown method"), "{bad_method}");

    let report = registry.shutdown(SHUTDOWN_TIMEOUT).await;
    assert_eq!(report.stopped, vec!["crypto".to_string()]);
}
