//! C++ SDK end-to-end tests (implementation plan §Phase 5 acceptance): the
//! real engine against the real examples, built by the one C++ build path
//! (`scripts/bash/build_cpp_examples.sh`) into `target/cpp/`, copied under
//! `target/e2e-cpp-*` — never committed. Acceptance:
//!
//! - cpp_module: Lua calls a C++ method and gets a structured reply; a C++
//!   slot receives a bus event and answers back into Lua (the app asserts
//!   all of it and prints the success marker).
//! - cpp_app: a C++ module as the application entry runs to completion;
//!   SIGINT exits 0.
//!
//! Requires a C++20 compiler in PATH (`CXX=c++` by default, set
//! `CXX=g++-13` in CI); fixtures live under `target/` (gitignored).

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

/// The built binary (`CARGO_BIN_EXE_noisy` from the engine package).
const ENGINE: &str = env!("CARGO_BIN_EXE_noisy");

const START_TIMEOUT: Duration = Duration::from_secs(30);
const EXIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Builds both example modules once per test process through the pinned
/// script (the same path CI's C++ stage uses) — `cargo test` never builds
/// them itself.
fn ensure_cpp_examples_built() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if target_cpp().join("libmysensor.so").is_file()
            && target_cpp().join("libmyapp.so").is_file()
        {
            return;
        }
        let output = Command::new("bash")
            .arg("scripts/bash/build_cpp_examples.sh")
            .current_dir(workspace_root())
            .output()
            .expect("the C++ build script spawns");
        assert!(
            output.status.success(),
            "building the C++ examples failed: {}",
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

/// The `target/cpp` directory `scripts/bash/build_cpp_examples.sh` writes.
fn target_cpp() -> PathBuf {
    workspace_root().join("target/cpp")
}

/// Recreates `target/e2e-cpp-<name>/{modules}` and copies the module in.
fn fixture(name: &str, library: &str) -> PathBuf {
    ensure_cpp_examples_built();
    let dir = workspace_root()
        .join("target")
        .join(format!("e2e-cpp-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("modules")).expect("fixture modules dir");
    std::fs::copy(
        target_cpp().join(format!("lib{library}.so")),
        dir.join("modules").join(format!("lib{library}.so")),
    )
    .expect("the example cdylib was built");
    dir
}

struct Engine {
    child: Child,
    stdout_lines: Receiver<String>,
    stderr_lines: Receiver<String>,
}

impl Engine {
    fn spawn(dir: &Path) -> Self {
        let mut child = Command::new(ENGINE)
            .arg(dir)
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
                    assert!(Instant::now() < deadline, "stderr never closed");
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(TryRecvError::Disconnected) => return text,
            }
        }
    }

    fn sigint_and_wait(&mut self) -> Option<i32> {
        unsafe {
            libc::kill(self.child.id() as libc::c_int, libc::SIGINT);
        }
        self.wait_for_exit(EXIT_TIMEOUT)
    }

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

/// The cpp_module acceptance: structured reply, slot answering back into
/// Lua, and the tick heartbeat — asserted inside the Lua app.
#[test]
fn cpp_module_serves_lua_and_answers_the_bus() {
    let dir = fixture("module", "mysensor");
    std::fs::copy(
        workspace_root().join("examples/cpp_module/app.lua"),
        dir.join("app.lua"),
    )
    .expect("example app.lua exists");
    std::fs::copy(
        workspace_root().join("examples/cpp_module/engine.toml"),
        dir.join("engine.toml"),
    )
    .expect("example engine.toml exists");

    let mut engine = Engine::spawn(&dir);
    engine.wait_for_stdout("cpp_module: e2e ok", START_TIMEOUT);
    let code = engine.sigint_and_wait();
    assert_eq!(code, Some(0), "clean exit after SIGINT");
    let stderr = engine.stderr_text();
    assert!(!stderr.contains("panic"), "engine stderr: {stderr}");
}

/// The cpp_app acceptance: a C++ module as the application entry runs to
/// completion (it hashes through `crypto` with call_async and logs the
/// digest), then SIGINT exits 0.
#[test]
fn cpp_app_runs_as_the_entry_point() {
    let dir = fixture("app", "myapp");
    std::fs::copy(
        workspace_root().join("examples/cpp_app/engine.toml"),
        dir.join("engine.toml"),
    )
    .expect("example engine.toml exists");

    let mut engine = Engine::spawn(&dir);
    engine.wait_for_stdout("myapp: ready", START_TIMEOUT);
    let code = engine.sigint_and_wait();
    assert_eq!(code, Some(0), "clean exit after SIGINT");
    let stderr = engine.stderr_text();
    assert!(!stderr.contains("panic"), "engine stderr: {stderr}");
}
