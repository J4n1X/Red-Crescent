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

use std::cell::Cell;
use std::os::raw::c_int;
use std::path::Path;
use std::sync::Arc;

use std::time::Duration;

use mlua::{Lua, LuaSerdeExt, MultiValue, Table, Value};

use crate::fs_api;
use crate::process_api;
use crate::runtime::{
    ExitSignal, LuaCookie, RenderConfig, RenderState, RequestBody, RequestData, SendFileSpec,
};
use crate::template::TemplateCache;
use crate::{crypto_api, sqlite_api};

const MAX_INCLUDE_DEPTH: u32 = 16;

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

/// _out, using a raw C function rather than a Rust function improves callback
/// speed drastically.
/// # Safety
/// Registered only via `create_c_function`, so Lua guarantees a valid state and
/// at least one argument slot. It must never panic, because unwinding across
/// the C boundary is UB, so it does nothing that can: no borrow that could
/// conflict, no fallible conversion, and no Lua error raised. Appending can
/// abort on allocation failure, which is a clean abort rather than an unwind.
///
/// Aliasing: the pointer is only ever dereferenced from inside a Lua call, and
/// no safe path holds a `borrow_mut` of the buffer across anything that can
/// re-enter Lua, so the two never overlap.
unsafe extern "C-unwind" fn lua_out(state: *mut mlua::lua_State) -> c_int {
    let Ok(out) = CURRENT_OUT.try_with(|c| c.get()) else {
        return 0;
    };
    if out.is_null() {
        return 0;
    }
    let mut len: usize = 0;
    let ptr = unsafe { mlua::ffi::lua_tolstring(state, 1, &mut len) };
    if ptr.is_null() {
        return 0;
    }
    unsafe {
        let bytes = std::slice::from_raw_parts(ptr as *const u8, len);
        (*out).extend_from_slice(bytes);
    }
    0
}

/// Where the output functions other than `_out` write: the same buffer the raw
/// `_out` appends to, so ordering holds without coordination. Callers derive
/// their bytes first, so nothing here can re-enter Lua and alias the buffer.
fn emit(bytes: &[u8]) -> mlua::Result<()> {
    with_state(|st| {
        st.out.borrow_mut().extend_from_slice(bytes);
        Ok(())
    })
}

/// Escapes into the buffer rather than into a temporary, which is what makes
/// an interpolation allocation-free.
fn emit_escaped(bytes: &[u8]) -> mlua::Result<()> {
    with_state(|st| {
        escape_into(bytes, &mut st.out.borrow_mut());
        Ok(())
    })
}

/// Registry key for the Lua closure that renders what `lua_tolstring` cannot.
/// NUL-terminated, so the raw function hands it straight to `lua_getfield`.
const OUT_EXPR_SLOW: &str = "rc_out_expr_slow\0";

/// `<?lua= expr ?>`, raw for the same reason as [`lua_out`]: it runs once per
/// interpolation, and an mlua callback's dispatch costs more than the escaping.
///
/// # Safety
/// As `lua_out`, and it must not panic for the same reason. Strings, numbers,
/// nil and booleans are rendered here; anything else goes to [`out_expr_slow`],
/// the only step that can raise -- and a raise longjmps, so nothing with a
/// destructor is live across it.
unsafe extern "C-unwind" fn lua_out_expr(state: *mut mlua::lua_State) -> c_int {
    let Ok(out) = CURRENT_OUT.try_with(|c| c.get()) else {
        return 0;
    };
    if out.is_null() {
        return 0;
    }
    use std::fmt::Write as _;

    let top = unsafe { mlua::ffi::lua_gettop(state) };
    for index in 1..=top {
        // SAFETY (every arm): `out` addresses this request's buffer, which is a
        // separate allocation from anything Lua owns, so reading a Lua string
        // while appending to it cannot overlap.
        match unsafe { mlua::ffi::lua_type(state, index) } {
            mlua::ffi::LUA_TNIL => {}
            mlua::ffi::LUA_TBOOLEAN => {
                let word: &[u8] = if unsafe { mlua::ffi::lua_toboolean(state, index) } != 0 {
                    b"true"
                } else {
                    b"false"
                };
                unsafe { (*out).extend_from_slice(word) };
            }
            mlua::ffi::LUA_TSTRING => {
                let mut len: usize = 0;
                // Already a string, so this converts nothing and cannot raise.
                let ptr = unsafe { mlua::ffi::lua_tolstring(state, index, &mut len) };
                if !ptr.is_null() {
                    let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
                    escape_into(bytes, unsafe { &mut *out });
                }
            }
            // Formatted by Rust, not Lua: `tostring(5.0)` is "5.0" on 5.4 and
            // "5" on LuaJIT, and a template must not see which backend it runs
            // on. `lua_isinteger` classifies exactly as mlua's own conversion.
            mlua::ffi::LUA_TNUMBER => {
                let mut buf = StackBuf::new();
                let written = if unsafe { mlua::ffi::lua_isinteger(state, index) } != 0 {
                    write!(buf, "{}", unsafe { mlua::ffi::lua_tointeger(state, index) })
                } else {
                    write!(buf, "{}", unsafe { mlua::ffi::lua_tonumber(state, index) })
                };
                if written.is_ok() {
                    unsafe { (*out).extend_from_slice(buf.as_bytes()) };
                }
            }
            _ => unsafe { out_expr_slow(state, index) },
        }
    }
    0
}

