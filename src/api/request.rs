//! The `request` table, built natively in one call.

use std::cell::Cell;
use std::os::raw::c_int;

use mlua::{Lua, LuaSerdeExt, Table, Value};

use crate::runtime::{RequestBody, RequestData};

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
pub(super) fn install(lua: &Lua) -> mlua::Result<()> {
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

pub(super) fn build(lua: &Lua, request: &RequestData) -> mlua::Result<Table> {
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
