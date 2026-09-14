//! The Lua-facing API.
//!
//! `install_core` registers the request-independent surface (`html_escape`,
//! `log`, `json`, `sqlite`, `crypto`, the `server` table, and the confined
//! `package.path`) — it is shared by request rendering and background jobs.
//! `install` adds the request-specific parts on top: output (`_out`,
//! `_out_expr`, `print`), `include()`, `exit()`/`redirect()`, and the
//! `request`/`response` tables.

use std::rc::Rc;
use std::sync::Arc;

use std::time::Duration;

use mlua::{Lua, LuaSerdeExt, MultiValue, Table, Value};

use crate::fs_api;
use crate::process_api;
use crate::runtime::{
    ExitSignal, LuaCookie, RenderConfig, RenderState, RequestBody, RequestData, SendFileSpec,
};
use crate::{crypto_api, sqlite_api};

const MAX_INCLUDE_DEPTH: u32 = 16;

/// Request-independent API, shared by page rendering and background jobs.
pub(crate) fn install_core(lua: &Lua, cfg: &RenderConfig) -> mlua::Result<()> {
    let globals = lua.globals();

    globals.set(
        "html_escape",
        lua.create_function(|_, input: String| Ok(html_escape(&input)))?,
    )?;

    // --- server info ----------------------------------------------------

    let server = lua.create_table()?;
    server.set("data_dir", cfg.data_dir.display().to_string())?;
    server.set("version", env!("CARGO_PKG_VERSION"))?;
    server.set("max_body_size", cfg.max_body_size)?;
    server.set("max_upload_size", cfg.max_upload_size)?;
    server.set("max_upload_files", cfg.max_upload_files)?;
    globals.set("server", server)?;

    // --- log ------------------------------------------------------------

    macro_rules! create_log_function {
        ($lua:expr, $level:ident) => {
            $lua.create_function(|lua, params: MultiValue| {
                let message = params_to_string(lua, params)?;
                log::$level!("{}", message);
                Ok(())
            })
        };
    }

    let log_table = lua.create_table()?;
    log_table.set("trace", create_log_function!(lua, trace)?)?;
    log_table.set("debug", create_log_function!(lua, debug)?)?;
    log_table.set("info", create_log_function!(lua, info)?)?;
    log_table.set("warn", create_log_function!(lua, warn)?)?;
    log_table.set("error", create_log_function!(lua, error)?)?;

    let log_metatable = lua.create_table()?;
    log_metatable.set(
        "__newindex",
        lua.create_function(
            |_, (_t, key, _v): (Table, String, Value)| -> mlua::Result<()> {
                Err(mlua::Error::runtime(format!(
                    "attempt to modify read-only log table: {key}"
                )))
            },
        )?,
    )?;
    log_table.set_metatable(Some(log_metatable))?;

    globals.set("log", log_table.clone())?;
    lua.register_module("log", log_table)?;

    // --- json -----------------------------------------------------------

    let json_table = lua.create_table()?;
    json_table.set(
        "encode",
        lua.create_function(|lua, (value, pretty): (Value, Option<bool>)| {
            let json: serde_json::Value = lua.from_value(value)?;
            let encoded = if pretty.unwrap_or(false) {
                serde_json::to_string_pretty(&json)
            } else {
                serde_json::to_string(&json)
            };
            encoded.map_err(mlua::Error::external)
        })?,
    )?;
    json_table.set(
        "decode",
        lua.create_function(|lua, text: String| {
            let json: serde_json::Value =
                serde_json::from_str(&text).map_err(mlua::Error::external)?;
            lua.to_value(&json)
        })?,
    )?;
    globals.set("json", json_table.clone())?;
    lua.register_module("json", json_table)?;

    // --- sqlite + crypto -------------------------------------------------

    sqlite_api::register(lua, &cfg.data_dir, cfg.sqlite_idle_connections)?;
    process_api::register(lua)?;
    fs_api::register(lua, &cfg.data_dir)?;
    crypto_api::register(lua)?;

    // --- background threads ----------------------------------------------

    let thread_table = lua.create_table()?;
    let spawn_cfg = cfg.clone();
    // spawn(name, script[, args]) -> spawned, id
    //
    // `args` travels as JSON: a Lua value belongs to the state that built it.
    // When `spawned` is false the args are ignored — that run has its own.
    thread_table.set(
        "spawn",
        lua.create_function(
            move |lua, (name, script, args): (String, String, Option<Value>)| {
                let args_json = match args {
                    None | Some(Value::Nil) => None,
                    Some(value) => {
                        let json: serde_json::Value = lua.from_value(value)?;
                        Some(serde_json::to_string(&json).map_err(mlua::Error::external)?)
                    }
                };
                let (id, spawned) =
                    crate::threads::spawn_named(spawn_cfg.clone(), &name, &script, args_json)
                        .map_err(mlua::Error::runtime)?;
                Ok((spawned, id as i64))
            },
        )?,
    )?;
    let registry = Arc::clone(&cfg.threads);
    thread_table.set(
        "running",
        lua.create_function(move |_, name: String| Ok(registry.is_running(&name)))?,
    )?;
    // status(id) -> "running" | "finished" | "failed" | "cancelled" | nil
    // Keyed by run id: a name may belong to a later run by the time you ask.
    let registry = Arc::clone(&cfg.threads);
    thread_table.set(
        "status",
        lua.create_function(move |_, id: i64| {
            Ok(registry
                .status(id.max(0) as u64)
                .map(crate::threads::RunStatus::as_str))
        })?,
    )?;
    // id() -> the calling thread's run id, nil on a request thread.
    thread_table.set(
        "id",
        lua.create_function(|_, ()| Ok(crate::threads::current_run_id().map(|id| id as i64)))?,
    )?;
    // kill(id) -> true if a stop was requested. Cooperative: raises a flag the
    // target checks on its next hook firing or sleep slice, so a thread parked
    // in os.execute will not notice until that call returns.
    let registry = Arc::clone(&cfg.threads);
    thread_table.set(
        "kill",
        lua.create_function(move |_, id: i64| Ok(registry.request_kill(id.max(0) as u64)))?,
    )?;
    // join(id[, seconds]) -> status, value
    //
    // Waits on a condvar. `value` is the return value when finished, the error
    // when failed, nil while running. Capped: a join holds the worker and the
    // timeout hook cannot fire inside it.
    let registry = Arc::clone(&cfg.threads);
    thread_table.set(
        "join",
        lua.create_function(move |lua, (id, seconds): (i64, Option<f64>)| {
            let id = id.max(0) as u64;
            if crate::threads::current_run_id() == Some(id) {
                return Err(mlua::Error::runtime(
                    "thread.join: a thread cannot join itself",
                ));
            }
            let seconds = seconds.unwrap_or(crate::threads::DEFAULT_JOIN_SECS);
            if !seconds.is_finite() || seconds < 0.0 {
                return Err(mlua::Error::runtime(
                    "thread.join: seconds must be zero or more",
                ));
            }
            let waited = Duration::from_secs_f64(seconds.min(crate::threads::MAX_JOIN_SECS));
            let Some((status, result, error)) = registry.join(id, waited) else {
                return Ok((Value::Nil, Value::Nil));
            };
            let value = match (status, result, error) {
                (crate::threads::RunStatus::Finished, Some(json), _) => {
                    let parsed: serde_json::Value =
                        serde_json::from_str(&json).map_err(mlua::Error::external)?;
                    lua.to_value(&parsed)?
                }
                (crate::threads::RunStatus::Failed, _, Some(err)) => {
                    Value::String(lua.create_string(&err)?)
                }
                _ => Value::Nil,
            };
            Ok((Value::String(lua.create_string(status.as_str())?), value))
        })?,
    )?;
    globals.set("thread", thread_table.clone())?;
    lua.register_module("thread", thread_table)?;

    // --- require: confine module search to the serve directory ----------

    if let Ok(package) = globals.get::<Table>("package") {
        let dir = cfg.serve_dir.display();
        package.set("path", format!("{dir}/?.lua;{dir}/?/init.lua"))?;
        // Empty unless C-module directories were configured.
        package.set("cpath", cfg.c_module_path.as_deref().unwrap_or(""))?;
    }

    Ok(())
}

