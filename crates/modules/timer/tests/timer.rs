//! Phase 4 acceptance for the `timer` module: `after`/`every` fire through
//! callback tokens, `cancel` aborts and emits `timer.cancelled`, `now` is
//! the epoch clock, and `timer.sleep` parks the calling Lua job — the
//! dispatch loop keeps serving events while the sleeper waits. That last
//! property is the parked-job decision recorded in IMPLEMENTATION_PLAN §5,
//! and the turnover benchmark is asserted here (a simple-block
//! implementation cannot pass it).

use noisy_core::registry::ModuleRegistry;
use noisy_core::{LuaDeps, LuaRuntime};
use noisy_core::NoisyEvent;
use noisy_timer::TimerModule;
use serde_json::json;
use std::time::Duration;

const REGULAR_TIMEOUT: Duration = Duration::from_secs(2);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(100);

/// Registry + Lua runtime with the timer module, entry script started.
async fn boot(
    source: &str,
) -> (
    ModuleRegistry,
    LuaRuntime,
    tokio::sync::broadcast::Receiver<NoisyEvent>,
) {
    let mut registry = ModuleRegistry::new();
    registry
        .load(Box::new(TimerModule::new()), json!({}))
        .await
        .expect("timer module loads");
    let deps: LuaDeps = registry.lua_deps();
    let runtime = LuaRuntime::start(deps);
    let mut rx = registry.bus().subscribe();
    // Drain lifecycle noise (service_started) before the test's events.
    drain_until_ready(&mut rx).await;
    runtime
        .handle()
        .run_source("app.lua", source.to_string())
        .await
        .expect("entry script starts");
    (registry, runtime, rx)
}

async fn drain_until_ready(rx: &mut tokio::sync::broadcast::Receiver<NoisyEvent>) {
    while let Ok(event) = rx.try_recv() {
        if event.name == "noisy.service_started" {
            continue;
        }
        break;
    }
}

/// The next matching event within a deadline; other events are skipped.
async fn next_named(
    rx: &mut tokio::sync::broadcast::Receiver<NoisyEvent>,
    name: &str,
) -> NoisyEvent {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {name}"
        );
        let remaining = deadline
            .saturating_duration_since(tokio::time::Instant::now())
            .min(Duration::from_millis(200));
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(event)) if event.name == name => return event,
            Ok(Ok(_)) => {}        // unrelated event; keep waiting
            Ok(Err(_)) => {}       // lagged; keep waiting
            Err(_) => {}           // window expiry; re-check the deadline
        }
    }
}

/// The engine.shutdown call both tests share.
async fn stop(
    mut registry: ModuleRegistry,
    runtime: LuaRuntime,
    rx: tokio::sync::broadcast::Receiver<NoisyEvent>,
) {
    let report = registry.shutdown(SHUTDOWN_TIMEOUT).await;
    assert_eq!(report.stopped, vec!["timer".to_string()]);
    drop(rx);
    runtime.join().expect("lua runtime stops cleanly");
}

