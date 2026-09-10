pub mod api;
pub mod config;
pub mod crypto_api;
pub mod runtime;
pub mod sqlite_api;
pub mod template;
pub mod threads;
pub mod web;

use std::path::{Path, PathBuf};

/// Validate C-module directories and build the `package.cpath` they imply;
/// `None` (an empty list) leaves native modules disabled.
///
/// Each must exist and sit *outside* the serve directory — a `.so` below it
/// would also be downloadable as a static file.
pub fn resolve_c_module_dirs(dirs: &[PathBuf], serve_dir: &Path) -> Result<Option<String>, String> {
    if dirs.is_empty() {
        return Ok(None);
    }
    let mut parts = Vec::with_capacity(dirs.len());
    for dir in dirs {
        let abs = dir
            .canonicalize()
            .map_err(|e| format!("C module directory '{}' is not usable: {e}", dir.display()))?;
        if !abs.is_dir() {
            return Err(format!(
                "C module directory '{}' is not a directory",
                abs.display()
            ));
        }
        if abs.starts_with(serve_dir) {
            return Err(format!(
                "C module directory '{}' must not be inside the serve directory '{}' — \
                 a .so there would also be downloadable as a static file",
                abs.display(),
                serve_dir.display()
            ));
        }
        // `?.so` covers submodules: require("socket.core") -> socket/core.so.
        parts.push(format!("{}/?.so", abs.display()));
    }
    Ok(Some(parts.join(";")))
}

/// Create and canonicalize the data directory and its upload spool.
///
/// Refuses a data directory inside the serve directory — databases and
/// uploads must never be reachable as static files. Stale spool files from
/// previous runs are swept (no requests are in flight yet).
pub fn setup_data_dir(data_dir: &Path, serve_dir: &Path) -> Result<(PathBuf, PathBuf), String> {
    std::fs::create_dir_all(data_dir)
        .map_err(|e| format!("cannot create data directory '{}': {e}", data_dir.display()))?;
    let data_dir = data_dir
        .canonicalize()
        .map_err(|e| format!("data directory '{}' is not usable: {e}", data_dir.display()))?;
    if data_dir.starts_with(serve_dir) {
        return Err(format!(
            "data directory '{}' must not be inside the serve directory '{}' — \
             databases and uploads would become downloadable",
            data_dir.display(),
            serve_dir.display()
        ));
    }

    let spool_dir = data_dir.join(".spool");
    std::fs::create_dir_all(&spool_dir)
        .map_err(|e| format!("cannot create spool directory: {e}"))?;
    if let Ok(entries) = std::fs::read_dir(&spool_dir) {
        for entry in entries.flatten() {
            if std::fs::remove_file(entry.path()).is_ok() {
                log::debug!("swept stale spool file {}", entry.path().display());
            }
        }
    }

    Ok((data_dir, spool_dir))
}
