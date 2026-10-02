//! The Lua-facing API.
//!
//! `install_core` is the request-independent surface, shared by rendering and
//! background jobs; `install_request_api` adds output, `include`, `exit` and
//! the environment builder.
//!
//! Both run once per state, because states are pooled. `build_request_env` is
//! what runs per request: a *closed* environment holding copies of the library
//! and API tables, which a sandbox chaining to `_G` cannot substitute for --
//! see POOLING-PLAN.md. The copies outlive the request and are replaced by
//! `reset_request_env` only once one has been modified.

mod env;
mod modules;
mod output;
mod request;
mod response;

use std::cell::Cell;

use mlua::{Lua, LuaSerdeExt, MultiValue, Table, Value};

pub(crate) use env::{build_request_env, env_names, reset_request_env};
pub use output::html_escape;

use crate::runtime::{RenderConfig, RenderState};
#[cfg(feature = "builtin-sqlite")]
use crate::sqlite_api;
use crate::{crypto_api, fs_api, process_api};

thread_local! {
    /// Separate from `CURRENT` so the raw `_out` reaches the buffer in one load.
    static CURRENT_OUT: Cell<*mut Vec<u8>> = const { Cell::new(std::ptr::null_mut()) };
    /// The request rendering on this thread, or null outside a render.
    static CURRENT: Cell<*const RenderState> = const { Cell::new(std::ptr::null()) };
}

/// Points the thread-locals at `state` for as long as this lives; restored on
/// drop, including while unwinding. This is what lets the API functions be
/// built once per state instead of capturing the request.
pub(crate) struct RequestScope {
    out: *mut Vec<u8>,
    state: *const RenderState,
}

impl RequestScope {
    pub(crate) fn new(state: &RenderState) -> Self {
        RequestScope {
            out: CURRENT_OUT.replace(state.out.as_ptr()),
            state: CURRENT.replace(state),
        }
    }
}

impl Drop for RequestScope {
    fn drop(&mut self) {
        crate::plugins::end_request();
        let _ = CURRENT_OUT.try_with(|c| c.set(self.out));
        let _ = CURRENT.try_with(|c| c.set(self.state));
    }
}

/// Run `f` against the request being rendered on this thread. Null only on a
/// background thread's state, which has no request.
fn with_state<R>(f: impl FnOnce(&RenderState) -> mlua::Result<R>) -> mlua::Result<R> {
    let ptr = CURRENT.try_with(|c| c.get()).unwrap_or(std::ptr::null());
    if ptr.is_null() {
        return Err(mlua::Error::runtime(
            "this function is only available while a request is being rendered",
        ));
    }
    // SAFETY: every caller is a Lua callback, and Lua only runs inside the
    // span where `render` holds a `RequestScope`.
    f(unsafe { &*ptr })
}

/// Request-independent API, shared by page rendering and background jobs.
pub(crate) fn install_core(lua: &Lua, cfg: &RenderConfig) -> mlua::Result<()> {
    output::install_html_escape(lua)?;
    install_server(lua, cfg)?;
    install_log(lua)?;
    install_json(lua)?;
    #[cfg(feature = "builtin-sqlite")]
    sqlite_api::register(lua, &cfg.data_dir, cfg.sqlite_idle_connections)?;
    process_api::register(lua)?;
    fs_api::register(lua, &cfg.data_dir)?;
    crypto_api::register(lua)?;
    crate::threads::register(lua, cfg)?;
    crate::plugins::install(lua, cfg.plugins)?;
    modules::install_package(lua, cfg)
}

/// Installed once when a state is built. Nothing here may capture a request --
/// a pooled state serves many; see [`with_state`].
pub(crate) fn install_request_api(lua: &Lua, cfg: &RenderConfig) -> mlua::Result<()> {
    install_core(lua, cfg)?;
    output::install(lua)?;
    response::install_control_flow(lua)?;

    // What the environment builder closes over. Only the callbacks: it reads
    // the library and API tables from its own globals, which is one table
    // constructor instead of forty crossings on every state a non-pooled
    // server builds.
    let shared = lua.create_table()?;
    // Native modules cannot go through the chunk cache, so they fall back to
    // Lua's own require. Only with --c-module-dir, which also turns pooling off.
    if cfg.c_module_path.is_some() {
        shared.set("native_require", lua.globals().get::<Value>("require")?)?;
    }
    shared.set("search", modules::search(lua, cfg)?)?;
    shared.set("include", modules::include(lua, cfg)?)?;
    shared.set("set_cookie", response::set_cookie(lua)?)?;
    shared.set("send_file", response::send_file(lua, cfg)?)?;
    shared.set("flush", response::flush(lua)?)?;

    request::install(lua)?;
    env::install(lua, shared, cfg.plugins)
}

/// Writes into the page being rendered, for plugins' `host.write`.
pub(crate) fn emit_output(bytes: &[u8]) -> mlua::Result<()> {
    output::emit(bytes)
}

fn install_server(lua: &Lua, cfg: &RenderConfig) -> mlua::Result<()> {
    let server = lua.create_table()?;
    server.set("data_dir", cfg.data_dir.display().to_string())?;
    server.set("version", env!("CARGO_PKG_VERSION"))?;
    server.set("max_body_size", cfg.max_body_size)?;
    server.set("max_upload_size", cfg.max_upload_size)?;
    server.set("max_upload_files", cfg.max_upload_files)?;
    lua.globals().set("server", server)
}

fn install_log(lua: &Lua) -> mlua::Result<()> {
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

    lua.globals().set("log", log_table.clone())?;
    lua.register_module("log", log_table)
}

fn install_json(lua: &Lua) -> mlua::Result<()> {
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
    lua.globals().set("json", json_table.clone())?;
    lua.register_module("json", json_table)
}

thread_local! {
    /// Set when the running request created something with a finalizer.
    static FINALIZERS: Cell<bool> = const { Cell::new(false) };
}

thread_local! {
    /// Set when the running request called something that can wait for seconds.
    static BLOCKING: Cell<bool> = const { Cell::new(false) };
}

pub(crate) fn mark_blocking() {
    let _ = BLOCKING.try_with(|b| b.set(true));
}

/// Whether a blocking call was made since the last call, clearing the mark.
pub(crate) fn take_blocking() -> bool {
    BLOCKING.try_with(|b| b.replace(false)).unwrap_or(true)
}

pub(crate) fn mark_finalizers() {
    let _ = FINALIZERS.try_with(|f| f.set(true));
}

/// Whether a finalizer was created since the last call, clearing the mark.
pub(crate) fn take_finalizers() -> bool {
    FINALIZERS.try_with(|f| f.replace(false)).unwrap_or(true)
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
