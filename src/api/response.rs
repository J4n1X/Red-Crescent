//! Control flow and response helpers: `exit`, `redirect`, `set_cookie` and
//! `send_file`.

use mlua::{Function, Lua, Table};

use super::with_state;
use crate::resolve::is_private;
use crate::runtime::{ExitSignal, LuaCookie, RenderConfig, SendFileSpec};

pub(super) fn install_control_flow(lua: &Lua) -> mlua::Result<()> {
    let globals = lua.globals();

    globals.set(
        "exit",
        lua.create_function(|_, ()| -> mlua::Result<()> {
            Err(mlua::Error::external(ExitSignal))
        })?,
    )?;

    // The redirect is mirrored into Rust-side state *before* raising the exit
    // signal, so even a template pcall around redirect() can't lose it.
    globals.set(
        "redirect",
        lua.create_function(
            move |_, (url, status): (String, Option<u16>)| -> mlua::Result<()> {
                let status = status.unwrap_or(302);
                if !(300..=399).contains(&status) {
                    return Err(mlua::Error::runtime(format!(
                        "redirect: status must be a 3xx code, got {status}"
                    )));
                }
                with_state(|st| {
                    *st.redirect.borrow_mut() = Some((url, status));
                    Ok(())
                })?;
                Err(mlua::Error::external(ExitSignal))
            },
        )?,
    )?;

    Ok(())
}

pub(super) fn set_cookie(lua: &Lua) -> mlua::Result<Function> {
    lua.create_function(move |_, spec: Table| {
        let cookie = LuaCookie {
            name: spec
                .get::<Option<String>>("name")?
                .ok_or_else(|| mlua::Error::runtime("set_cookie: 'name' field is required"))?,
            value: spec
                .get::<Option<String>>("value")?
                .ok_or_else(|| mlua::Error::runtime("set_cookie: 'value' field is required"))?,
            path: spec.get("path")?,
            domain: spec.get("domain")?,
            max_age: spec.get("max_age")?,
            http_only: spec.get::<Option<bool>>("http_only")?.unwrap_or(false),
            secure: spec.get::<Option<bool>>("secure")?.unwrap_or(false),
            same_site: spec.get("same_site")?,
        };
        if let Some(ss) = &cookie.same_site
            && !["strict", "lax", "none"].contains(&ss.to_lowercase().as_str())
        {
            return Err(mlua::Error::runtime(format!(
                "set_cookie: same_site must be 'strict', 'lax' or 'none', got '{ss}'"
            )));
        }
        with_state(|st| {
            st.cookies.borrow_mut().push(cookie);
            Ok(())
        })
    })
}

/// Serves a file from the data or serve directory instead of the rendered
/// output. The async side handles delivery: ranges, ETag, disposition.
pub(super) fn send_file(lua: &Lua, cfg: &RenderConfig) -> mlua::Result<Function> {
    let data_dir = cfg.data_dir.clone();
    let serve_dir = cfg.serve_dir.clone();
    lua.create_function(move |_, (path, opts): (String, Option<Table>)| {
        let abs = std::path::Path::new(&path)
            .canonicalize()
            .map_err(|_| mlua::Error::runtime(format!("send_file: file not found: {path}")))?;
        if !abs.starts_with(&data_dir) && !abs.starts_with(&serve_dir) {
            return Err(mlua::Error::runtime(format!(
                "send_file: path is outside the data and serve directories: {path}"
            )));
        }
        if !abs.is_file() {
            return Err(mlua::Error::runtime(format!(
                "send_file: not a regular file: {path}"
            )));
        }
        // Inside the serve directory the same rule as the static file
        // server applies: server-side source never reaches a client, by
        // any route. Otherwise an app that passes a user-controlled path
        // to send_file would hand out its own modules and config. The
        // data directory is exempt — a .lua file there is user content,
        // not source. (The two directories are disjoint: startup refuses
        // a data directory inside the serve directory.)
        if !abs.starts_with(&data_dir) && is_private(&serve_dir, &abs) {
            return Err(mlua::Error::runtime(format!(
                "send_file: refusing to send Lua source or a hidden file \
                 from the serve directory: {path}"
            )));
        }
        let (download_name, content_type) = match &opts {
            Some(o) => (o.get("download_name")?, o.get("content_type")?),
            None => (None, None),
        };
        with_state(|st| {
            *st.send_file.borrow_mut() = Some(SendFileSpec {
                path: abs,
                download_name,
                content_type,
            });
            Ok(())
        })
    })
}
