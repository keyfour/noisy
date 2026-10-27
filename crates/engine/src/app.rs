//! Application assembly (specification §8, MODULE_GUIDE §1): the in-tree
//! module registry and the plugin search path.

use noisy_core::module::NoisyModule;
use std::path::{Path, PathBuf};

/// In-tree Rust modules, listed here per the module guide ("Rust module |
/// listed in `engine/src/app.rs`"). The foundation modules load in this
/// order: `log` first so every later log call gets mirrored from boot.
pub fn in_tree_modules() -> Vec<Box<dyn NoisyModule>> {
    vec![
        Box::new(noisy_log::LogModule::new()),
        Box::new(noisy_timer::TimerModule::new()),
        Box::new(noisy_fs::FsModule::new()),
        Box::new(noisy_crypto::CryptoModule::new()),
    ]
}

/// The resolved plugin search path (implementation plan §Phase 3):
/// `$NOISY_MODULE_PATH`, then `<app-dir>/modules/`, then the directories
/// from the config — deduplicated, order preserved.
pub fn plugin_dirs(app_dir: &Path, config_paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(env_paths) = std::env::var_os("NOISY_MODULE_PATH") {
        dirs.extend(
            std::env::split_paths(&env_paths)
                .filter(|p| !p.as_os_str().is_empty()),
        );
    }
    dirs.push(app_dir.join("modules"));
    dirs.extend(config_paths.iter().map(|p| app_dir.join(p)));
    // Remove duplicates, keep the first occurrence.
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|p| seen.insert(p.clone()));
    dirs
}

/// All candidate `.so` files under the search path, sorted for
/// deterministic load order.
pub fn plugin_files(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && path.extension().is_some_and(|ext| ext == "so") {
                files.push(path);
            }
        }
    }
    files.sort();
    files.dedup();
    files
}
