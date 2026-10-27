//! Phase 4 acceptance for the `fs` module: the §4.3 surface round-trips
//! and the watch ordering test (a create/write sequence yields
//! `file_created` → `file_modified` in order; `unwatch` stops the flow).
//! Fixtures live under the gitignored `target/tmp` (`CARGO_TARGET_TMPDIR`).

use noisy_core::registry::ModuleRegistry;
use noisy_core::NoisyEvent;
use noisy_fs::FsModule;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;

const REGULAR_TIMEOUT: Duration = Duration::from_secs(2);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(100);

async fn boot(
    name: &str,
) -> (
    ModuleRegistry,
    PathBuf,
    tokio::sync::broadcast::Receiver<NoisyEvent>,
) {
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("fixture base created");

    let mut registry = ModuleRegistry::new();
    registry
        .load(Box::new(FsModule::new()), json!({}))
        .await
        .expect("fs module loads");
    let rx = registry.bus().subscribe();
    (registry, base, rx)
}

/// Events arrive in bus order: collect the next `fs.*` signal (skipping
/// unrelated events without discarding the ones the test still needs —
/// collectors like this one must not drop-ahead).
async fn next_fs_event(
    rx: &mut tokio::sync::broadcast::Receiver<NoisyEvent>,
    kind: &str,
) -> (String, NoisyEvent) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for fs.{kind}"
        );
        if let Ok(Ok(event)) = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await
        {
            if event.name == format!("fs.{kind}") {
                let path = event.payload["path"].as_str().unwrap_or_default().to_string();
                return (path, event);
            }
        }
    }
}

async fn call(registry: &mut ModuleRegistry, method: &str, params: Value) -> Value {
    registry
        .call("fs", method, params, REGULAR_TIMEOUT)
        .await
        .expect("method replies")
}#[tokio::test(flavor = "multi_thread")]
async fn surface_round_trips() {
    let (mut registry, base, rx) = boot("fs-surface").await;
    let _ = rx; // surface tests do not need the event stream

    let file = base.join("notes.txt");

    // write → read → append → read
    call(
        &mut registry,
        "write",
        json!({ "path": file.to_string_lossy(), "content": "first" }),
    )
    .await;
    assert_eq!(
        call(&mut registry, "read", json!({ "path": file.to_string_lossy() })).await,
        json!("first")
    );
    call(
        &mut registry,
        "append",
        json!({ "path": file.to_string_lossy(), "content": "/second" }),
    )
    .await;
    assert_eq!(
        call(&mut registry, "read", json!({ "path": file.to_string_lossy() })).await,
        json!("first/second")
    );
    assert_eq!(
        call(&mut registry, "exists", json!({ "path": file.to_string_lossy() })).await,
        json!(true)
    );

    // stat
    let stat = call(&mut registry, "stat", json!({ "path": file.to_string_lossy() })).await;
    assert_eq!(stat["is_file"], true);
    assert_eq!(stat["size"], 12);
    assert!(stat["modified"].as_u64().unwrap_or(0) > 0);

    // mkdir / list
    let subdir = base.join("entries");
    call(&mut registry, "mkdir", json!({ "path": subdir.to_string_lossy() })).await;
    call(
        &mut registry,
        "write",
        json!({ "path": subdir.join("a.txt").to_string_lossy(), "content": "a" }),
    )
    .await;
    call(
        &mut registry,
        "write",
        json!({ "path": subdir.join("b.bin").to_string_lossy(), "content": "b" }),
    )
    .await;
    assert_eq!(
        call(&mut registry, "list", json!({ "path": subdir.to_string_lossy() })).await,
        json!(["a.txt", "b.bin"])
    );
    let stat = call(&mut registry, "stat", json!({ "path": subdir.to_string_lossy() })).await;
    assert_eq!(stat["is_dir"], true);

    // copy / move
    let copied = base.join("copied.txt");
    call(
        &mut registry,
        "copy",
        json!({
            "src": file.to_string_lossy(),
            "dst": copied.to_string_lossy(),
        }),
    )
    .await;
    assert_eq!(
        call(&mut registry, "read", json!({ "path": copied.to_string_lossy() })).await,
        json!("first/second")
    );
    let moved = base.join("moved.txt");
    call(
        &mut registry,
        "move",
        json!({
            "src": copied.to_string_lossy(),
            "dst": moved.to_string_lossy(),
        }),
    )
    .await;
    assert_eq!(
        call(&mut registry, "exists", json!({ "path": copied.to_string_lossy() })).await,
        json!(false)
    );
    assert_eq!(
        call(&mut registry, "read", json!({ "path": moved.to_string_lossy() })).await,
        json!("first/second")
    );

    // glob (relative to canonical base)
    let pattern = format!("{}/*.txt", base.display());
    let globbed = call(&mut registry, "glob", json!({ "pattern": pattern })).await;
    let mut globbed: Vec<String> = globbed
        .as_array()
        .expect("list of paths")
        .iter()
        .map(|v| v.as_str().unwrap_or_default().to_string())
        .collect();
    globbed.sort();
    assert!(globbed.contains(&file.to_string_lossy().into_owned()), "{globbed:?}");
    assert!(globbed.contains(&moved.to_string_lossy().into_owned()), "{globbed:?}");

    // delete
    call(&mut registry, "delete", json!({ "path": moved.to_string_lossy() })).await;
    assert_eq!(
        call(&mut registry, "exists", json!({ "path": moved.to_string_lossy() })).await,
        json!(false)
    );
    // deleting a directory removes it recursively
    call(&mut registry, "delete", json!({ "path": subdir.to_string_lossy() })).await;
    assert_eq!(
        call(&mut registry, "exists", json!({ "path": subdir.to_string_lossy() })).await,
        json!(false)
    );

    // Error cases: non-UTF-8 content is refused by `read` and served by
    // `read_bytes` through the binary convention; missing paths and
    // unknown methods surface readable errors.
    let binary = base.join("raw.bin");
    call(
        &mut registry,
        "write",
        json!({
            "path": binary.to_string_lossy(),
            "content": { "data": "//4=", "encoding": "base64" }, // 0xFF 0xFE
        }),
    )
    .await;
    let read_error = registry
        .call("fs", "read", json!({ "path": binary.to_string_lossy() }), REGULAR_TIMEOUT)
        .await
        .expect_err("non-UTF-8 content is refused");
    assert!(read_error.to_string().contains("not valid UTF-8"), "{read_error}");
    let raw = call(&mut registry, "read_bytes", json!({ "path": binary.to_string_lossy() })).await;
    assert_eq!(raw["encoding"], "base64");
    assert_eq!(raw["data"], "//4=");

    let missing = base.join("nope.txt");
    let missing_error = registry
        .call("fs", "read", json!({ "path": missing.to_string_lossy() }), REGULAR_TIMEOUT)
        .await
        .expect_err("missing file is a readable error");
    assert!(missing_error.to_string().contains("fs.read"), "{missing_error}");

    let unknown = registry
        .call("fs", "chmod", json!({}), REGULAR_TIMEOUT)
        .await
        .expect_err("unknown method");
    assert!(unknown.to_string().contains("unknown method"), "{unknown}");

    let report = registry.shutdown(SHUTDOWN_TIMEOUT).await;
    assert_eq!(report.stopped, vec!["fs".to_string()]);
}

