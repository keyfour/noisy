//! The `noisy` engine binary (specification §3.1, implementation plan
//! §Phase 3): config → bus + registry → in-tree modules → `.so` plugins →
//! Lua runtime → app entry → `noisy.ready`; SIGINT/SIGTERM → clean
//! shutdown → exit 0. `--check` validates config and entry Lua without
//! spawning anything.

use noisy_core::module::NoisyModule;
use noisy_core::lua_runtime::{check_script, LuaRuntime};
use noisy_ffi::PluginModule;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use noisy_engine::app;
use noisy_engine::config::{Config, Entry, Overrides};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let Cli {
        overrides,
        check,
        app_dir,
    } = match parse_cli(&args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("noisy: {message}\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    let config = match Config::load(&app_dir, &overrides) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("noisy: {error}");
            return ExitCode::FAILURE;
        }
    };

    // Structured logs on stdout (specification §4.6 style, no colors).
    tracing_subscriber::fmt()
        .with_target(false)
        .with_writer(std::io::stdout)
        .with_env_filter(tracing_subscriber::EnvFilter::new(
            config.log_level.env_filter(),
        ))
        .init();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "noisy engine starting");

    if check {
        return run_check(&app_dir, &config);
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let report = match runtime.block_on(run(&app_dir, config)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("noisy: {error}");
            ExitCode::FAILURE
        }
    };
    runtime.shutdown_timeout(Duration::from_secs(2));
    report
}

const USAGE: &str = "usage: noisy <app-dir> [--check] [--entry <script.lua|module>]
                     [--log-level <debug|info|warn|error>] [--capacity <n>]
                     [--shutdown-timeout <ms>] [--no-hot-reload]";

struct Cli {
    overrides: Overrides,
    check: bool,
    app_dir: PathBuf,
}

/// Minimal flag parser (the dependency table has no CLI crate): positional
/// app dir plus the five flags, several of them taking a value.
fn parse_cli(args: &[String]) -> Result<Cli, String> {
    let mut overrides = Overrides::default();
    let mut check = false;
    let mut app_dir: Option<PathBuf> = None;
    let mut iter = args.iter();

    fn value(iter: &mut std::slice::Iter<'_, String>, flag: &str) -> Result<String, String> {
        iter.next()
            .cloned()
            .ok_or_else(|| format!("{flag} needs a value"))
    }

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--check" => check = true,
            "--no-hot-reload" => overrides.no_hot_reload = true,
            "--help" | "-h" => {
                println!("noisy engine\n{USAGE}");
                std::process::exit(0);
            }
            "--entry" => overrides.entry = Some(value(&mut iter, "--entry")?),
            "--log-level" => overrides.log_level = Some(value(&mut iter, "--log-level")?),
            "--capacity" => {
                let raw = value(&mut iter, "--capacity")?;
                overrides.capacity = Some(
                    raw.parse::<u64>()
                        .map_err(|_| format!("--capacity `{raw}` is not a number"))?,
                );
            }
            "--shutdown-timeout" => {
                let raw = value(&mut iter, "--shutdown-timeout")?;
                overrides.shutdown_timeout_ms = Some(
                    raw.parse::<u64>().map_err(|_| {
                        format!("--shutdown-timeout `{raw}` is not milliseconds")
                    })?,
                );
            }
            other if other.starts_with('-') && other.len() > 2 => {
                return Err(format!("unknown flag `{other}`"));
            }
            other => {
                if app_dir.is_some() {
                    return Err(format!("unexpected argument `{other}`"));
                }
                app_dir = Some(PathBuf::from(other));
            }
        }
    }

    Ok(Cli {
        app_dir: app_dir.unwrap_or_else(|| PathBuf::from(".")),
        overrides,
        check,
    })
}

/// `--check`: engine config + entry Lua syntax validation, everything
/// without spawning (implementation plan §Phase 3). Exit 0 means good.
fn run_check(app_dir: &Path, config: &Config) -> ExitCode {
    match &config.entry {
        Entry::Lua(script) => {
            let path = app_dir.join(script);
            let Ok(source) = std::fs::read_to_string(&path) else {
                eprintln!("--check: cannot read entry script {}", path.display());
                return ExitCode::FAILURE;
            };
            match check_script(&source, &path.display().to_string()) {
                Ok(()) => {
                    println!("--check ok: {}", path.display());
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("--check: {error}");
                    ExitCode::FAILURE
                }
            }
        }
        Entry::Module(name) => {
            println!("--check ok: entry module `{name}`");
            ExitCode::SUCCESS
        }
    }
}

/// The boot sequence and the engine loop.
async fn run(app_dir: &Path, config: Config) -> Result<(), String> {
    let shutdown_timeout = Duration::from_millis(config.shutdown_timeout_ms);

    // 1. bus + registry (config first — this runs after `Config::load`).
    let mut registry = noisy_core::registry::ModuleRegistry::with_bus(
        noisy_core::EventBus::new(config.capacity),
    );

    // 2. in-tree modules.
    for module in app::in_tree_modules() {
        let name = module.descriptor().name.clone();
        registry
            .load(module, config.module_config(&name))
            .await
            .map_err(|e| format!("loading in-tree module `{name}`: {e}"))?;
    }

    // 3. dynamic plugins: ABI version and descriptor validated at load;
    // names must be unique (the registry checks, the engine reports first).
    let plugin_files = app::plugin_files(&app::plugin_dirs(app_dir, &config.plugin_paths));
    for path in &plugin_files {
        let name = path.file_stem().and_then(|n| n.to_str()).unwrap_or("plugin");
        let module = PluginModule::load(path, config.tick_interval_ms(name))
            .map_err(|e| e.to_string())?;
        let module_name = module.descriptor().name.clone();
        registry
            .load(Box::new(module), config.module_config(&module_name))
            .await
            .map_err(|e| format!("loading plugin `{module_name}` from {}: {e}", path.display()))?;
    }

    // 4. Lua runtime + generated namespaces.
    let runtime = LuaRuntime::start(registry.lua_deps());

    // 5. the application entry point: a module entry is already loaded and
    // running (specification §6.5); a Lua entry is evaluated now.
    match &config.entry {
        Entry::Module(_) => {}
        Entry::Lua(script) => {
            let path = app_dir.join(script);
            runtime
                .handle()
                .load_script(&path)
                .await
                .map_err(|error| format!("entry script {} failed: {error}", path.display()))?;
        }
    }

    registry.emit_core(
        "ready",
        serde_json::json!({ "modules": registry.module_names(), "entry": entry_name(&config.entry) }),
    );

    // 6. run until SIGINT/SIGTERM, then a clean shutdown.
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| format!("signal handler: {e}"))?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("SIGINT received; shutting down");
        }
        _ = terminate.recv() => {
            tracing::info!("SIGTERM received; shutting down");
        }
    }

    let report = registry.shutdown(shutdown_timeout).await;
    if !report.stragglers.is_empty() {
        tracing::warn!(modules = ?report.stragglers, "modules failed to stop in time");
    }
    runtime.join().expect("lua thread exits after shutdown");
    tracing::info!("engine stopped");
    Ok(())
}

/// The entry descriptor used in the `noisy.ready` payload.
fn entry_name(entry: &Entry) -> String {
    match entry {
        Entry::Lua(script) => script.display().to_string(),
        Entry::Module(name) => format!("module:{name}"),
    }
}