/// Full request-rendering API: everything from [`install_core`] plus output,
/// control flow, include, and the request/response tables.
pub(crate) fn install(
    lua: &Lua,
    state: &Rc<RenderState>,
    cfg: &RenderConfig,
    request: &RequestData,
) -> mlua::Result<()> {
    install_core(lua, cfg)?;

    let globals = lua.globals();

    // --- output ---------------------------------------------------------
    // The buffer is raw bytes: Lua strings are byte strings, so binary
    // output survives untouched.

    // _out: internal, used by generated chunks for literal HTML segments.
    let st = Rc::clone(state);
    globals.set(
        "_out",
        lua.create_function(move |_, text: mlua::String| {
            st.out.borrow_mut().extend_from_slice(&text.as_bytes());
            Ok(())
        })?,
    )?;

    // _out_expr: internal, used for <?lua= expr ?>. nil prints nothing.
    let st = Rc::clone(state);
    globals.set(
        "_out_expr",
        lua.create_function(move |lua, values: MultiValue| {
            let mut out = st.out.borrow_mut();
            for value in values {
                if let Value::Nil = value {
                    continue;
                }
                append_value(lua, value, &mut out)?;
            }
            Ok(())
        })?,
    )?;

    // print: writes to the page, joining arguments with a space.
    let st = Rc::clone(state);
    globals.set(
        "print",
        lua.create_function(move |lua, params: MultiValue| {
            let mut out = st.out.borrow_mut();
            let mut first = true;
            for value in params {
                if !first {
                    out.push(b' ');
                }
                first = false;
                append_value(lua, value, &mut out)?;
            }
            Ok(())
        })?,
    )?;

    // --- control flow ---------------------------------------------------

    globals.set(
        "exit",
        lua.create_function(|_, ()| -> mlua::Result<()> {
            Err(mlua::Error::external(ExitSignal))
        })?,
    )?;

    // The redirect is mirrored into Rust-side state *before* raising the exit
    // signal, so even a template pcall around redirect() can't lose it.
    let st = Rc::clone(state);
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
                *st.redirect.borrow_mut() = Some((url, status));
                Err(mlua::Error::external(ExitSignal))
            },
        )?,
    )?;

    // --- include --------------------------------------------------------

    let st = Rc::clone(state);
    let serve_dir = cfg.serve_dir.clone();
    let cache = Arc::clone(&cfg.cache);
    globals.set(
        "include",
        lua.create_function(move |lua, rel: String| {
            if st.include_depth.get() >= MAX_INCLUDE_DEPTH {
                return Err(mlua::Error::runtime(format!(
                    "include: depth limit ({MAX_INCLUDE_DEPTH}) exceeded — recursive include?"
                )));
            }
            let joined = serve_dir.join(rel.trim_start_matches('/'));
            let abs = joined
                .canonicalize()
                .map_err(|_| mlua::Error::runtime(format!("include: file not found: {rel}")))?;
            if !abs.starts_with(&serve_dir) {
                return Err(mlua::Error::runtime(format!(
                    "include: path escapes the serve directory: {rel}"
                )));
            }
            if !abs
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("lhtml"))
            {
                return Err(mlua::Error::runtime(
                    "include: only .lhtml files can be included (use require() for .lua modules)",
                ));
            }

            let display_name = abs
                .strip_prefix(&serve_dir)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| rel.clone());
            let template = cache
                .load(&abs, &display_name)
                .map_err(|e| mlua::Error::runtime(format!("include: {e}")))?;

            let func = lua
                .load(template.lua_source.as_str())
                .set_name(template.chunk_name.clone())
                .into_function()?;

            st.include_depth.set(st.include_depth.get() + 1);
            let result = func.call::<()>(());
            st.include_depth.set(st.include_depth.get() - 1);
            result
        })?,
    )?;

    // --- request table --------------------------------------------------

    globals.set("request", build_request_table(lua, request)?)?;

    // --- response table -------------------------------------------------

    let response = lua.create_table()?;
    response.set("status", 200)?;
    response.set("headers", lua.create_table()?)?;

    let st = Rc::clone(state);
    response.set(
        "set_cookie",
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
            st.cookies.borrow_mut().push(cookie);
            Ok(())
        })?,
    )?;

    // send_file: serve a file from the data or serve directory instead of
    // the rendered output. The async side handles the actual delivery
    // (ranges, ETag, Content-Disposition).
    let st = Rc::clone(state);
    let data_dir = cfg.data_dir.clone();
    let serve_dir = cfg.serve_dir.clone();
    response.set(
        "send_file",
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
            if !abs.starts_with(&data_dir) {
                if abs
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("lua"))
                {
                    return Err(mlua::Error::runtime(format!(
                        "send_file: refusing to send Lua source from the serve directory: {path}"
                    )));
                }
                let hidden = abs
                    .strip_prefix(&serve_dir)
                    .map(|rel| {
                        rel.components().any(|c| match c {
                            std::path::Component::Normal(name) => {
                                name.to_string_lossy().starts_with('.')
                            }
                            _ => false,
                        })
                    })
                    .unwrap_or(true);
                if hidden {
                    return Err(mlua::Error::runtime(format!(
                        "send_file: refusing to send a hidden file from the serve directory: {path}"
                    )));
                }
            }
            let (download_name, content_type) = match &opts {
                Some(o) => (o.get("download_name")?, o.get("content_type")?),
                None => (None, None),
            };
            *st.send_file.borrow_mut() = Some(SendFileSpec {
                path: abs,
                download_name,
                content_type,
            });
            Ok(())
        })?,
    )?;

    globals.set("response", response)?;

    Ok(())
}

