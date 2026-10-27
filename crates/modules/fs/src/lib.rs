//! The `fs` module (specification §4.3).
//!
//! The filesystem surface runs over `tokio::fs`; watching uses one
//! `inotify` instance. The event stream lives in a dedicated watcher task
//! fed by the actor through an acked command channel, so `watch` replies
//! only after the kernel registration and a write following it cannot be
//! missed.
//!
//! Event mapping (`path` payloads are the affected paths):
//!
//! | inotify                                 | `fs.*` signal                      |
//! |-----------------------------------------|------------------------------------|
//! | `CREATE` (+`ISDIR`)                     | `file_created` / `dir_created`     |
//! | `MODIFY`                                | `file_modified`                    |
//! | `DELETE`, `MOVED_FROM` (+`ISDIR`)       | `file_deleted` / `dir_deleted`     |
//! | `MOVED_TO` (+`ISDIR`)                   | `file_created` / `dir_created`     |
//! | `Q_OVERFLOW`                            | `watch_error { path, error }`      |
//! | `DELETE_SELF`, `MOVE_SELF`, `IGNORED`   | `watch_error` + watch teardown     |
//!
//! A file watch registers on the parent directory and filters events by
//! the file name; a directory watch sees the directory's children.
//! Watching an already-watched path is idempotent (`unwatch` reports
//! whether the path was watched).
//!
//! Binary payloads (specification §3.5): `read_bytes` returns the file as
//! a UTF-8 string when it is valid UTF-8, otherwise as
//! `{ data = <base64>, encoding = "base64" }`; `write`/`append` accept
//! either shape for `content`.

use glob::glob as glob_iter;
use inotify::{EventMask, EventOwned, Inotify, WatchDescriptor, WatchMask};
use noisy_core::error::ModuleError;
use noisy_core::module::{
    Command, HostCtx, MethodCall, ModuleContext, ModuleDescriptor, NoisyModule,
};
use serde_json::{json, Value};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use tokio::io::unix::AsyncFd;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Events a module-side watch registers for.
fn watch_mask() -> WatchMask {
    WatchMask::CREATE
        | WatchMask::MODIFY
        | WatchMask::DELETE
        | WatchMask::MOVED_FROM
        | WatchMask::MOVED_TO
        | WatchMask::DELETE_SELF
        | WatchMask::MOVE_SELF
}

/// One active inotify registration, tracked for event mapping.
struct ActiveWatch {
    wd: WatchDescriptor,
    /// The watched path as the caller gave it (canonicalized to absolute
    /// when it exists so payload paths stay recognizable).
    path: PathBuf,
    /// Directory target: events on the directory's children. File target:
    /// events on the parent directory filtered down to this file name.
    target: WatchTarget,
}

enum WatchTarget {
    Dir,
    File {
        /// The file's bare name under the watched parent directory.
        name: OsString,
        /// The file's path (the payload path for its events).
        path: PathBuf,
    },
}

impl ActiveWatch {
    /// Maps one event under this watch: `Some((signal kind, payload path))`.
    fn map(&self, mask: EventMask, event_name: Option<&OsStr>) -> Option<(&'static str, PathBuf)> {
        let is_dir = mask.contains(EventMask::ISDIR);
        match &self.target {
            WatchTarget::Dir => {
                let name = event_name?;
                let path = self.path.join(name);
                if mask.intersects(EventMask::CREATE) {
                    return Some(if is_dir { ("dir_created", path) } else { ("file_created", path) });
                }
                if mask.intersects(EventMask::MODIFY) {
                    return Some(("file_modified", path));
                }
                if mask.intersects(EventMask::DELETE | EventMask::MOVED_FROM) {
                    return Some(if is_dir { ("dir_deleted", path) } else { ("file_deleted", path) });
                }
                if mask.intersects(EventMask::MOVED_TO) {
                    return Some(if is_dir { ("dir_created", path) } else { ("file_created", path) });
                }
                None
            }
            WatchTarget::File { name, path } => {
                let event_name = event_name?;
                if event_name != name.as_os_str() {
                    return None; // A sibling's event under the shared parent.
                }
                if mask.intersects(EventMask::CREATE | EventMask::MOVED_TO) {
                    return Some(("file_created", path.clone()));
                }
                if mask.intersects(EventMask::MODIFY) {
                    return Some(("file_modified", path.clone()));
                }
                if mask.intersects(EventMask::DELETE | EventMask::MOVED_FROM) {
                    return Some(("file_deleted", path.clone()));
                }
                None
            }
        }
    }

