//! End-to-end engine tests (implementation plan §Phase 3 acceptance): spawn
//! the real `noisy` binary against fixtures under `target/e2e-*`, wait for
//! observable behavior, signal SIGINT, and assert exit codes.
//!
//! Fixtures live under the workspace's `target/` (never committed); the
//! `testplugin`/`badplugin` cdylibs are built by `cargo test --workspace`
//! before this suite runs.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

/// The built binary (`CARGO_BIN_EXE_noisy` from the engine package).
const ENGINE: &str = env!("CARGO_BIN_EXE_noisy");

const START_TIMEOUT: Duration = Duration::from_secs(30);
const EXIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Builds the workspace's cdylibs once per test process: `cargo test` does
/// not produce cdylib artifacts on its own, and the fixtures need them.
fn ensure_plugins_built() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if target_dir().join("debug/libtestplugin.so").is_file()
            && target_dir().join("debug/libbadplugin.so").is_file()
        {
            return;
        }
        let output = Command::new("cargo")
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
        .expect("crates/engine sits two levels under the workspace")
        .to_path_buf()
}

/// `target/` of the workspace, where `cargo build` leaves the cdylibs.
fn target_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/engine sits two levels under the workspace")
        .join("target")
}

/// Recreates `target/e2e-<name>/{modules}` and copies the plugin in.
fn fixture(name: &str, plugin_crate: &str) -> PathBuf {
    ensure_plugins_built();
    let dir = workspace_root()
        .join("target")
        .join(format!("e2e-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("modules")).expect("fixture modules dir");
    std::fs::copy(
        workspace_root().join(format!("target/debug/lib{plugin_crate}.so")),
        dir.join(format!("modules/lib{plugin_crate}.so")),
    )
    .expect("cdylib built by the workspace build");
    dir
}

/// Writes the standard `engine.toml` (Lua entry).
fn write_config(dir: &Path) {
    std::fs::write(
        dir.join("engine.toml"),
        "[engine]\nlog_level = \"info\"\n\n[entry]\ntype = \"lua\"\nscript = \"app.lua\"\n",
    )
    .expect("engine.toml");
}

/// A full-assert application: methods, events, the async-reply path, and
/// ticks through the plugin, then idles for SIGINT.
const APP_LUA: &str = r#"
local reply = probe.echo("hello")
assert(reply.echo == "hello", "echo mismatch")

-- events: Lua emit -> plugin; each call yields to the actor so the
-- event can be delivered between calls
noisy.emit("app.poke", { n = 1 })
local attempts = 0
while probe.events().events < 1 do
    attempts = attempts + 1
    assert(attempts < 100000, "plugin never received app.poke")
end

-- unknown method -> error surfaced to Lua
local ok = pcall(function() return probe.nope() end)
assert(not ok, "unknown method should fail")

-- async call path: the reply arrives later via noisy_module_reply
probe.call_self()
local last = probe.last_call()
attempts = 0
while last == nil or last.id == nil do
    attempts = attempts + 1
    assert(attempts < 100000, "async reply never arrived")
    last = probe.last_call()
end
assert(last.id == 0, "first async call id is 0")
assert(last.value.echo == "self", "async reply value")

-- tick: the exported heartbeat, default interval
attempts = 0
while probe.tick_count().ticks < 1 do
    attempts = attempts + 1
    assert(attempts < 100000, "tick never fired")
end

print("lua asserts ok: echo, events, unknown-method, async-reply, tick")
"#;

struct Engine {
    child: Child,
    stdout_lines: Receiver<String>,
    stderr_lines: Receiver<String>,
}

impl Engine {
    /// Spawns `noisy <dir> [args]`; stdout lines and stderr lines stream
    /// back through channels.
    fn spawn(dir: &Path, extra_args: &[&str]) -> Self {
        let mut child = Command::new(ENGINE)
            .arg(dir)
            .args(extra_args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("engine binary starts");
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        let (stdout_tx, stdout_lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if stdout_tx.send(line).is_err() {
                    break;
                }
            }
        });
        let (stderr_tx, stderr_lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                if stderr_tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdout_lines,
            stderr_lines,
        }
    }

    /// Waits for a stdout line containing `marker`.
    fn wait_for_stdout(&self, marker: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            match self.stdout_lines.try_recv() {
                Ok(line) if line.contains(marker) => return,
                Ok(_) => {}
                Err(TryRecvError::Empty) => {
                    assert!(
                        Instant::now() < deadline,
                        "stdout marker `{marker}` never appeared"
                    );
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(TryRecvError::Disconnected) => {
                    panic!("engine stdout closed before `{marker}` appeared");
                }
            }
        }
    }

    /// Collects stderr until EOF (after the engine exits).
    fn stderr_text(&self) -> String {
        let deadline = Instant::now() + EXIT_TIMEOUT;
        let mut text = String::new();
        loop {
            match self.stderr_lines.try_recv() {
                Ok(line) => {
                    text.push_str(&line);
                    text.push('\n');
                }
                Err(TryRecvError::Empty) => {
                    assert!(
                        Instant::now() < deadline,
                        "stderr never closed"
                    );
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(TryRecvError::Disconnected) => return text,
            }
        }
    }

    /// Sends SIGINT and expects an exit within `EXIT_TIMEOUT`.
    fn sigint_and_wait(&mut self) -> Option<i32> {
        unsafe {
            libc::kill(self.child.id() as libc::c_int, libc::SIGINT);
        }
        self.wait_for_exit(EXIT_TIMEOUT)
    }

    /// Waits for the engine to exit on its own.
    fn wait_for_exit(&mut self, timeout: Duration) -> Option<i32> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait().expect("child status is readable") {
                Some(status) => return status.code(),
                None if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                None => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    panic!("engine did not exit within {timeout:?}");
                }
            }
        }
    }
}