fn build_request_table(lua: &Lua, request: &RequestData) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("method", request.method.as_str())?;
    table.set("path", request.path.as_str())?;
    // nil rather than a placeholder string when there is no peer address, so
    // `or` in Lua picks up a fallback the way it does for every other field.
    table.set("remote_addr", request.remote_addr.as_deref())?;

    // Query parameters: `query` maps each name to its last value (PHP-style);
    // `query_all` maps each name to the array of all its values, in order.
    let query_pairs: Vec<(String, String)> = serde_urlencoded::from_str(&request.query_string)
        .unwrap_or_else(|e| {
            log::warn!("failed to parse query string: {e}");
            Vec::new()
        });
    let query = lua.create_table()?;
    let query_all = lua.create_table()?;
    for (name, value) in &query_pairs {
        query.set(name.as_str(), value.as_str())?;
        let values: Table = match query_all.get::<Option<Table>>(name.as_str())? {
            Some(t) => t,
            None => {
                let t = lua.create_table()?;
                query_all.set(name.as_str(), &t)?;
                t
            }
        };
        values.push(value.as_str())?;
    }
    table.set("query", query)?;
    table.set("query_all", query_all)?;

    // Headers: stored lowercased; a metatable makes lookups case-insensitive.
    let headers = lua.create_table()?;
    for (name, value) in &request.headers {
        headers.set(name.as_str(), value.as_str())?;
    }
    let headers_meta = lua.create_table()?;
    headers_meta.set(
        "__index",
        lua.create_function(|_, (t, key): (Table, String)| t.raw_get::<Value>(key.to_lowercase()))?,
    )?;
    headers.set_metatable(Some(headers_meta))?;
    table.set("headers", headers)?;

    let cookies = lua.create_table()?;
    for (name, value) in &request.cookies {
        cookies.set(name.as_str(), value.as_str())?;
    }
    table.set("cookies", cookies)?;

    // Body: always a table so `pairs(request.body)` is safe. `request.files`
    // is always an array (uploads land there for multipart requests).
    let files_table = lua.create_table()?;
    let content_type = request.content_type.as_deref().unwrap_or("");
    let body_table = match &request.body {
        RequestBody::Multipart { fields, files } => {
            for file in files {
                let f = lua.create_table()?;
                f.set("field", file.field.as_str())?;
                f.set("filename", file.filename.as_str())?;
                f.set("content_type", file.content_type.as_deref())?;
                f.set("size", file.size)?;
                f.set("path", file.path.display().to_string())?;
                files_table.push(f)?;
            }
            let t = lua.create_table()?;
            for (name, value) in fields {
                t.set(name.as_str(), value.as_str())?;
            }
            t
        }
        RequestBody::Raw(bytes) => {
            if !bytes.is_empty() {
                table.set("raw_body", lua.create_string(bytes)?)?;
            }
            if content_type.starts_with("application/x-www-form-urlencoded") {
                let pairs: Vec<(String, String)> = serde_urlencoded::from_bytes(bytes)
                    .unwrap_or_else(|e| {
                        log::warn!("failed to parse form body: {e}");
                        Vec::new()
                    });
                let t = lua.create_table()?;
                for (name, value) in &pairs {
                    t.set(name.as_str(), value.as_str())?;
                }
                t
            } else if content_type.starts_with("application/json")
                || content_type.ends_with("+json")
            {
                match serde_json::from_slice::<serde_json::Value>(bytes) {
                    Ok(json) => match lua.to_value(&json)? {
                        Value::Table(t) => t,
                        other => {
                            // A JSON scalar at the top level: expose it as body[1].
                            let t = lua.create_table()?;
                            t.push(other)?;
                            t
                        }
                    },
                    Err(e) => {
                        log::warn!("failed to parse JSON body: {e}");
                        lua.create_table()?
                    }
                }
            } else {
                lua.create_table()?
            }
        }
    };
    table.set("body", body_table)?;
    table.set("files", files_table)?;

    Ok(table)
}

