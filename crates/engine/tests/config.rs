//! `engine.toml` parsing and CLI override behavior (Phase 3 CLI contract).

use noisy_engine::config::{Config, Entry, LogLevel, Overrides};
use std::path::Path;

fn load(app_dir: &Path, overrides: &Overrides) -> Config {
    Config::load(app_dir, overrides).expect("valid config")
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("two levels up is the workspace")
        .join("target")
        .join(format!("cfg-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("config fixture dir");
    dir
}

#[test]
fn defaults_when_no_engine_toml() {
    let dir = temp_dir("defaults");
    let config = load(&dir, &Overrides::default());
    assert_eq!(config.entry, Entry::Lua(std::path::PathBuf::from("app.lua")));
    assert_eq!(config.log_level, LogLevel::Info);
    assert_eq!(config.capacity, 1024);
    assert_eq!(config.shutdown_timeout_ms, 2000);
    assert!(!config.hot_reload);
}

#[test]
fn module_entry_and_module_table() {
    let dir = temp_dir("module_entry");
    std::fs::write(
        dir.join("engine.toml"),
        r#"
[entry]
type = "module"
name = "myapp"

[gadget]
protect = true
tick_interval_ms = 0

[timer_pool]
"#
        .trim_start(),
    )
    .expect("engine.toml");

    let config = load(&dir, &Overrides::default());
    assert_eq!(config.entry, Entry::Module("myapp".to_string()));
    // Per-module tables become module config; tick is an engine knob.
    assert_eq!(config.tick_interval_ms("gadget"), 0, "0 disables the tick");
    assert_eq!(
        config.module_config("gadget")["protect"],
        serde_json::json!(true)
    );
    assert_eq!(config.tick_interval_ms("timer_pool"), 1000, "default tick");
    assert_eq!(
        config.module_config("timer_pool").as_object().expect("table").len(),
        0,
        "reserved keys only are stripped"
    );
    assert_eq!(config.module_config("unknown"), serde_json::json!(null));
}

#[test]
fn cli_overrides_win() {
    let dir = temp_dir("overrides");
    std::fs::write(
        dir.join("engine.toml"),
        r#"
[engine]
log_level = "info"
capacity = 512
shutdown_timeout_ms = 1000

[entry]
type = "module"
name = "myapp"
"#
        .trim_start(),
    )
    .expect("engine.toml");

    let overrides = Overrides {
        entry: Some("other.lua".to_string()),
        log_level: Some("debug".to_string()),
        capacity: Some(4096),
        shutdown_timeout_ms: Some(500),
        no_hot_reload: true,
    };
    let config = load(&dir, &overrides);
    assert_eq!(
        config.entry,
        Entry::Lua(std::path::PathBuf::from("other.lua"))
    );
    assert_eq!(config.log_level, LogLevel::Debug);
    assert_eq!(config.capacity, 4096);
    assert_eq!(config.shutdown_timeout_ms, 500);
}

#[test]
fn invalid_values_fail_fast() {
    let dir = temp_dir("invalid");
    std::fs::write(dir.join("engine.toml"), "[entry]\ntype = \"ssh\"\n").expect("engine.toml");
    let error = Config::load(&dir, &Overrides::default()).expect_err("bad entry type");
    assert!(error.to_string().contains("entry"), "readable: {error}");

    let dir = temp_dir("invalid_level");
    std::fs::write(
        dir.join("engine.toml"),
        "[engine]\nlog_level = \"verbose\"\n",
    )
    .expect("engine.toml");
    let error =
        Config::load(&dir, &Overrides::default()).expect_err("bad log level");
    assert!(error.to_string().contains("log_level"), "readable: {error}");

    let dir = temp_dir("bad_cli");
    let overrides = Overrides {
        log_level: Some("loud".to_string()),
        ..Overrides::default()
    };
    let error = Config::load(&dir, &overrides).expect_err("bad CLI level");
    assert!(error.to_string().contains("log-level"), "readable: {error}");
}
