//! `require` and `include`, both confined to the serve directory.

use std::path::Path;
use std::sync::Arc;

use mlua::{Function, Lua, Table, Value};

use super::with_state;
use crate::resolve::{Resolved, display_name, is_lhtml, resolve};
use crate::runtime::RenderConfig;
use crate::template::TemplateCache;

const MAX_INCLUDE_DEPTH: u32 = 16;

/// Confines `package.path` to the serve directory, and gives background
/// threads a searcher that goes through the chunk cache.
pub(super) fn install_package(lua: &Lua, cfg: &RenderConfig) -> mlua::Result<()> {
    if let Ok(package) = lua.globals().get::<Table>("package") {
        let dir = cfg.serve_dir.display();
        package.set("path", format!("{dir}/?.lua;{dir}/?/init.lua"))?;
        // Empty unless C-module directories were configured.
        package.set("cpath", cfg.c_module_path.as_deref().unwrap_or(""))?;

        // Background threads only: requests get their own `require` from the
        // environment builder, since `package.loaded` persists on a pooled
        // state. Goes through the chunk cache either way.
        if let Ok(searchers) = package.get::<Table>("searchers") {
            let serve_dir = cfg.serve_dir.clone();
            let cache = Arc::clone(&cfg.cache);
            searchers.set(
                2,
                lua.create_function(move |lua, name: String| {
                    // Stock protocol: loader + filename, or the paths tried.
                    Ok(match find_module(&cache, &serve_dir, &name)? {
                        Found::Chunk(chunk, abs) => (
                            Value::Function(cache.chunk(lua, &chunk, None)?),
                            Value::String(lua.create_string(abs.to_string_lossy().as_bytes())?),
                        ),
                        Found::Missing(tried) => {
                            (Value::String(lua.create_string(&tried)?), Value::Nil)
                        }
                    })
                })?,
            )?;
        }
    }
    Ok(())
}

enum Found {
    Chunk(Arc<crate::template::CompiledChunk>, std::path::PathBuf),
    /// A `no file '...'` line per path tried, as stock `require` reports.
    Missing(String),
}

/// Resolve a module name like the stock searcher: `?.lua` then `?/init.lua`,
/// dots becoming separators. Resolved like a request path, so a crafted name
/// cannot reach outside the serve directory.
fn find_module(cache: &TemplateCache, serve_dir: &Path, name: &str) -> mlua::Result<Found> {
    let rel = name.replace('.', std::path::MAIN_SEPARATOR_STR);
    let mut tried = String::new();

    for candidate in [format!("{rel}.lua"), format!("{rel}/init.lua")] {
        let (abs, meta) = match resolve(serve_dir, &candidate, None, |_| false) {
            Resolved::Found(f) if f.meta.is_some() || f.path.is_file() => (f.path, f.meta),
            _ => {
                let joined = serve_dir.join(&candidate);
                tried.push_str(&format!("\n\tno file '{}'", joined.display()));
                continue;
            }
        };

        let module = cache
            .load_module(&abs, meta.as_ref(), &candidate)
            .map_err(|e| mlua::Error::runtime(format!("require '{name}': {e}")))?;
        return Ok(Found::Chunk(module, abs));
    }

    Ok(Found::Missing(tried))
}

/// `search(name, env)` for the environment's `require`: the loader bound to
/// the requesting environment, and its path.
pub(super) fn search(lua: &Lua, cfg: &RenderConfig) -> mlua::Result<Function> {
    let serve_dir = cfg.serve_dir.clone();
    let cache = Arc::clone(&cfg.cache);
    lua.create_function(move |lua, (name, env): (String, Table)| {
        // Bound to the requesting environment, so module globals are
        // that request's and die with it.
        Ok(match find_module(&cache, &serve_dir, &name)? {
            Found::Chunk(module, abs) => (
                Value::Function(cache.chunk_for(lua, &module, &env)?),
                Value::String(lua.create_string(abs.to_string_lossy().as_bytes())?),
            ),
            Found::Missing(tried) => (Value::Nil, Value::String(lua.create_string(&tried)?)),
        })
    })
}

/// `include(rel, env)`: renders another template in place.
pub(super) fn include(lua: &Lua, cfg: &RenderConfig) -> mlua::Result<Function> {
    let serve_dir = cfg.serve_dir.clone();
    let cache = Arc::clone(&cfg.cache);
    lua.create_function(move |lua, (rel, env): (String, Table)| {
        with_state(|st| {
            if st.include_depth.get() >= MAX_INCLUDE_DEPTH {
                return Err(mlua::Error::runtime(format!(
                    "include: depth limit ({MAX_INCLUDE_DEPTH}) exceeded — recursive include?"
                )));
            }
            let rel_path = rel.trim_start_matches('/');
            let (abs, meta) = match resolve(&serve_dir, rel_path, None, |_| false) {
                Resolved::Found(found) => (found.path, found.meta),
                Resolved::Missing => {
                    return Err(mlua::Error::runtime(format!(
                        "include: file not found: {rel}"
                    )));
                }
                Resolved::Escaped(_) => {
                    return Err(mlua::Error::runtime(format!(
                        "include: path escapes the serve directory: {rel}"
                    )));
                }
            };
            if !is_lhtml(&abs) {
                return Err(mlua::Error::runtime(
                    "include: only .lhtml files can be included (use require() for .lua modules)",
                ));
            }

            let display_name = display_name(&serve_dir, &abs, &rel);
            let template = cache
                .load(&abs, meta.as_ref(), &display_name)
                .map_err(|e| mlua::Error::runtime(format!("include: {e}")))?;

            let func = cache.chunk_for(lua, &template, &env)?;

            st.include_depth.set(st.include_depth.get() + 1);
            let result = func.call::<()>(());
            st.include_depth.set(st.include_depth.get() - 1);
            result
        })
    })
}
