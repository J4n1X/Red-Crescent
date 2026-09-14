//! The Lua-facing `fs` module: the few filesystem operations Lua lacks.
//!
//! Lua can open and remove files but cannot create a directory or a hard
//! link, which is why applications reach for a shell. Both are confined to the
//! data directory, like `sqlite.open` and `send_file`.

use std::path::{Component, Path, PathBuf};

use mlua::Lua;

pub fn register(lua: &Lua, data_dir: &Path) -> mlua::Result<()> {
    let fs = lua.create_table()?;

    let dir = data_dir.to_path_buf();
    fs.set(
        "mkdir",
        lua.create_function(move |_, path: String| {
            let target = resolve(&dir, &path)?;
            std::fs::create_dir_all(&target).map_err(|e| {
                mlua::Error::runtime(format!("fs.mkdir: {}: {e}", target.display()))
            })?;
            Ok(true)
        })?,
    )?;

    let dir = data_dir.to_path_buf();
    fs.set(
        "link",
        lua.create_function(move |_, (from, to): (String, String)| {
            let src = resolve(&dir, &from)?;
            let dest = resolve(&dir, &to)?;
            // Hard link first: it copies no data. Falling back to a real copy
            // covers a destination on another filesystem, where linking cannot
            // work at all.
            if std::fs::hard_link(&src, &dest).is_ok() {
                return Ok(true);
            }
            match std::fs::copy(&src, &dest) {
                Ok(_) => Ok(true),
                Err(e) => Err(mlua::Error::runtime(format!(
                    "fs.link: {} -> {}: {e}",
                    src.display(),
                    dest.display()
                ))),
            }
        })?,
    )?;

    lua.globals().set("fs", &fs)?;
    lua.register_module("fs", fs)?;
    Ok(())
}

/// Absolute path inside the data directory, or an error. The parent is
/// canonicalized so a symlink cannot point the result outside.
fn resolve(data_dir: &Path, path: &str) -> mlua::Result<PathBuf> {
    let candidate = Path::new(path);
    let relative = if candidate.is_absolute() {
        candidate.strip_prefix(data_dir).map_err(|_| {
            mlua::Error::runtime(format!("fs: path is outside the data directory: {path}"))
        })?
    } else {
        candidate
    };
    if relative
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(mlua::Error::runtime(format!(
            "fs: path must stay inside the data directory: {path}"
        )));
    }

    let full = data_dir.join(relative);
    let parent = full
        .parent()
        .ok_or_else(|| mlua::Error::runtime("fs: path has no parent"))?;
    if let Ok(real) = parent.canonicalize()
        && !real.starts_with(data_dir)
    {
        return Err(mlua::Error::runtime(format!(
            "fs: path escapes the data directory: {path}"
        )));
    }
    Ok(full)
}