/// The watch ordering test: create+write yields `file_created` then
/// `file_modified` in order; a second write is a bare `file_modified`;
/// `unwatch` stops the flow; directory children report dir kinds.
#[tokio::test(flavor = "multi_thread")]
async fn watch_reports_ordered_events_and_unwatch_stops() {
    let (mut registry, base, mut rx) = boot("fs-watch").await;
    let dir = std::fs::canonicalize(&base).expect("canonical base");

    call(&mut registry, "watch", json!({ "path": dir.to_string_lossy() })).await;

    // First write to a fresh path: CREATE then MODIFY, in that order.
    let file = dir.join("watched.txt");
    call(
        &mut registry,
        "write",
        json!({ "path": file.to_string_lossy(), "content": "v1" }),
    )
    .await;
    let (created_path, _) = next_fs_event(&mut rx, "file_created").await;
    assert_eq!(PathBuf::from(created_path), file, "created event carries the path");
    let (modified_path, _) = next_fs_event(&mut rx, "file_modified").await;
    assert_eq!(PathBuf::from(modified_path), file);

    // Second write: a bare modify (the file already exists).
    call(
        &mut registry,
        "write",
        json!({ "path": file.to_string_lossy(), "content": "v2" }),
    )
    .await;
    let (modified_path, _) = next_fs_event(&mut rx, "file_modified").await;
    assert_eq!(PathBuf::from(modified_path), file);

    // Directory children: a new subdirectory is a dir_created.
    let sub = dir.join("subdir");
    call(&mut registry, "mkdir", json!({ "path": sub.to_string_lossy() })).await;
    let (dir_created_path, _) = next_fs_event(&mut rx, "dir_created").await;
    assert_eq!(PathBuf::from(dir_created_path), sub);

    // Deleting the child directory reports dir_deleted.
    call(&mut registry, "delete", json!({ "path": sub.to_string_lossy() })).await;
    let (_, _) = next_fs_event(&mut rx, "dir_deleted").await;

    // Unwatch stops the flow: a further write produces no events.
    let was_watched = call(
        &mut registry,
        "unwatch",
        json!({ "path": dir.to_string_lossy() }),
    )
    .await;
    assert_eq!(was_watched, json!(true));
    call(
        &mut registry,
        "write",
        json!({ "path": file.to_string_lossy(), "content": "v3" }),
    )
    .await;
    let quiet =
        tokio::time::timeout(Duration::from_millis(300), rx.recv()).await;
    match quiet {
        Err(_) => {}                                          // nothing arrived
        Ok(Ok(event)) if event.name.starts_with("fs.") => {
            panic!("events must stop after unwatch, got {event:?}")
        }
        Ok(Ok(_)) => {}                                       // unrelated event
        Ok(Err(_)) => {}                                      // lagged
    }

    // Unwatching an unwatched path reports false.
    let not_watched = call(
        &mut registry,
        "unwatch",
        json!({ "path": dir.to_string_lossy() }),
    )
    .await;
    assert_eq!(not_watched, json!(false));

    let report = registry.shutdown(SHUTDOWN_TIMEOUT).await;
    assert_eq!(report.stopped, vec!["fs".to_string()]);
}