    /// Self-teardown bits: the kernel dropped the watch (target deleted,
    /// moved, or unmounted).
    fn went_away(mask: EventMask) -> bool {
        mask.intersects(EventMask::DELETE_SELF | EventMask::MOVE_SELF | EventMask::IGNORED)
    }
}

/// Watcher commands from the method actor; both are acked.
enum WatchCmd {
    /// Register a watch; the ack resolves after the kernel registration.
    Add {
        path: PathBuf,
        ack: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    /// Remove a watch by path; the ack reports whether it was watched.
    Remove {
        path: PathBuf,
        ack: tokio::sync::oneshot::Sender<bool>,
    },
}

/// Serves one method call (the synchronous filesystem surface). Watching
/// goes through the command channel to the watcher task.
async fn serve(call: &MethodCall, cmds: &mpsc::Sender<WatchCmd>) -> Result<Value, ModuleError> {
    let params = call.params().clone();
    match call.method() {
        "read" => {
            let path = expect_path(&params, call)?;
            let bytes =
                tokio::fs::read(&path).await.map_err(read_failed(&path, call.method()))?;
            String::from_utf8(bytes)
                .map(Value::String)
                .map_err(|_| non_utf8_content(call))
        }
        "read_bytes" => {
            let path = expect_path(&params, call)?;
            let bytes =
                tokio::fs::read(&path).await.map_err(read_failed(&path, call.method()))?;
            Ok(match String::from_utf8(bytes) {
                Ok(text) => Value::String(text),
                Err(err) => binary_convention(err.into_bytes()),
            })
        }
        "write" | "append" => {
            let path = expect_path(&params, call)?;
            let content = decode_content(params.get("content"), call)?;
            if call.method() == "write" {
                tokio::fs::write(&path, &content)
                    .await
                    .map_err(write_failed(&path, call.method()))?;
            } else {
                let mut file = tokio::fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(&path)
                    .await
                    .map_err(write_failed(&path, call.method()))?;
                file.write_all(&content)
                    .await
                    .map_err(write_failed(&path, call.method()))?;
            }
            Ok(json!({}))
        }
        "delete" => {
            let path = expect_path(&params, call)?;
            let meta =
                tokio::fs::metadata(&path).await.map_err(read_failed(&path, call.method()))?;
            if meta.is_dir() {
                tokio::fs::remove_dir_all(&path)
                    .await
                    .map_err(write_failed(&path, call.method()))?;
            } else {
                tokio::fs::remove_file(&path)
                    .await
                    .map_err(write_failed(&path, call.method()))?;
            }
            Ok(json!({}))
        }
        "exists" => {
            let path = expect_path(&params, call)?;
            Ok(json!(tokio::fs::symlink_metadata(&path).await.is_ok()))
        }
        "list" => {
            let path = expect_path(&params, call)?;
            let mut names = Vec::new();
            let mut dir =
                tokio::fs::read_dir(&path).await.map_err(read_failed(&path, call.method()))?;
            while let Some(entry) = dir.next_entry().await.map_err(read_failed(&path, call.method()))? {
                names.push(entry.file_name().to_string_lossy().into_owned());
            }
            names.sort();
            Ok(json!(names))
        }
        "mkdir" => {
            let path = expect_path(&params, call)?;
            tokio::fs::create_dir_all(&path)
                .await
                .map_err(write_failed(&path, call.method()))?;
            Ok(json!({}))
        }
        "move" | "copy" => {
            let src = expect_path(&params, call)?;
            let Some(dst) = params["dst"].as_str() else {
                return Err(bad_param(call, "`src` and `dst` must be strings"));
            };
            let dst = PathBuf::from(dst);
            if call.method() == "move" {
                tokio::fs::rename(&src, &dst).await.map_err(|err| {
                    ModuleError::failed(
                        "fs",
                        format!("fs.move: {} → {}: {err}", src.display(), dst.display()),
                    )
                })?;
                Ok(json!({}))
            } else {
                let copied = tokio::fs::copy(&src, &dst).await.map_err(|err| {
                    ModuleError::failed(
                        "fs",
                        format!("fs.copy: {} → {}: {err}", src.display(), dst.display()),
                    )
                })?;
                Ok(json!(copied))
            }
        }
        "stat" => {
            let path = expect_path(&params, call)?;
            let meta =
                tokio::fs::metadata(&path).await.map_err(read_failed(&path, call.method()))?;
            Ok(json!({
                "size": meta.len(),
                "modified": meta
                    .modified()
                    .ok()
                    .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis())
                    .unwrap_or(0),
                "is_dir": meta.is_dir(),
                "is_file": meta.is_file(),
            }))
        }
        "glob" => {
            let Some(pattern) = params["pattern"].as_str() else {
                return Err(bad_param(call, "`pattern` must be a string"));
            };
            let mut paths = Vec::new();
            let matches = glob_iter(pattern)
                .map_err(|err| ModuleError::failed("fs", format!("fs.glob: {err}")))?;
            for entry in matches {
                match entry {
                    Ok(path) => paths.push(path.to_string_lossy().into_owned()),
                    Err(err) => {
                        tracing::warn!(error = %err, "fs.glob: unreadable entry skipped");
                    }
                }
            }
            paths.sort();
            Ok(json!(paths))
        }
        "watch" | "unwatch" => {
            let path = expect_path(&params, call)?;
            if call.method() == "watch" {
                let (ack, ack_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
                cmds.send(WatchCmd::Add { path, ack })
                    .await
                    .map_err(|_| ModuleError::unavailable("fs"))?;
                ack_rx
                    .await
                    .map_err(|_| ModuleError::unavailable("fs"))?
                    .map_err(|message| ModuleError::failed("fs", message))?;
                Ok(json!({}))
            } else {
                let (ack, ack_rx) = tokio::sync::oneshot::channel::<bool>();
                cmds.send(WatchCmd::Remove { path, ack })
                    .await
                    .map_err(|_| ModuleError::unavailable("fs"))?;
                Ok(json!(ack_rx.await.unwrap_or(false)))
            }
        }
        other => Err(ModuleError::unknown_method("fs", other)),
    }
}

// ───────────────────── parameter plumbing

fn bad_param(call: &MethodCall, message: &str) -> ModuleError {
    ModuleError::failed("fs", format!("fs.{}: {message}", call.method()))
}

fn non_utf8_content(call: &MethodCall) -> ModuleError {
    bad_param(
        call,
        "content is not valid UTF-8; pass { data = <base64>, encoding = \"base64\" }",
    )
}

fn expect_path(params: &Value, call: &MethodCall) -> Result<PathBuf, ModuleError> {
    match params["path"].as_str().or_else(|| params["src"].as_str()) {
        Some(path) => Ok(PathBuf::from(path)),
        None => Err(bad_param(call, "`path` must be a string")),
    }
}

fn read_failed(path: &Path, method: &str) -> impl Fn(std::io::Error) -> ModuleError + 'static {
    let where_text = format!("fs.{method}: {}", path.display());
    move |err| ModuleError::failed("fs", format!("{where_text}: {err}"))
}

