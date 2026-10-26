//! Engine configuration (`engine.toml`, specification §8/§6.5): parsed
//! completely before any module is loaded — with CLI overrides applied.

use serde_json::Value as Json;
use std::fmt;
use std::path::{Path, PathBuf};

/// The loaded (`<app-dir>/engine.toml`) configuration with defaults filled
/// in and CLI overrides applied.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Application entry: a Lua script or a module name (specification §6.5).
    pub entry: Entry,
    /// Engine log level (module logs go through `host.log`).
    pub log_level: LogLevel,
    /// Bus channel capacity.
    pub capacity: usize,
    /// Per-module join budget at shutdown.
    pub shutdown_timeout_ms: u64,
    /// Hot-reload switch (the fs watch arrives in Phase 9; parsed now).
    pub hot_reload: bool,
    /// Extra directories to search for `.so` plugins.
    pub plugin_paths: Vec<PathBuf>,
    /// Per-module configuration tables (`[<module_name>]`).
    pub module_configs: Vec<(String, Json)>,
}

/// Where the application starts.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    /// A Lua script, relative to the app directory (default: `app.lua`).
    Lua(PathBuf),
    /// A module entry point named in the config (specification §6.5).
    Module(String),
}

/// Engine log level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "debug" => Some(Self::Debug),
            "info" => Some(Self::Info),
            "warn" => Some(Self::Warn),
            "error" => Some(Self::Error),
            _ => None,
        }
    }

    /// Filter directive for `tracing-subscriber`.
    pub fn env_filter(self) -> String {
        format!("noisy={}", self.as_str())
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// A configuration problem — always readable.
#[derive(Debug)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "configuration error: {}", self.0)
    }
}

impl Config {
    /// Loads `<app-dir>/engine.toml` (defaults apply when absent) and
    /// applies CLI overrides. Fails fast: nothing is parsed lazily.
    pub fn load(app_dir: &Path, overrides: &Overrides) -> Result<Self, ConfigError> {
        let file = app_dir.join("engine.toml");
        let raw = if file.is_file() {
            std::fs::read_to_string(&file)
                .map_err(|e| ConfigError(format!("reading {}: {e}", file.display())))?
        } else {
            String::new()
        };
        let parsed: toml::Table =
            toml::from_str(&raw).map_err(|e| ConfigError(format!("{}: {e}", file.display())))?;

        let engine = parsed.get("engine").and_then(|e| e.as_table());
        let entry = parsed.get("entry").and_then(|e| e.as_table());

        fn engine_string<'a>(table: Option<&'a toml::Table>, key: &str) -> Option<&'a str> {
            table.and_then(|t| t.get(key)).and_then(|v| v.as_str())
        }
        fn engine_int(table: Option<&toml::Table>, key: &str) -> Option<i64> {
            table.and_then(|t| t.get(key)).and_then(|v| v.as_integer())
        }
        fn entry_string<'a>(table: Option<&'a toml::Table>, key: &str) -> Option<&'a str> {
            table.and_then(|t| t.get(key)).and_then(|v| v.as_str())
        }

        // Entry (specification §6.5): `type` selects the shape.
        let mut entry = match entry_string(entry, "type") {
            Some("module") => Entry::Module(
                entry_string(entry, "name")
                    .ok_or_else(|| ConfigError("[entry] type = \"module\" needs `name`".into()))?
                    .to_string(),
            ),
            Some("lua") | None => Entry::Lua(PathBuf::from(
                entry_string(entry, "script").unwrap_or("app.lua"),
            )),
            Some(other) => return Err(ConfigError(format!(
                "[entry] type = `{other}` is not `lua` or `module`"
            ))),
        };
        if let Some(value) = &overrides.entry {
            // `--entry`: a `.lua` path is a Lua entry, anything else names
            // a module entry point.
            if value.ends_with(".lua") {
                entry = Entry::Lua(PathBuf::from(value));
            } else {
                entry = Entry::Module(value.clone());
            }
        }

        let log_level = if let Some(text) = &overrides.log_level {
            LogLevel::parse(text)
                .ok_or_else(|| ConfigError(format!("--log-level `{text}` is not debug|info|warn|error")))?
        } else {
            engine_string(engine, "log_level")
                .map(|text| {
                    LogLevel::parse(text)
                        .ok_or_else(|| ConfigError(format!("[engine] log_level `{text}` is not debug|info|warn|error")))
                })
                .transpose()?
                .unwrap_or(LogLevel::Info)
        };

        let capacity: Option<u64> = overrides
            .capacity
            .or_else(|| engine_int(engine, "capacity").map(|value| value.max(1) as u64));
        let capacity = capacity.unwrap_or(1024) as usize;
        let shutdown_timeout_ms = overrides
            .shutdown_timeout_ms
            .or_else(|| {
                engine_int(engine, "shutdown_timeout_ms")
                    .and_then(|v| u64::try_from(v).ok())
            })
            .unwrap_or(2000);
        let hot_reload = !overrides.no_hot_reload
            && engine
                .and_then(|t| t.get("hot_reload"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

        let plugin_paths = parsed
            .get("plugins")
            .and_then(|p| p.get("paths"))
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .map(|i| {
                        i.as_str().map(PathBuf::from).ok_or_else(|| {
                            ConfigError("[plugins] paths must be strings".into())
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?
            .unwrap_or_default();

        let module_configs = parsed
            .iter()
            .filter(|(key, _)| {
                !matches!(key.as_str(), "engine" | "entry" | "plugins")
            })
            .map(|(key, value)| {
                (key.clone(), process_toml_value(value))
            })
            .collect();

        Ok(Self {
            entry,
            log_level,
            capacity,
            shutdown_timeout_ms,
            hot_reload,
            plugin_paths,
            module_configs,
        })
    }

    /// The configuration passed to a module's context; engine-reserved
    /// keys are stripped.
    pub fn module_config(&self, name: &str) -> Json {
        self.module_configs
            .iter()
            .find(|(module, _)| module == name)
            .map(|(_, value)| value.clone())
            .unwrap_or(Json::Null)
    }

    /// Per-module engine knob: how often a dynamic module's tick fires.
    /// 0 disables the tick.
    pub fn tick_interval_ms(&self, name: &str) -> u64 {
        self.module_config(name)["tick_interval_ms"].as_u64().unwrap_or(1000)
    }
}

/// CLI overrides applied on top of `engine.toml`.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Overrides {
    pub entry: Option<String>,
    pub log_level: Option<String>,
    pub capacity: Option<u64>,
    pub shutdown_timeout_ms: Option<u64>,
    pub no_hot_reload: bool,
}

/// Converts a TOML value into JSON for the boundary (`toml` crate's
/// datetime needs a string/number representation).
fn process_toml_value(value: &toml::Value) -> Json {
    match value {
        toml::Value::Table(table) => table
            .iter()
            .map(|(k, v)| (k.clone(), process_toml_value(v)))
            .collect::<serde_json::Map<_, _>>()
            .into(),
        toml::Value::Array(items) => items.iter().map(process_toml_value).collect::<Vec<_>>().into(),
        toml::Value::String(s) => Json::from(s.clone()),
        toml::Value::Integer(i) => Json::from(*i),
        toml::Value::Float(f) => Json::from(*f),
        toml::Value::Boolean(b) => Json::from(*b),
        toml::Value::Datetime(d) => Json::from(d.to_string()),
    }
}