/// Watching a single file filters the parent-directory events down to it.
#[tokio::test(flavor = "multi_thread")]
async fn file_watch_filters_to_the_file() {
    let (mut registry, base, mut rx) = boot("fs-watch-file").await;
    let dir = std::fs::canonicalize(&base).expect("canonical base");

    let file = dir.join("target.txt");
    std::fs::write(&file, "seed").expect("seed file exists before the watch");
    let sibling = dir.join("sibling.txt");
    std::fs::write(&sibling, "sibling").expect("sibling file exists");

    call(&mut registry, "watch", json!({ "path": file.to_string_lossy() })).await;

    // Writes to the watched file and to a sibling: only the watched one
    // may surface as file_modified.
    std::fs::write(&sibling, "churn").expect("sibling churn");
    call(
        &mut registry,
        "write",
        json!({ "path": file.to_string_lossy(), "content": "seeded write" }),
    )
    .await;

    let (path, _) = next_fs_event(&mut rx, "file_modified").await;
    assert_eq!(PathBuf::from(path), file, "the sibling's churn is filtered out");

    // And no sibling-created event follows: the next fs event (if any
    // at all) must belong to the watched file or never come.
    match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
        Ok(Ok(event)) if event.name.starts_with("fs.") && event.name != "fs.file_modified" => {
            panic!("sibling events must be filtered, got {event:?}");
        }
        _ => {}
    }

    let report = registry.shutdown(SHUTDOWN_TIMEOUT).await;
    assert_eq!(report.stopped, vec!["fs".to_string()]);
}

/// Deleting the watched directory itself triggers the self-teardown path
/// (`fs.watch_error`) and the watch stops reporting afterwards.
#[tokio::test(flavor = "multi_thread")]
async fn deleting_the_watched_dir_raises_watch_error() {
    let (mut registry, base, mut rx) = boot("fs-watch-self").await;
    let dir = std::fs::canonicalize(&base).expect("canonical base");

    call(&mut registry, "watch", json!({ "path": dir.to_string_lossy() })).await;
    std::fs::remove_dir_all(&dir).expect("the fixture dir is removed under the watch");

    let (path, error) = next_fs_event(&mut rx, "watch_error").await;
    assert_eq!(PathBuf::from(path), dir, "the error carries the watched path");
    assert!(
        error.payload["error"].as_str().unwrap_or_default().contains("removed"),
        "{error:?}"
    );

    // The watch is gone: re-watching the (recreated) path works and the
    // module still shuts down cleanly.
    std::fs::create_dir_all(&dir).expect("recreate");
    call(&mut registry, "watch", json!({ "path": dir.to_string_lossy() })).await;
    call(
        &mut registry,
        "unwatch",
        json!({ "path": dir.to_string_lossy() }),
    )
    .await;

    let report = registry.shutdown(SHUTDOWN_TIMEOUT).await;
    assert_eq!(report.stopped, vec!["fs".to_string()]);
}