fn write_failed(path: &Path, method: &str) -> impl Fn(std::io::Error) -> ModuleError + 'static {
    let where_text = format!("fs.{method}: {}", path.display());
    move |err| ModuleError::failed("fs", format!("{where_text}: {err}"))
}

/// `read_bytes` result for non-UTF-8 content (specification §3.5).
fn binary_convention(bytes: Vec<u8>) -> Value {
    use base64::Engine as _;
    json!({
        "data": base64::engine::general_purpose::STANDARD.encode(bytes),
        "encoding": "base64",
    })
}

/// Writes/append `content`: a UTF-8 string or the binary convention object.
fn decode_content(content: Option<&Value>, call: &MethodCall) -> Result<Vec<u8>, ModuleError> {
    let Some(content) = content else {
        return Err(bad_param(call, "`content` is required"));
    };
    match content {
        Value::String(text) => Ok(text.as_bytes().to_vec()),
        Value::Object(fields)
            if fields.get("encoding").and_then(Value::as_str) == Some("base64") =>
        {
            use base64::Engine as _;
            let data = fields["data"].as_str().ok_or_else(|| {
                bad_param(call, "`data` must be a base64 string when encoding is given")
            })?;
            base64::engine::general_purpose::STANDARD
                .decode(data)
                .map_err(|err| bad_param(call, &format!("`data` is not valid base64: {err}")))
        }
        _ => Err(non_utf8_content(call)),
    }
}

// ───────────────────── the watcher task

/// The `fs` module: method actor plus the dedicated inotify watcher task.
pub struct FsModule;

impl Default for FsModule {
    fn default() -> Self {
        Self::new()
    }
}

impl FsModule {
    pub fn new() -> Self {
        Self
    }
}

