//! The `timer` module (specification §4.4).
//!
//! Owns the timer id registry. `after` and `every` schedule tokio tasks
//! that emit `timer.fired { timer_id, elapsed_ms }` and invoke the
//! callback token received from the Lua binding (Lua function arguments
//! become opaque tokens, specification §3.1); `cancel` aborts a running
//! timer and emits `timer.cancelled`; `now` returns epoch milliseconds.
//!
//! `sleep` is declared here for API parity with the specification, but it
//! never sleeps on the module side: sleeping is parking a calling Lua job
//! (coroutine suspension), which is a Lua-runtime capability. Native
//! (non-Lua) callers get a readable refusal pointing at `timer.after`.
//! The registry ids of parked sleeps are the wake tokens of the Lua
//! runtime.

use noisy_core::callback::CallbackToken;
use noisy_core::error::ModuleError;
use noisy_core::module::{
    Command, HostCtx, MethodCall, ModuleContext, ModuleDescriptor, NoisyModule,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;

type Registry = Arc<Mutex<HashMap<u64, JoinHandle<()>>>>;

/// One fire: emits `timer.fired` and invokes the callback token with the
/// same payload. The invocation is a non-blocking send against the bounded
/// callback sink; a busy Lua thread gets up to one second of 5 ms retries
/// before the fire is dropped with a warning.
async fn fire(host: &HostCtx, id: u64, started_ms: u64, token: CallbackToken) {
    let elapsed_ms = host.now_ms().saturating_sub(started_ms);
    let payload = json!({ "timer_id": id, "elapsed_ms": elapsed_ms });
    host.emit("fired", payload.clone());

    let mut attempts = 0;
    while !host.invoke_callback(token, payload.clone()) {
        attempts += 1;
        if attempts >= 200 {
            tracing::warn!(
                timer_id = id,
                token,
                "callback sink stayed saturated for 1s; dropping this fire"
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Serves one method call; the actor loop never blocks across long timers
/// — everything scheduled is a `tokio::spawn`.
async fn serve(
    call: &MethodCall,
    host: &HostCtx,
    next: &AtomicU64,
    timers: &Registry,
) -> Result<Value, ModuleError> {
    let params = call.params().clone();
    match call.method() {
        "after" | "every" => {
            let ms = params["ms"].as_u64().ok_or_else(|| {
                ModuleError::failed(
                    "timer",
                    format!("timer.{}: `ms` must be a non-negative number", call.method()),
                )
            })?;
            let token = params["fn"].as_u64().ok_or_else(|| {
                ModuleError::failed(
                    "timer",
                    format!(
                        "timer.{}: `fn` must be a callback token (Lua function arguments \
                         become tokens; native callers pass the token from the Lua binding)",
                        call.method()
                    ),
                )
            })?;
            if ms == 0 {
                return Err(ModuleError::failed(
                    "timer",
                    "timer: `ms` must be greater than zero",
                ));
            }

            let id = next.fetch_add(1, Ordering::Relaxed);
            let started_ms = host.now_ms();
            let fire_host = host.clone();
            let handle = if call.method() == "after" {
                let timers = Arc::clone(timers);
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    fire(&fire_host, id, started_ms, token).await;
                    // One-shot timers leave the registry as they fire; an
                    // abort racing between the sleep and the fire is at
                    // most a dropped fire (documented cancellation edge).
                    timers.lock().unwrap().remove(&id);
                })
            } else {
                tokio::spawn(async move {
                    let mut tick = tokio::time::interval(Duration::from_millis(ms));
                    // `interval` fires immediately on the first tick; that
                    // tick is consumed so the first handler call comes
                    // after one full period, matching `every(ms)`.
                    tick.tick().await;
                    loop {
                        tick.tick().await;
                        fire(&fire_host, id, started_ms, token).await;
                    }
                })
            };
            timers.lock().unwrap().insert(id, handle);
            Ok(json!(id))
        }
        "cancel" => {
            let id = params["timer_id"].as_u64().ok_or_else(|| {
                ModuleError::failed("timer", "timer.cancel: `timer_id` must be a number")
            })?;
            let handle = timers.lock().unwrap().remove(&id);
            match handle {
                Some(handle) => {
                    handle.abort();
                    host.emit("cancelled", json!({ "timer_id": id }));
                    Ok(json!({}))
                }
                None => Err(ModuleError::failed(
                    "timer",
                    format!("timer.cancel: no timer with id {id}"),
                )),
            }
        }
        "now" => Ok(json!(host.now_ms())),
        "sleep" => Err(ModuleError::failed(
            "timer",
            "timer.sleep parks the calling Lua job; native (non-Lua) callers should use \
             timer.after(ms, fn) instead",
        )),
        other => Err(ModuleError::unknown_method("timer", other)),
    }
}

/// The `timer` module.
pub struct TimerModule;

impl Default for TimerModule {
    fn default() -> Self {
        Self::new()
    }
}

impl TimerModule {
    pub fn new() -> Self {
        Self
    }
}

impl NoisyModule for TimerModule {
    fn descriptor(&self) -> ModuleDescriptor {
        ModuleDescriptor::new("timer")
            .version("0.1.0")
            .signals(["fired", "cancelled"])
            .method_params("after", ["ms", "fn"])
            .method_params("every", ["ms", "fn"])
            .method_params("cancel", ["timer_id"])
            .method("now")
            .method_params("sleep", ["ms"])
    }

    fn start(self: Box<Self>, ctx: ModuleContext) -> JoinHandle<()> {
        let ModuleContext {
            mut commands,
            host,
            shutdown,
            config: _,
        } = ctx;
        let next = AtomicU64::new(1);
        let timers: Registry = Arc::new(Mutex::new(HashMap::new()));
        tokio::spawn(async move {
            {
                let timers = Arc::clone(&timers);
                loop {
                    tokio::select! {
                        biased;
                        _ = shutdown.cancelled() => break,
                        cmd = commands.recv() => match cmd {
                            Some(Command::MethodCall(call)) => {
                                let reply = serve(&call, &host, &next, &timers).await;
                                call.reply(reply);
                            }
                            // The timer module issues no async calls.
                            Some(_) => {}
                            None => break,
                        },
                    }
                }
            }
            // Aborting every scheduled timer so no fire outlives the module.
            {
                let mut map = timers.lock().unwrap();
                for (_, handle) in map.drain() {
                    handle.abort();
                }
            }
            // Drain phase: precise shutdown failures for late callers.
            while let Some(cmd) = commands.recv().await {
                if let Command::MethodCall(call) = cmd {
                    call.reply(Err(ModuleError::ShuttingDown));
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_declares_the_spec_surface() {
        let descriptor = TimerModule::new().descriptor();
        descriptor.validate().expect("valid descriptor");
        for method in ["after", "every", "cancel", "now", "sleep"] {
            assert!(descriptor.method_decl(method).is_some(), "{method}");
        }
        assert_eq!(descriptor.signals, vec!["fired", "cancelled"]);
    }
}