pub fn html_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            c => out.push(c),
        }
    }
    out
}

/// Append a value to the output buffer: strings pass through as raw bytes,
/// everything else is stringified like `value_to_string`.
fn append_value(lua: &Lua, value: Value, out: &mut Vec<u8>) -> mlua::Result<()> {
    match value {
        Value::String(s) => out.extend_from_slice(&s.as_bytes()),
        other => out.extend_from_slice(value_to_string(lua, other)?.as_bytes()),
    }
    Ok(())
}

/// Stringify print/log arguments, joined with a single space.
pub(crate) fn params_to_string(lua: &Lua, params: MultiValue) -> mlua::Result<String> {
    let parts = params
        .into_iter()
        .map(|value| value_to_string(lua, value))
        .collect::<mlua::Result<Vec<String>>>()?;
    Ok(parts.join(" "))
}

fn value_to_string(lua: &Lua, value: Value) -> mlua::Result<String> {
    Ok(match value {
        Value::Nil => "nil".to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::Integer(n) => n.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.to_string_lossy(),
        Value::Function(_) => "<function>".to_string(),
        value => serde_json::to_string(&lua.from_value::<serde_json::Value>(value)?)
            .map_err(mlua::Error::external)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_html_entities() {
        assert_eq!(
            html_escape(r#"<b class="x">&'</b>"#),
            "&lt;b class=&quot;x&quot;&gt;&amp;&#x27;&lt;/b&gt;"
        );
    }
}