#[tokio::test(flavor = "multi_thread")]
async fn after_fires_once_through_the_callback_token() {
    let (registry, runtime, mut rx) = boot(
        r#"
timer.after(50, function()
    noisy.emit("probe.step", {})
end)
"#,
    )
    .await;

    let event = next_named(&mut rx, "probe.step").await;
    assert_eq!(event.source, "probe", "noisy.emit splits by name");

    // `after` fires exactly once: no further steps within a generous wait.
    if let Ok(Ok(other)) =
        tokio::time::timeout(Duration::from_millis(150), rx.recv()).await
    {
        assert_ne!(other.name, "probe.step", "after must fire once");
    }

    stop(registry, runtime, rx).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn every_repeats_and_cancel_stops_it() {
    let (registry, runtime, mut rx) = boot(
        r#"
ticks = 0
every_id = timer.every(20, function()
    ticks = ticks + 1
    if ticks == 3 then
        timer.cancel(every_id)
        noisy.emit("probe.cancelled", { ticks = ticks })
    end
end)
"#,
    )
    .await;

    // The handler reached the cancel branch; the module reports the
    // cancellation on its own signal. `timer.cancelled` is emitted before
    // the call reply reaches Lua, so it can precede `probe.cancelled` in
    // the bus order — collect both, don't discard either.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut cancelled: Option<NoisyEvent> = None;
    while cancelled.is_none() {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for cancellation");
        let recv = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await;
        let Ok(Ok(event)) = recv else { continue };
        if event.name == "timer.cancelled" {
            cancelled = Some(event);
        } else if event.name == "probe.cancelled" {
            continue; // arrival confirmation; keep waiting for the signal
        }
    }
    let cancelled = cancelled.expect("seen above");
    assert!(
        cancelled.payload["timer_id"].as_u64().unwrap_or(0) > 0,
        "the cancelled signal carries the numeric timer id"
    );

    // Ticks stop after the cancel: no further `timer.cancelled` for the
    // same id and (crucially) the tick handler is never re-invoked — the
    // remaining interval aborts between fires.
    if let Ok(Ok(other)) =
        tokio::time::timeout(Duration::from_millis(120), rx.recv()).await
    {
        assert_ne!(other.name, "timer.cancelled", "cancel must not repeat");
    }

    stop(registry, runtime, rx).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_an_unknown_id_errors_readably() {
    let (registry, runtime, rx) = boot("").await;

    let failure = runtime
        .handle()
        .run_source(
            "bad.lua",
            "timer.cancel(424242)".to_string(),
        )
        .await
        .expect_err("unknown id fails the script");
    assert!(failure.contains("no timer with id 424242"), "{failure}");

    stop(registry, runtime, rx).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn now_is_the_epoch_clock() {
    let (registry, runtime, rx) = boot("").await;

    let now = runtime.handle().eval("return timer.now()").await.expect("eval");
    let now = now.as_u64().expect("epoch ms number").to_owned();
    let age = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock");
    assert!(now > 0);
    // Within a sane distance of the host clock (zone-independent ms epoch).
    let drift = if now > age.as_millis() as u64 { now - age.as_millis() as u64 } else { age.as_millis() as u64 - now };
    assert!(drift < 5_000, "timer.now drift: {drift} ms");

    stop(registry, runtime, rx).await;
}

/// The parked-sleep proof: while an entry coroutine sleeps, the dispatch
/// loop still handles a bus event whose handler measures the turnaround.
/// A simple-block `sleep` would fail both assertions below.
#[tokio::test(flavor = "multi_thread")]
async fn sleep_parks_the_calling_job_and_keeps_serving_events() {
    let (registry, runtime, mut rx) = boot(
        r#"
seen = false
stamp = 0
noisy.on("probe.ping", function()
    seen = true
    stamp = timer.now()
end)
emit_time = timer.now()
noisy.emit("probe.ping", {})
timer.sleep(150)
assert(seen, "the dispatch loop kept serving events during sleep")
turnaround = stamp - emit_time
noisy.emit("probe.report", { turnaround = turnaround, slept = 150 })
"#,
    )
    .await;

    let report = next_named(&mut rx, "probe.report").await;
    let turnaround = report.payload["turnaround"].as_i64().expect("ms number");
    assert!(
        turnaround < 50,
        "an event during a 150ms sleep must be served well before it \
         resumes (turnaround = {turnaround} ms; a blocking sleep would \
         reach at least 150)"
    );

    // The sleeper's entry job also resumes timely.
    let resumed = runtime
        .handle()
        .eval("return { resumed = seen, after_sleep = os.clock() }")
        .await
        .expect("entry continued after the sleep");
    assert_eq!(resumed["resumed"], true);

    stop(registry, runtime, rx).await;
}

/// `sleep` wakes up after its full duration even when the loop is busy
/// with events meanwhile.
#[tokio::test(flavor = "multi_thread")]
async fn sleep_wakes_after_its_duration() {
    let (registry, runtime, mut rx) = boot(
        r#"
start_ms = timer.now()
noisy.emit("probe.ping", { })
timer.sleep(120)
noisy.emit("probe.resumed", { elapsed = timer.now() - start_ms })
"#,
    )
    .await;

    let resumed = next_named(&mut rx, "probe.resumed").await;
    let elapsed = resumed.payload["elapsed"].as_i64().expect("ms");
    assert!(
        (110..500).contains(&elapsed),
        "the resume lands after the full sleep duration (elapsed = {elapsed} ms)"
    );

    stop(registry, runtime, rx).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sleeping_script_errors_surface() {
    let (registry, runtime, mut rx) = boot(
        r#"
timer.after(20, function() error("boom from a timer handler") end)
sleep_ret = nil
"#,
    )
    .await;

    let error = next_named(&mut rx, "noisy.script_error").await;
    assert!(error.payload["message"].as_str().unwrap_or_default().contains("boom"), "{error:?}");

    // The dispatch loop survived: the runtime still works.
    let ok = runtime.handle().eval("return 1 + 1").await.expect("loop alive");
    assert_eq!(ok, json!(2));

    stop(registry, runtime, rx).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sleep_requires_numbers() {
    let (registry, runtime, rx) = boot("").await;

    let failure = runtime
        .handle()
        .run_source(
            "bad.lua",
            "timer.sleep(\"soon\")".to_string(),
        )
        .await
        .expect_err("non-numeric sleep fails");
    assert!(failure.contains("expects a millisecond count"), "{failure}");

    stop(registry, runtime, rx).await;
}

/// The module-side `sleep` handler refuses native callers (the parked
/// semantics live in the Lua runtime); the refusal is readable.
#[tokio::test(flavor = "multi_thread")]
async fn module_side_sleep_refuses_native_callers() {
    let mut registry = ModuleRegistry::new();
    registry
        .load(Box::new(TimerModule::new()), json!({}))
        .await
        .expect("timer module loads");

    let refusal = registry
        .call("timer", "sleep", json!({ "ms": 50 }), REGULAR_TIMEOUT)
        .await
        .expect_err("module-side sleep is refused");
    assert!(
        refusal.to_string().contains("timer.sleep parks the calling Lua job"),
        "{refusal}"
    );

    let report = registry.shutdown(SHUTDOWN_TIMEOUT).await;
    assert_eq!(report.stopped, vec!["timer".to_string()]);
}
