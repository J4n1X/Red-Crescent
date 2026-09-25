//! Each request's closed environment: copies of the library and API tables,
//! built by `env_builder.lua` and reset between requests on a pooled state.

use std::os::raw::c_int;

use mlua::{Lua, Table};

use super::mark_finalizers;
use crate::runtime::RequestData;

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
const ENV_BUILDER: &str = include_str!("env_builder.lua");

/// Every name a request environment defines from the globals, for plugins
/// to be checked against.
pub(crate) fn env_names() -> impl Iterator<Item = &'static str> {
    ENV_TABLES.iter().chain(ENV_FUNCS).copied()
}

/// The builder source with its name lists and constructor fields filled in.
/// Plugin tables are copied per request like the built-in ones.
fn env_builder_source(plugins: &[&str]) -> String {
    let tables: Vec<&str> = ENV_TABLES
        .iter()
        .copied()
        .chain(plugins.iter().copied())
        .collect();
    let tables = tables.as_slice();
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
    let env = fields(tables, "copies") + &fields(ENV_FUNCS, "funcs");
    ENV_BUILDER
        .replace("--[[TABLES]]", &quoted(tables))
        .replace("--[[FUNCS]]", &quoted(ENV_FUNCS))
        .replace("--[[LOADED]]", &fields(tables, "modules"))
        .replace("--[[ENV]]", &env)
        .replace("--[[LOAD_SHIM]]", LOAD_SHIM)
}

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

/// # Safety
/// Touches only a const-init thread-local; cannot panic or raise.
unsafe extern "C-unwind" fn lua_mark_finalizers(_state: *mut mlua::lua_State) -> c_int {
    mark_finalizers();
    0
}

/// Builds the builder from `shared`, the callbacks it closes over, and stores
/// it with its reset in the registry.
pub(super) fn install(
    lua: &Lua,
    shared: Table,
    plugins: &crate::plugins::Plugins,
) -> mlua::Result<()> {
    // SAFETY: see `lua_unmodified`; only the builder holds it.
    shared.set("unmodified", unsafe {
        lua.create_c_function(lua_unmodified)?
    })?;
    // SAFETY: see `lua_mark_finalizers`.
    shared.set("mark_finalizers", unsafe {
        lua.create_c_function(lua_mark_finalizers)?
    })?;

    let (builder, reset): (mlua::Function, mlua::Function) =
        env_builder_factory(lua, plugins)?.call(shared)?;
    lua.set_named_registry_value(ENV_BUILDER_KEY, builder)?;
    lua.set_named_registry_value(ENV_RESET_KEY, reset)
}

/// The chunk that builds the builder, from bytecode after the first state
/// compiles it. Without this a non-pooled server reparses it every request,
/// which measured 1.7x on `hello`. Keyed by the plugin names, which the
/// source bakes in; a server has one set, tests have several.
fn env_builder_factory(
    lua: &Lua,
    plugins: &crate::plugins::Plugins,
) -> mlua::Result<mlua::Function> {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    type ByPlugins = HashMap<Vec<String>, Arc<[u8]>>;
    static DUMPED: Mutex<Option<ByPlugins>> = Mutex::new(None);

    let key: Vec<String> = plugins.names().map(str::to_string).collect();
    let cached = DUMPED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .get(&key)
        .cloned();
    if let Some(code) = cached {
        return lua.load(&*code).into_function();
    }
    let names: Vec<&str> = key.iter().map(String::as_str).collect();
    let function = lua
        .load(env_builder_source(&names))
        .set_name("=rc:env")
        .into_function()?;
    DUMPED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(key, function.dump(false).into());
    Ok(function)
}

/// This request's closed environment.
///
/// One deliberate difference from stock Lua: `string.upper = f` changes
/// `string.upper(...)` but not `("x"):upper()`, because the string metatable
/// still points at the untouched original.
pub(crate) fn build_request_env(lua: &Lua, request: &RequestData) -> mlua::Result<Table> {
    let builder: mlua::Function = lua.named_registry_value(ENV_BUILDER_KEY)?;
    builder.call(super::request::build(lua, request)?)
}

/// Readies a pooled state's copies for the next request, replacing any this
/// one modified.
pub(crate) fn reset_request_env(lua: &Lua) -> mlua::Result<()> {
    let reset: mlua::Function = lua.named_registry_value(ENV_RESET_KEY)?;
    reset.call(())
}