impl NoisyModule for FsModule {
    fn descriptor(&self) -> ModuleDescriptor {
        ModuleDescriptor::new("fs")
            .version("0.1.0")
            .signals([
                "file_created",
                "file_modified",
                "file_deleted",
                "dir_created",
                "dir_deleted",
                "watch_error",
            ])
            .method_params("read", ["path"])
            .method_params("read_bytes", ["path"])
            .method_params("write", ["path", "content"])
            .method_params("append", ["path", "content"])
            .method_params("delete", ["path"])
            .method_params("exists", ["path"])
            .method_params("list", ["path"])
            .method_params("mkdir", ["path"])
            .method_params("move", ["src", "dst"])
            .method_params("copy", ["src", "dst"])
            .method_params("stat", ["path"])
            .method_params("glob", ["pattern"])
            .method_params("watch", ["path"])
            .method_params("unwatch", ["path"])
    }

    fn start(self: Box<Self>, ctx: ModuleContext) -> JoinHandle<()> {
        let ModuleContext {
            mut commands,
            host,
            shutdown,
            config: _,
        } = ctx;
        tokio::spawn(async move {
            // The watcher owns the inotify instance and the watch
            // bookkeeping; the actor only forwards watch commands.
            let (cmds, cmd_rx) = mpsc::channel::<WatchCmd>(32);
            let watcher = tokio::spawn(run_watcher(cmd_rx, host.clone()));
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => break,
                    cmd = commands.recv() => match cmd {
                        Some(Command::MethodCall(call)) => {
                            let reply = serve(&call, &cmds).await;
                            call.reply(reply);
                        }
                        // The fs module issues no async calls; the watcher
                        // task runs fire-and-observe.
                        Some(_) => {}
                        None => break,
                    },
                }
            }
            watcher.abort();
            // Drain phase: precise shutdown failures for late callers.
            while let Some(cmd) = commands.recv().await {
                if let Command::MethodCall(call) = cmd {
                    call.reply(Err(ModuleError::ShuttingDown));
                }
            }
        })
    }
}

/// The watcher: one inotify instance, acked add/remove commands, event
/// mapping. All bookkeeping is local to this task.
async fn run_watcher(mut cmds: mpsc::Receiver<WatchCmd>, host: HostCtx) {
    let inotify = match Inotify::init() {
        Ok(inotify) => inotify,
        Err(err) => {
            tracing::error!(error = %err, "fs watcher: inotify init failed");
            fail_pending(&mut cmds).await;
            return;
        }
    };
    // SAFETY: the inotify fd comes from `inotify_init1`, is owned for the
    // watcher's whole lifetime, and is never replaced or closed while the
    // `AsyncFd` registration holds it (tokio #8587 contract).
    let mut evented = match unsafe {
        AsyncFd::register_with_interest(inotify, tokio::io::Interest::READABLE)
    } {
        Ok(evented) => evented,
        Err(err) => {
            tracing::error!(error = %err, "fs watcher: AsyncFd registration failed");
            fail_pending(&mut cmds).await;
            return;
        }
    };
    let mut watches: Vec<ActiveWatch> = Vec::new();
    let mut buffer = [0u8; 4096];

    loop {
        tokio::select! {
            biased;
            cmd = cmds.recv() => match cmd {
                Some(WatchCmd::Add { path, ack }) => {
                    let _ = ack.send(add_watch(&evented, &mut watches, path));
                }
                Some(WatchCmd::Remove { path, ack }) => {
                    let _ = ack.send(remove_watch(&evented, &mut watches, &path));
                }
                // All command senders dropped: the fs module actor is gone.
                None => break,
            },
            ready = evented.readable_mut() => match ready {
                Err(_closed) => break,
                Ok(mut guard) => {
                    match guard
                        .try_io(|evented| evented.get_mut().read_events(&mut buffer))
                    {
                        // All currently available events, drained at once.
                        Ok(Ok(events)) => {
                            for event in events {
                                handle_event(&mut watches, event.to_owned(), &host);
                            }
                        }
                        Ok(Err(err)) => {
                            tracing::warn!(error = %err, "fs watcher: event read failed");
                            host.emit(
                                "watch_error",
                                json!({ "path": Value::Null, "error": err.to_string() }),
                            );
                        }
                        // Readiness cleared by `try_io`; the loop resumes.
                        Err(_would_block) => {}
                    }
                }
            },
        }
    }
}

// ───────────────────── watcher internals