/// A `write!` target that cannot allocate or panic, so formatting a number
/// leaves nothing with a destructor live where Lua might longjmp.
struct StackBuf {
    buf: [u8; 40],
    len: usize,
}

impl StackBuf {
    fn new() -> Self {
        StackBuf {
            buf: [0; 40],
            len: 0,
        }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

impl std::fmt::Write for StackBuf {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let end = self.len.checked_add(s.len()).ok_or(std::fmt::Error)?;
        let slot = self.buf.get_mut(self.len..end).ok_or(std::fmt::Error)?;
        slot.copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

/// Hands one value to the Lua closure under [`OUT_EXPR_SLOW`]. Skipped if that
/// is missing, which means a state whose request API was never installed.
unsafe fn out_expr_slow(state: *mut mlua::lua_State, index: c_int) {
    unsafe {
        if mlua::ffi::lua_checkstack(state, 2) == 0 {
            return;
        }
        mlua::ffi::lua_getfield(
            state,
            mlua::ffi::LUA_REGISTRYINDEX,
            OUT_EXPR_SLOW.as_ptr() as *const std::os::raw::c_char,
        );
        if mlua::ffi::lua_type(state, -1) != mlua::ffi::LUA_TFUNCTION {
            mlua::ffi::lua_pop(state, 1);
            return;
        }
        mlua::ffi::lua_pushvalue(state, index);
        mlua::ffi::lua_call(state, 1, 0);
    }
}

/// A Lua-side buffer was tried on JIT backends and removed: it wins on a page
/// of literal HTML, but every `<?lua= ?>` then costs a Rust callback *plus* a
/// call back into Lua to reach the buffer. On a 1000-row template that measured
/// 1,442 rps against 2,820 for writing straight through.
fn install_out(lua: &Lua) -> mlua::Result<()> {
    // SAFETY: see `lua_out`. Its buffer comes from the thread-local that
    // `RequestScope` in `render` keeps pointed at this request's state.
    let sink = unsafe { lua.create_c_function(lua_out)? };
    lua.globals().set("_out", sink)
}

/// Request-independent API, shared by page rendering and background jobs.
pub(crate) fn install_core(lua: &Lua, cfg: &RenderConfig) -> mlua::Result<()> {
    let globals = lua.globals();

    globals.set(
        "html_escape",
        lua.create_function(|lua, input: mlua::String| {
            if first_escapable(&input.as_bytes()).is_none() {
                return Ok(input);
            }
            lua.create_string(escape_bytes(&input.as_bytes()))
        })?,
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
            mark_blocking();
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
/// dots becoming separators. Canonicalized and checked against the serve
/// directory, so a crafted name cannot reach outside it.
fn find_module(cache: &TemplateCache, serve_dir: &Path, name: &str) -> mlua::Result<Found> {
    let rel = name.replace('.', std::path::MAIN_SEPARATOR_STR);
    let mut tried = String::new();

    for candidate in [format!("{rel}.lua"), format!("{rel}/init.lua")] {
        let joined = serve_dir.join(&candidate);
        let resolved = joined
            .canonicalize()
            .ok()
            .filter(|abs| abs.starts_with(serve_dir) && abs.is_file());
        let Some(abs) = resolved else {
            tried.push_str(&format!("\n\tno file '{}'", joined.display()));
            continue;
        };

        let module = cache
            .load_module(&abs, &candidate)
            .map_err(|e| mlua::Error::runtime(format!("require '{name}': {e}")))?;
        return Ok(Found::Chunk(module, abs));
    }

    Ok(Found::Missing(tried))
}

/// Registry keys for the environment builder and its reset, out of reach of
/// template code.
const ENV_BUILDER_KEY: &str = "rc_env_builder";
const ENV_RESET_KEY: &str = "rc_env_reset";

/// Library tables each request gets a private copy of. Copying is what makes a
/// reused state safe: a sandbox chaining to _G would let one request's
/// `string.upper = f` reach the next.
const ENV_TABLES: &[&str] = &[
    "string",
    "table",
    "math",
    "os",
    "io",
    "coroutine",
    "utf8",
    "log",
    "json",
    "sqlite",
    "crypto",
    "fs",
    "process",
    "thread",
    "server",
];

/// Shared by reference, since a Lua function value is immutable. Absent on
/// purpose: getmetatable (getmetatable("").__index is the real string table,
/// which no copy can hide), rawset (walks past the log table's read-only
/// guard), dofile/loadfile (chunks bound to the real globals), and
/// require/load, replaced in the builder.
const ENV_FUNCS: &[&str] = &[
    "assert",
    "collectgarbage",
    "error",
    "ipairs",
    "next",
    "pairs",
    "pcall",
    "rawequal",
    "rawget",
    "rawlen",
    "select",
    "setmetatable",
    "tonumber",
    "tostring",
    "type",
    "unpack",
    "xpcall",
    "html_escape",
    "print",
    "exit",
    "redirect",
    "_out",
    "_out_expr",
    "_out_raw",
];

/// Lua 5.2+ takes the environment as `load`'s fourth argument.
#[cfg(not(feature = "luajit"))]
const LOAD_SHIM: &str = r##"
    env.load = function(chunk, chunkname, mode, ...)
        if select("#", ...) > 0 then return rawload(chunk, chunkname, mode, ...) end
        return rawload(chunk, chunkname, mode, env)
    end
"##;

/// 5.1 has no environment parameter, so the chunk is rebound after loading.
#[cfg(feature = "luajit")]
const LOAD_SHIM: &str = r#"
    local setfenv = setfenv
    local function bind(f, err)
        if f == nil then return f, err end
        return setfenv(f, env)
    end
    env.load = function(chunk, chunkname, mode) return bind(rawload(chunk, chunkname, mode)) end
    env.loadstring = env.load
"#;

/// Builds one request's environment. Compiled once per state; it returns the
/// per-request builder and the reset the pool runs between requests.
///
/// The copies live as long as the state and are replaced only once a request
/// has changed one, so a clean request allocates none of them. The
/// environment itself is fresh each time, from constructors sized in one go.
const ENV_BUILDER: &str = r#"
local shared = ...

local search, include = shared.search, shared.include
local set_cookie, send_file = shared.set_cookie, shared.send_file
local native_require = shared.native_require
local mark, unmodified = shared.mark_finalizers, shared.unmodified

local TABLES = { --[[TABLES]] }
local FUNCS = { --[[FUNCS]] }

local next, select, error, tostring, rawload, type = next, select, error, tostring, load, type
local rawget, setmetatable = rawget, setmetatable
local G, VERSION = _G, _VERSION
local pkg_path, pkg_cpath = package.path, package.cpath
local pkg_config, loadlib = package.config, package.loadlib

-- A finalizer left for the incremental collector would run inside a later
-- request, so anything that can create one marks this request for a full
-- collection before the state is handed on.
local function marked(f) return function(...) mark() return f(...) end end

local funcs = {}
for i = 1, #FUNCS do funcs[i] = G[FUNCS[i]] end
for i = 1, #FUNCS do
    if FUNCS[i] == "setmetatable" then
        funcs[i] = function(t, mt)
            if type(mt) == "table" and rawget(mt, "__gc") ~= nil then mark() end
            return setmetatable(t, mt)
        end
    end
end

-- Resolved once per state. A name absent on this backend (utf8 on LuaJIT)
-- stays a hole and drops out of the constructors below.
local sources, ismodule = {}, {}
for i = 1, #TABLES do
    local name = TABLES[i]
    local source = G[name]
    if type(source) == "table" then
        if name == "io" then
            local wrapped = {}
            for k, v in next, source do wrapped[k] = v end
            for _, k in next, { "open", "lines", "popen", "tmpfile", "input", "output" } do
                if type(source[k]) == "function" then wrapped[k] = marked(source[k]) end
            end
            source = wrapped
        end
        sources[i] = source
        -- Only what stock require already resolves stays require-able.
        ismodule[i] = package.loaded[name] ~= nil
    end
end

local copies, modules, sizes = {}, {}, {}
local function copy_of(i)
    local copy, n = {}, 0
    for k, v in next, sources[i] do
        copy[k] = v
        n = n + 1
    end
    copies[i], sizes[i] = copy, n
    if ismodule[i] then modules[i] = copy end
end
for i = 1, #TABLES do
    if sources[i] ~= nil then copy_of(i) end
end

local function reset()
    for i = 1, #TABLES do
        local copy = copies[i]
        if copy ~= nil and not unmodified(copy, sources[i], sizes[i]) then copy_of(i) end
    end
end

local function build(request)
    local env
    local preload, loading = {}, {}
    local loaded = { --[[LOADED]] _G = nil }
    env = {
        --[[ENV]]
        _G = nil,
        _VERSION = VERSION,
        request = request,
        -- `loaded` is this request's, so module state dies with it. `searchers`
        -- is absent rather than copied: require below does not consult it.
        package = {
            path = pkg_path,
            cpath = pkg_cpath,
            config = pkg_config,
            loaded = loaded,
            preload = preload,
            loadlib = loadlib,
        },
        require = nil,
        include = nil,
        load = nil,
        loadstring = nil,
        response = {
            status = 200,
            headers = {},
            set_cookie = set_cookie,
            send_file = send_file,
        },
    }
    env._G = env
    loaded._G = env

    --[[LOAD_SHIM]]

    local function record(name, result)
        if result == nil then result = true end
        loaded[name], loading[name] = result, nil
        return result
    end

    env.require = function(name)
        local module = loaded[name]
        if module ~= nil then return module end
        if loading[name] then
            error("loop or previous error loading module '" .. tostring(name) .. "'", 2)
        end
        loading[name] = true
        local loader = preload[name]
        if loader ~= nil then return record(name, loader(name, ":preload:")) end
        local path
        loader, path = search(name, env)
        if loader ~= nil then return record(name, loader(name, path)) end
        loading[name] = nil
        -- Native modules only, and only with --c-module-dir, which pools nothing.
        if native_require ~= nil then return native_require(name) end
        error("module '" .. tostring(name) .. "' not found:" .. tostring(path), 2)
    end

    env.include = function(rel) return include(rel, env) end

    return env
end

return build, reset
"#;

/// The builder source with its name lists and constructor fields filled in.
fn env_builder_source() -> String {
    let quoted = |names: &[&str]| {
        names
            .iter()
            .map(|n| format!("{n:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let fields = |names: &[&str], array: &str| {
        names
            .iter()
            .enumerate()
            .map(|(i, n)| format!("{n} = {array}[{}], ", i + 1))
            .collect::<String>()
    };
    let env = fields(ENV_TABLES, "copies") + &fields(ENV_FUNCS, "funcs");
    ENV_BUILDER
        .replace("--[[TABLES]]", &quoted(ENV_TABLES))
        .replace("--[[FUNCS]]", &quoted(ENV_FUNCS))
        .replace("--[[LOADED]]", &fields(ENV_TABLES, "modules"))
        .replace("--[[ENV]]", &env)
        .replace("--[[LOAD_SHIM]]", LOAD_SHIM)
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

/// # Safety
/// Touches only a const-init thread-local; cannot panic or raise.
/// `unmodified(copy, source, size)`: whether `copy` still holds exactly the
/// `size` entries of `source` and has no metatable. Raw reads throughout, so
/// nothing a template planted can run here.
///
/// # Safety
/// Called only by the environment reset, with two tables and an integer. It
/// allocates nothing and cannot raise: the traversal never writes to `copy`,
/// and the stack stays within the LUA_MINSTACK slots a C function is given.
unsafe extern "C-unwind" fn lua_unmodified(state: *mut mlua::lua_State) -> c_int {
    use mlua::ffi;
    unsafe {
        let mut same = ffi::lua_getmetatable(state, 1) == 0;
        if !same {
            ffi::lua_settop(state, 3);
        }
        let size = ffi::lua_tointeger(state, 3);
        let mut n: ffi::lua_Integer = 0;
        ffi::lua_pushnil(state);
        while same && ffi::lua_next(state, 1) != 0 {
            ffi::lua_pushvalue(state, -2);
            ffi::lua_rawget(state, 2);
            same = ffi::lua_rawequal(state, -1, -2) != 0;
            ffi::lua_settop(state, -3);
            n += 1;
        }
        ffi::lua_settop(state, 3);
        ffi::lua_pushboolean(state, (same && n == size) as c_int);
    }
    1
}

unsafe extern "C-unwind" fn lua_mark_finalizers(_state: *mut mlua::lua_State) -> c_int {
    mark_finalizers();
    0
}

/// Installed once when a state is built. Nothing here may capture a request --
/// a pooled state serves many; see [`with_state`].
pub(crate) fn install_request_api(lua: &Lua, cfg: &RenderConfig) -> mlua::Result<()> {
    install_core(lua, cfg)?;

    let globals = lua.globals();

    // --- output ---------------------------------------------------------

    install_out(lua)?;

    // <?lua= expr ?>. Escapes by default; nil prints nothing. The slow path is
    // registered first, because the raw function looks it up by registry key.
    lua.set_named_registry_value(
        &OUT_EXPR_SLOW[..OUT_EXPR_SLOW.len() - 1],
        lua.create_function(|lua, value: Value| {
            emit_escaped(value_to_string(lua, value)?.as_bytes())
        })?,
    )?;
    // SAFETY: see `lua_out_expr`. Same buffer and same thread-local as `_out`.
    globals.set("_out_expr", unsafe { lua.create_c_function(lua_out_expr)? })?;

    // <?lua== expr ?>: unescaped, for templates deliberately emitting markup.
    globals.set(
        "_out_raw",
        lua.create_function(|lua, values: MultiValue| {
            for value in values {
                match value {
                    Value::Nil => {}
                    Value::String(s) => emit(&s.as_bytes())?,
                    other => emit(value_to_string(lua, other)?.as_bytes())?,
                }
            }
            Ok(())
        })?,
    )?;

    // Joins arguments with a space. Unescaped: it is how a template emits
    // markup it assembled itself.
    globals.set(
        "print",
        lua.create_function(|lua, params: MultiValue| {
            for (i, value) in params.into_iter().enumerate() {
                if i > 0 {
                    emit(b" ")?;
                }
                match value {
                    Value::String(s) => emit(&s.as_bytes())?,
                    other => emit(value_to_string(lua, other)?.as_bytes())?,
                }
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

    // --- the values the environment builder closes over ------------------
    //
    // Only the callbacks: the builder reads the library and API tables from
    // its own globals, which is one table constructor instead of forty
    // crossings on every state a non-pooled server builds.

    let shared = lua.create_table()?;

    // Native modules cannot go through the chunk cache, so they fall back to
    // Lua's own require. Only with --c-module-dir, which also turns pooling off.
    if cfg.c_module_path.is_some() {
        shared.set("native_require", globals.get::<Value>("require")?)?;
    }

    // --- require ---------------------------------------------------------

    let serve_dir = cfg.serve_dir.clone();
    let cache = Arc::clone(&cfg.cache);
    shared.set(
        "search",
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
        })?,
    )?;

    // --- include --------------------------------------------------------

    let serve_dir = cfg.serve_dir.clone();
    let cache = Arc::clone(&cfg.cache);
    shared.set(
        "include",
        lua.create_function(move |lua, (rel, env): (String, Table)| {
            with_state(|st| {
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

                let func = cache.chunk_for(lua, &template, &env)?;

                st.include_depth.set(st.include_depth.get() + 1);
                let result = func.call::<()>(());
                st.include_depth.set(st.include_depth.get() - 1);
                result
            })
        })?,
    )?;

    // --- response helpers ------------------------------------------------

    shared.set(
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
            with_state(|st| {
                st.cookies.borrow_mut().push(cookie);
                Ok(())
            })
        })?,
    )?;

    // send_file: serve a file from the data or serve directory instead of
    // the rendered output. The async side handles the actual delivery
    // (ranges, ETag, Content-Disposition).
    let data_dir = cfg.data_dir.clone();
    let serve_dir = cfg.serve_dir.clone();
    shared.set(
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
            with_state(|st| {
                *st.send_file.borrow_mut() = Some(SendFileSpec {
                    path: abs,
                    download_name,
                    content_type,
                });
                Ok(())
            })
        })?,
    )?;

    // SAFETY: see `lua_unmodified`; only the builder holds it.
    shared.set("unmodified", unsafe {
        lua.create_c_function(lua_unmodified)?
    })?;
    // SAFETY: see `lua_mark_finalizers`.
    shared.set("mark_finalizers", unsafe {
        lua.create_c_function(lua_mark_finalizers)?
    })?;

    install_request_table(lua)?;

    let (builder, reset): (mlua::Function, mlua::Function) =
        env_builder_factory(lua)?.call(shared)?;
    lua.set_named_registry_value(ENV_BUILDER_KEY, builder)?;
    lua.set_named_registry_value(ENV_RESET_KEY, reset)?;

    Ok(())
}

/// The chunk that builds the builder, from bytecode after the first state
/// compiles it. Without this a non-pooled server reparses it every request,
/// which measured 1.7x on `hello`.
fn env_builder_factory(lua: &Lua) -> mlua::Result<mlua::Function> {
    static DUMPED: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    if let Some(code) = DUMPED.get() {
        return lua.load(code.as_slice()).into_function();
    }
    let function = lua
        .load(env_builder_source())
        .set_name("=rc:env")
        .into_function()?;
    let _ = DUMPED.set(function.dump(false));
    Ok(function)
}

/// This request's closed environment.
///
/// One deliberate difference from stock Lua: `string.upper = f` changes
/// `string.upper(...)` but not `("x"):upper()`, because the string metatable
/// still points at the untouched original.
pub(crate) fn build_request_env(lua: &Lua, request: &RequestData) -> mlua::Result<Table> {
    let builder: mlua::Function = lua.named_registry_value(ENV_BUILDER_KEY)?;
    builder.call(build_request_table(lua, request)?)
}

/// Readies a pooled state's copies for the next request, replacing any this
/// one modified.
pub(crate) fn reset_request_env(lua: &Lua) -> mlua::Result<()> {
    let reset: mlua::Function = lua.named_registry_value(ENV_RESET_KEY)?;
    reset.call(())
}

/// Whether the calling hook should abort this request. The watchdog owns the
/// clock, so this is one atomic load -- no `Instant::now()` on the hot path.
/// It stays true once set, so a script-level pcall can delay the abort by at
/// most one hook interval rather than swallowing it.
pub(crate) fn should_abort() -> bool {
    crate::watchdog::should_abort()
}

thread_local! {
    static CAPTURED_STATE: Cell<usize> = const { Cell::new(0) };
}

/// Hands the running `lua_State` out to Rust. Calling a raw C function is the
/// only place mlua surfaces the pointer.
unsafe extern "C-unwind" fn capture_state(state: *mut mlua::lua_State) -> c_int {
    let _ = CAPTURED_STATE.try_with(|c| c.set(state as usize));
    0
}

/// Address of `lua`'s state, which the watchdog needs to interrupt a JIT
/// backend. An address rather than a pointer so it can cross threads.
pub(crate) fn state_address(lua: &Lua) -> mlua::Result<usize> {
    // SAFETY: `capture_state` only stores its argument; it cannot panic and
    // raises nothing.
    let capture = unsafe { lua.create_c_function(capture_state)? };
    capture.call::<()>(())?;
    Ok(CAPTURED_STATE.try_with(|c| c.get()).unwrap_or(0))
}

/// Armed by the watchdog's signal handler; no hook is installed otherwise.
/// Consults the slot before raising, so an arming that raced a finished
/// request is a no-op rather than a killed innocent.
///
/// # Safety
/// A `lua_Hook`, so it must not panic and must leave no Rust value with a
/// `Drop` impl live across the raise: `lua_error` longjmps.
pub(crate) unsafe extern "C-unwind" fn timeout_hook(
    state: *mut mlua::lua_State,
    _ar: *mut mlua::ffi::lua_Debug,
) {
    unsafe { mlua::ffi::lua_sethook(state, None, 0, 0) };
    if !should_abort() {
        return;
    }
    unsafe {
        mlua::ffi::lua_pushliteral(state, c"execution time limit exceeded");
        mlua::ffi::lua_error(state);
    }
}

/// Registry keys for the native request-table builder and the metatable that
/// makes `request.headers` case-insensitive, both made once per state.
const REQUEST_TABLE_KEY: &str = "rc_request_table";
const HEADERS_META: &std::ffi::CStr = c"rc_headers_meta";

/// What `lua_request_table` reads: borrowed, so nothing it touches has a
/// destructor for a longjmp to skip.
struct RequestFields<'a> {
    request: &'a RequestData,
    query: &'a [(String, String)],
}

thread_local! {
    static PENDING_REQUEST: Cell<*const ()> = const { Cell::new(std::ptr::null()) };
}

unsafe fn push_str(state: *mut mlua::lua_State, s: &str) {
    unsafe { mlua::ffi::lua_pushlstring(state, s.as_ptr().cast(), s.len()) };
}

/// Pairs into a fresh table, last value winning.
unsafe fn push_pairs(state: *mut mlua::lua_State, pairs: &[(String, String)]) {
    use mlua::ffi;
    unsafe {
        ffi::lua_createtable(state, 0, pairs.len() as c_int);
        for (name, value) in pairs {
            push_str(state, name);
            push_str(state, value);
            ffi::lua_rawset(state, -3);
        }
    }
}

/// Builds `request` in one call; through mlua every field costs a protected
/// call of its own. The body is left empty for `build_request_table` to fill.
///
/// # Safety
/// Called only from `build_request_table`, which points `PENDING_REQUEST` at
/// fields outliving the call. A memory error longjmps out of here, so this
/// frame holds only borrows, and stays within LUA_MINSTACK slots.
unsafe extern "C-unwind" fn lua_request_table(state: *mut mlua::lua_State) -> c_int {
    use mlua::ffi;
    let ptr = PENDING_REQUEST
        .try_with(|p| p.get())
        .unwrap_or(std::ptr::null());
    if ptr.is_null() {
        return 0;
    }
    let fields = unsafe { &*(ptr as *const RequestFields) };
    let request = fields.request;
    unsafe {
        ffi::lua_createtable(state, 0, 10);
        push_str(state, &request.method);
        ffi::lua_setfield(state, -2, c"method".as_ptr());
        push_str(state, &request.path);
        ffi::lua_setfield(state, -2, c"path".as_ptr());
        // nil rather than a placeholder string when there is no peer address,
        // so `or` in Lua picks up a fallback as for every other field.
        if let Some(addr) = &request.remote_addr {
            push_str(state, addr);
            ffi::lua_setfield(state, -2, c"remote_addr".as_ptr());
        }

        // `query` maps each name to its last value (PHP-style); `query_all`
        // maps each name to the array of all its values, in order.
        push_pairs(state, fields.query);
        ffi::lua_createtable(state, 0, 0);
        for (name, value) in fields.query {
            push_str(state, name);
            if ffi::lua_rawget(state, -2) == ffi::LUA_TNIL {
                ffi::lua_pop(state, 1);
                ffi::lua_createtable(state, 1, 0);
                push_str(state, name);
                ffi::lua_pushvalue(state, -2);
                ffi::lua_rawset(state, -4);
            }
            push_str(state, value);
            let n = ffi::lua_rawlen(state, -2);
            ffi::lua_rawseti(state, -2, n as ffi::lua_Integer + 1);
            ffi::lua_pop(state, 1);
        }
        ffi::lua_setfield(state, -3, c"query_all".as_ptr());
        ffi::lua_setfield(state, -2, c"query".as_ptr());

        // Stored lowercased; the metatable makes lookups case-insensitive.
        push_pairs(state, &request.headers);
        ffi::lua_getfield(state, ffi::LUA_REGISTRYINDEX, HEADERS_META.as_ptr());
        ffi::lua_setmetatable(state, -2);
        ffi::lua_setfield(state, -2, c"headers".as_ptr());

        push_pairs(state, &request.cookies);
        ffi::lua_setfield(state, -2, c"cookies".as_ptr());

        ffi::lua_createtable(state, 0, 0);
        ffi::lua_setfield(state, -2, c"body".as_ptr());
        ffi::lua_createtable(state, 0, 0);
        ffi::lua_setfield(state, -2, c"files".as_ptr());
    }
    1
}

/// Installs what `build_request_table` needs on a state.
fn install_request_table(lua: &Lua) -> mlua::Result<()> {
    let headers_meta = lua.create_table()?;
    headers_meta.set(
        "__index",
        lua.create_function(|_, (t, key): (Table, String)| t.raw_get::<Value>(key.to_lowercase()))?,
    )?;
    let key = HEADERS_META.to_str().expect("registry key is ASCII");
    lua.set_named_registry_value(key, headers_meta)?;
    // SAFETY: see `lua_request_table`.
    let build = unsafe { lua.create_c_function(lua_request_table)? };
    lua.set_named_registry_value(REQUEST_TABLE_KEY, build)
}

fn build_request_table(lua: &Lua, request: &RequestData) -> mlua::Result<Table> {
    let query_pairs: Vec<(String, String)> = serde_urlencoded::from_str(&request.query_string)
        .unwrap_or_else(|e| {
            log::warn!("failed to parse query string: {e}");
            Vec::new()
        });
    let fields = RequestFields {
        request,
        query: &query_pairs,
    };
    let build: mlua::Function = lua.named_registry_value(REQUEST_TABLE_KEY)?;
    PENDING_REQUEST.with(|p| p.set(&fields as *const RequestFields as *const ()));
    let table = build.call::<Table>(());
    PENDING_REQUEST.with(|p| p.set(std::ptr::null()));
    let table = table?;

    let content_type = request.content_type.as_deref().unwrap_or("");
    let is_json = content_type.starts_with("application/json") || content_type.ends_with("+json");
    if matches!(&request.body, RequestBody::Raw(bytes) if bytes.is_empty()) && !is_json {
        return Ok(table);
    }

    // Body: always a table so `pairs(request.body)` is safe. `request.files`
    // is always an array (uploads land there for multipart requests).
    let files_table = lua.create_table()?;
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
            } else if is_json {
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

/// Position of the first character needing an entity. Kept free of side effects
/// so it stays a pure scan the optimiser can vectorise -- the overwhelmingly
/// common answer is `None`, and that case must cost no more than the copy.
#[inline]
fn first_escapable(bytes: &[u8]) -> Option<usize> {
    bytes
        .iter()
        .position(|b| matches!(b, b'&' | b'<' | b'>' | b'"' | b'\''))
}

/// Appends the escaped form of `bytes`: one scan and one copy when there is
/// nothing to escape, otherwise run by run rather than byte by byte.
fn escape_into(bytes: &[u8], out: &mut Vec<u8>) {
    let Some(first) = first_escapable(bytes) else {
        out.extend_from_slice(bytes);
        return;
    };
    out.reserve(bytes.len() + 16);
    // From 0, not `first`: the run copied at the first replacement is the
    // unescaped prefix leading up to it.
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate().skip(first) {
        let replacement: &[u8] = match b {
            b'&' => b"&amp;",
            b'<' => b"&lt;",
            b'>' => b"&gt;",
            b'"' => b"&quot;",
            b'\'' => b"&#x27;",
            _ => continue,
        };
        out.extend_from_slice(&bytes[start..i]);
        out.extend_from_slice(replacement);
        start = i + 1;
    }
    out.extend_from_slice(&bytes[start..]);
}

fn escape_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    escape_into(bytes, &mut out);
    out
}

pub fn html_escape(input: &str) -> String {
    if first_escapable(input.as_bytes()).is_none() {
        return input.to_string();
    }
    // escape_bytes only ever replaces ASCII with ASCII, so UTF-8 is preserved.
    String::from_utf8(escape_bytes(input.as_bytes())).expect("escaping preserves UTF-8")
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

    /// `escape_into` copies the runs between replacements, so where the first
    /// one falls decides which runs exist at all.
    #[test]
    fn escapes_every_run_layout() {
        for (input, want) in [
            ("", ""),
            ("plain text", "plain text"),
            ("<lead", "&lt;lead"),
            ("trail>", "trail&gt;"),
            ("mid&dle", "mid&amp;dle"),
            ("a<<b", "a&lt;&lt;b"),
            ("<>&\"'", "&lt;&gt;&amp;&quot;&#x27;"),
            ("pre<mid>post", "pre&lt;mid&gt;post"),
        ] {
            assert_eq!(html_escape(input), want, "input {input:?}");
        }
    }

    #[test]
    fn escaping_preserves_non_ascii() {
        assert_eq!(html_escape("héllo <wörld>"), "héllo &lt;wörld&gt;");
    }
}