#[test]
fn full_app_runs_and_sigint_is_clean() {
    let dir = fixture("good", "testplugin");
    write_config(&dir);
    std::fs::write(dir.join("app.lua"), APP_LUA).expect("app.lua");

    let mut engine = Engine::spawn(&dir, &[]);
    engine.wait_for_stdout("lua asserts ok", START_TIMEOUT);
    let code = engine.sigint_and_wait();
    assert_eq!(code, Some(0), "clean exit after SIGINT");
}

#[test]
fn wrong_abi_plugin_fails_readable() {
    let dir = fixture("badabi", "badplugin");
    write_config(&dir);
    std::fs::write(dir.join("app.lua"), "print(\"never reached\")").expect("app.lua");

    let mut engine = Engine::spawn(&dir, &[]);
    let code = engine.wait_for_exit(START_TIMEOUT);
    assert_ne!(code, Some(0), "boot failure must not exit 0");
    let stderr = engine.stderr_text();
    assert!(
        stderr.contains("ABI version mismatch"),
        "readable ABI failure, got: {stderr}"
    );
}

#[test]
fn entry_script_failure_exits_nonzero() {
    let dir = fixture("scriptfail", "testplugin");
    write_config(&dir);
    std::fs::write(
        dir.join("app.lua"),
        "assert(false, \"script blew up\")",
    )
    .expect("app.lua");

    let mut engine = Engine::spawn(&dir, &[]);
    let code = engine.wait_for_exit(START_TIMEOUT);
    assert_ne!(code, Some(0), "script failure must not exit 0");
    let stderr = engine.stderr_text();
    assert!(
        stderr.contains("script blew up"),
        "script failure surfaces on stderr: {stderr}"
    );
}

#[test]
fn check_passes_a_good_script() {
    let dir = fixture("checkgood", "testplugin");
    write_config(&dir);
    std::fs::write(dir.join("app.lua"), "local x = 1\nassert(x == 1)\n").expect("app.lua");

    let mut engine = Engine::spawn(&dir, &["--check"]);
    let code = engine.wait_for_exit(START_TIMEOUT);
    assert_eq!(code, Some(0), "--check exits 0 for a valid script");
}

#[test]
fn check_catches_a_broken_script() {
    let dir = fixture("checkbad", "testplugin");
    write_config(&dir);
    std::fs::write(dir.join("app.lua"), "do end)\n").expect("app.lua");

    let mut engine = Engine::spawn(&dir, &["--check"]);
    let code = engine.wait_for_exit(START_TIMEOUT);
    assert_eq!(code, Some(1), "--check exits 1 for a broken script");
    let stderr = engine.stderr_text();
    assert!(
        stderr.contains("--check:"),
        "check failure is readable: {stderr}"
    );
}