/// Kernel registration plus the local bookkeeping entry. Canonicalizes the
/// path so payload paths are absolute and consistent.
fn add_watch(
    evented: &AsyncFd<Inotify>,
    watches: &mut Vec<ActiveWatch>,
    requested: PathBuf,
) -> Result<(), String> {
    if watches.iter().any(|w| w.path == requested) {
        return Ok(()); // Idempotent watch.
    }

    let meta = std::fs::symlink_metadata(&requested)
        .map_err(|err| format!("fs.watch: {}: {err}", requested.display()))?;
    let path = std::fs::canonicalize(&requested)
        .map_err(|err| format!("fs.watch: {}: {err}", requested.display()))?;

    let (root, target) = if meta.is_dir() {
        (path.clone(), WatchTarget::Dir)
    } else {
        let name = path
            .file_name()
            .map(OsStr::to_os_string)
            .ok_or_else(|| format!("fs.watch: {} has no file name", requested.display()))?;
        let root = path
            .parent()
            .map(PathBuf::from)
            .ok_or_else(|| format!("fs.watch: {} has no parent", requested.display()))?;
        (root, WatchTarget::File { name, path: path.clone() })
    };

    let wd = evented
        .get_ref()
        .watches()
        .add(&root, watch_mask())
        .map_err(|err| format!("fs.watch: {}: {err}", requested.display()))?;
    watches.push(ActiveWatch { wd, path, target });
    Ok(())
}

/// Drops the bookkeeping entry and the kernel watch. `Ok(false)` when the
/// path was not watched; a lost kernel watch still tears the entry down.
fn remove_watch(
    evented: &AsyncFd<Inotify>,
    watches: &mut Vec<ActiveWatch>,
    requested: &PathBuf,
) -> bool {
    let Some(index) = watches.iter().position(|w| &w.path == requested) else {
        return false;
    };
    let entry = watches.swap_remove(index);
    if let Err(err) = evented.get_ref().watches().remove(entry.wd) {
        tracing::warn!(error = %err, path = %entry.path.display(), "fs.unwatch: kernel removal failed");
    }
    true
}

/// Maps one inotify event to `fs.*` signals and tears down dead watches.
fn handle_event(watches: &mut Vec<ActiveWatch>, event: EventOwned, host: &HostCtx) {
    if event.mask.contains(EventMask::Q_OVERFLOW) {
        host.emit(
            "watch_error",
            json!({ "path": Value::Null, "error": "event queue overflow" }),
        );
        return;
    }
    let event_name: Option<&OsStr> = event.name.as_deref();

    // Reverse iteration: teardown swaps entries out.
    for index in (0..watches.len()).rev() {
        let watch = &watches[index];
        if watch.wd != event.wd {
            continue;
        }
        if ActiveWatch::went_away(event.mask) {
            host.emit(
                "watch_error",
                json!({ "path": watch.path, "error": "watch target removed" }),
            );
            watches.swap_remove(index);
            continue;
        }
        if let Some((kind, payload_path)) = watch.map(event.mask, event_name) {
            host.emit(kind, json!({ "path": payload_path.display().to_string() }));
        }
    }
}

/// When the watcher cannot come up, every queued and future ack gets a
/// failure so callers do not hang on the ack channel.
async fn fail_pending(cmds: &mut mpsc::Receiver<WatchCmd>) {
    while let Some(cmd) = cmds.recv().await {
        match cmd {
            WatchCmd::Add { ack, .. } => {
                let _ = ack.send(Err("fs watcher is unavailable".into()));
            }
            WatchCmd::Remove { ack, .. } => {
                let _ = ack.send(false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The spec surface: every §4.3 method present in the descriptor.
    #[test]
    fn descriptor_declares_the_spec_surface() {
        let descriptor = FsModule::new().descriptor();
        descriptor.validate().expect("valid descriptor");
        for method in [
            "read", "read_bytes", "write", "append", "delete", "exists", "list", "mkdir",
            "move", "copy", "stat", "glob", "watch", "unwatch",
        ] {
            assert!(descriptor.method_decl(method).is_some(), "{method}");
        }
        assert_eq!(
            descriptor.signals,
            vec![
                "file_created",
                "file_modified",
                "file_deleted",
                "dir_created",
                "dir_deleted",
                "watch_error",
            ]
        );
    }

    /// Deleting/moving the watched directory itself produces the self-
    /// teardown path (`went_away`), not a spurious child event.
    #[test]
    fn teardown_masks_mark_a_watch_dead() {
        use inotify::EventMask;
        for mask in [
            EventMask::DELETE_SELF,
            EventMask::MOVE_SELF,
            EventMask::IGNORED,
        ] {
            assert!(ActiveWatch::went_away(mask), "{mask:?}");
        }
        assert!(!ActiveWatch::went_away(EventMask::CREATE));
    }
}
