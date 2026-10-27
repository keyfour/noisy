//! The `log` module (specification §4.6).
//!
//! Every `HostCtx::log` call — from any module, the engine, or the Lua
//! `log.*` bindings — publishes the canonical `log.entry { module, level,
//! msg, fields, timestamp }` event directly on the bus. This module
//! subscribes to `log.` and mirrors those events to the console through
//! `tracing` (the engine subscriber renders them), filtered by the level
//! set with `log.set_level` (`debug | info | warn | error`). The filter
//! applies to the console mirror only; the bus always sees every entry, so
//! scripts can react below the print threshold.
//!
//! The module itself never calls `host.log`: mirroring the entries it
//! would publish would loop the subscription back into itself. Method
//! calls simply forward through the host, producing exactly one event.

use noisy_core::error::ModuleError;
use noisy_core::module::{
    Command, HostCtx, LogLevel, ModuleContext, ModuleDescriptor, NoisyModule,
};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use tokio::task::JoinHandle;

/// Numeric levels increasing in severity; the filter is a floor.
const DEBUG: u8 = 0;
const INFO: u8 = 1;
const WARN: u8 = 2;
const ERROR: u8 = 3;

/// Filters only the console mirror; `info` by default.
struct MirrorFilter(AtomicU8);

impl MirrorFilter {
    fn parse(name: &str) -> Option<u8> {
        match name {
            "debug" => Some(DEBUG),
            "info" => Some(INFO),
            "warn" => Some(WARN),
            "error" => Some(ERROR),
            _ => None,
        }
    }
}

impl Default for MirrorFilter {
    fn default() -> Self {
        Self(AtomicU8::new(INFO))
    }
}

fn level_from_method(method: &str) -> Option<LogLevel> {
    match method {
        "debug" => Some(LogLevel::Debug),
        "info" => Some(LogLevel::Info),
        "warn" => Some(LogLevel::Warn),
        "error" => Some(LogLevel::Error),
        _ => None,
    }
}

/// The `log` module: descriptor plus the mirror filter, running as one
/// actor that serves `log.*` methods and mirrors `log.entry` events.
pub struct LogModule {
    filter: Arc<MirrorFilter>,
}

impl Default for LogModule {
    fn default() -> Self {
        Self::new()
    }
}

impl LogModule {
    pub fn new() -> Self {
        Self {
            filter: Arc::default(),
        }
    }
}

impl NoisyModule for LogModule {
    fn descriptor(&self) -> ModuleDescriptor {
        ModuleDescriptor::new("log")
            .version("0.1.0")
            .signals(["entry"])
            .method_params("debug", ["msg", "fields"])
            .method_params("info", ["msg", "fields"])
            .method_params("warn", ["msg", "fields"])
            .method_params("error", ["msg", "fields"])
            .method_params("set_level", ["level"])
    }

    fn start(self: Box<Self>, ctx: ModuleContext) -> JoinHandle<()> {
        let ModuleContext {
            mut commands,
            host,
            shutdown,
            config: _,
        } = ctx;
        let filter = self.filter;
        let mut events = host.on("log.");
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => break,
                    maybe = events.next() => match maybe {
                        Some(event) => mirror(&filter, &event),
                        None => break,
                    },
                    cmd = commands.recv() => match cmd {
                        Some(Command::MethodCall(call)) => {
                            let reply = mirror_call(&filter, &host, &call);
                            call.reply(reply);
                        }
                        // The log module issues no async calls.
                        Some(_) => {}
                        None => break,
                    },
                }
            }
            // Drain phase: fail every command still queued with the precise
            // shutdown reason (specification §3.4). `recv` returns `None`
            // once the registry has dropped the command senders.
            while let Some(cmd) = commands.recv().await {
                if let Command::MethodCall(call) = cmd {
                    call.reply(Err(ModuleError::ShuttingDown));
                }
            }
        })
    }
}

/// Publishes a call-backed entry through `host.log`: exactly one canonical
/// `log.entry` event, mirrored by this module's own subscription.
fn mirror_call(filter: &Arc<MirrorFilter>, host: &HostCtx, call: &noisy_core::module::MethodCall) -> Result<Value, ModuleError> {
    if let Some(level) = level_from_method(call.method()) {
        let params = call.params();
        let Some(msg) = params["msg"].as_str() else {
            return Err(ModuleError::failed(
                "log",
                format!("log.{}: `msg` must be a string", call.method()),
            ));
        };
        host.log(level, msg, params.get("fields").cloned());
        Ok(json!({}))
    } else if call.method() == "set_level" {
        let params = call.params();
        let Some(name) = params["level"].as_str() else {
            return Err(ModuleError::failed(
                "log",
                "log.set_level: `level` must be a string",
            ));
        };
        match MirrorFilter::parse(name) {
            Some(level) => {
                filter.0.store(level, Ordering::Relaxed);
                Ok(json!({}))
            }
            None => Err(ModuleError::failed(
                "log",
                format!(
                    "log.set_level: unknown level `{name}` (supported: debug, info, warn, error)"
                ),
            )),
        }
    } else {
        Err(ModuleError::unknown_method("log", call.method()))
    }
}

/// Prints one canonical event at its own level, honoring the filter.
fn mirror(filter: &Arc<MirrorFilter>, event: &noisy_core::bus::NoisyEvent) {
    let level = event.payload["level"].as_str().unwrap_or("info");
    let level = match MirrorFilter::parse(level) {
        Some(level) => level,
        None => WARN, // unreadable entries surface loudly
    };
    if level < filter.0.load(Ordering::Relaxed) {
        return;
    }
    let module = event.payload["module"].as_str().unwrap_or("?");
    let msg = event.payload["msg"].as_str().unwrap_or_default();
    let fields = event.payload["fields"]
        .as_object()
        .and_then(|fields| {
            if fields.is_empty() {
                None
            } else {
                Some(Value::Object(fields.clone()).to_string())
            }
        });
    match level {
        DEBUG => tracing::debug!(module, fields, "{msg}"),
        INFO => tracing::info!(module, fields, "{msg}"),
        WARN => tracing::warn!(module, fields, "{msg}"),
        _ => tracing::error!(module, fields, "{msg}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_renders_levels() {
        for (name, value) in [("debug", DEBUG), ("info", INFO), ("warn", WARN), ("error", ERROR)] {
            assert_eq!(MirrorFilter::parse(name), Some(value), "{name}");
        }
        assert_eq!(MirrorFilter::parse("chatty"), None);
    }

    #[test]
    fn level_methods_map() {
        for method in ["debug", "info", "warn", "error"] {
            assert!(level_from_method(method).is_some(), "{method}");
        }
        assert!(level_from_method("set_level").is_none());
    }
}
