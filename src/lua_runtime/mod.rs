pub mod utils;

use anyhow::Result;
use mlua::{Lua, MultiValue, Value};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use utils::params_to_string;

#[derive(Clone, Debug)]
pub struct LuaRequest {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub query: HashMap<String, String>,
    pub body: Option<HashMap<String, String>>,
}

#[derive(Clone, Debug)]
pub struct ManagedLuaInstance {
    pub lua: Lua,
    initial_global_keys: HashSet<String>,
}

impl ManagedLuaInstance {
    /// Create a new Lua instance with logging functions registered
    /// that map to Rust's log crate.
    /// These functions can be called from Lua as `log.info()`, `log.debug()`, etc.
    /// # Panics
    /// This function will never panic, but the logging functions within may panic
    /// when false parameters are passed.
    /// # Errors
    /// This function will return an error if there is an issue creating the Lua functions or table.
    pub fn new() -> Result<Self> {
        log::trace!("Creating new ManagedLuaInstance");
        let lua = Lua::new();
        let globals = lua.globals();

        // Register html_escape function
        let html_escape_function = lua.create_function(|_, input: String| {
            let escaped = input
                .chars()
                .map(|c| match c {
                    '&' => "&amp;".to_string(),
                    '<' => "&lt;".to_string(),
                    '>' => "&gt;".to_string(),
                    '"' => "&quot;".to_string(),
                    '\'' => "&#x27;".to_string(),
                    _ => c.to_string(),
                })
                .collect::<String>();
            log::trace!("Escaping HTML: {input} -> {escaped}");
            Ok(escaped)
        })?;
        globals.set("html_escape", html_escape_function)?;

        // Register print function for easier output
        // It appends to the _output_buffer global variable, which is then read by the template processor
        // TODO: Maybe we could instead create a instance state and use `create_function_mut``
        let print_function = lua.create_function(|lua, params: MultiValue| {
            let str_val = params_to_string(lua, params).unwrap_or_default();
            let globals = lua.globals();
            let mut buffer: String = globals.get("_output_buffer").unwrap_or_default();
            buffer.push_str(&str_val);
            globals.set("_output_buffer", buffer)?;
            Ok(())
        })?;
        globals.set("print", print_function)?;

        macro_rules! create_log_function {
            ($lua:expr, $level:ident) => {
                $lua.create_function(|lua, params: MultiValue| {
                    let message =
                        params_to_string(lua, params).map_err(mlua::Error::external)?;
                    log::$level!("{}", message);
                    Ok(())
                })
            };
        }

        let log_trace_function = create_log_function!(lua, trace)?;
        let log_debug_function = create_log_function!(lua, debug)?;
        let log_info_function = create_log_function!(lua, info)?;
        let log_warn_function = create_log_function!(lua, warn)?;
        let log_error_function = create_log_function!(lua, error)?;
        let log_newindex_metafunction = lua
            .load(
     r#"
            function(table, key, value)
                error("Attempt to modify read-only log table: " .. key)
            end
            "#,
            )
            .eval::<mlua::Function>()?;
        
        let log_table = lua.create_table()?;

        log_table.set("trace", log_trace_function)?;
        log_table.set("debug", log_debug_function)?;
        log_table.set("info", log_info_function)?;
        log_table.set("warn", log_warn_function)?;
        log_table.set("error", log_error_function)?;

        let log_metatable = lua.create_table()?;
        log_metatable.set("__newindex", log_newindex_metafunction)?;
        log_table.set_metatable(Some(log_metatable))?;

        // Make log available globally without requiring it
        globals.set("log", log_table.clone())?;
        // Also register as a module for require("log") syntax
        lua.register_module("log", log_table)?;

        // Capture the initial set of global keys for reset
        let mut initial_global_keys = HashSet::new();
        for pair in globals.pairs::<String, Value>() {
            let (key, _) = pair?;
            initial_global_keys.insert(key);
        }

        Ok(ManagedLuaInstance {
            lua,
            initial_global_keys,
        })
    }

    pub fn reset(&self) -> Result<()> {
        log::trace!("Resetting ManagedLuaInstance to initial state");
        let globals = self.lua.globals();

        // Collect keys to remove (can't modify while iterating)
        let mut keys_to_remove = Vec::new();
        for pair in globals.pairs::<String, Value>() {
            let (key, _) = pair?;
            if !self.initial_global_keys.contains(&key) {
                keys_to_remove.push(key);
            }
        }

        // Remove any globals that weren't in the initial set
        for key in keys_to_remove {
            globals.set(key, Value::Nil)?;
        }
        Ok(())
    }
}

// Logic to parse the tags and run Lua
// TODO: Split this up better
// TODO: We could use a scope to wrap the entire execution such that we can
//       local variables can be defined and used everywhere.
pub fn process_lhtml(instance: &ManagedLuaInstance, input: &str, request: &LuaRequest) -> Result<String> {
    // Set up request table
    let request_table = lua_request_to_table(&instance.lua, request)?;
    instance.lua.globals().set("request", request_table)?;

    // Set up response table for status and header control
    let response_table = instance.lua.create_table()?;
    response_table.set("status", 200)?;
    response_table.set("headers", instance.lua.create_table()?)?;
    instance.lua.globals().set("response", response_table)?;

    // (?s) enables "dotall" mode so . matches newlines, allowing multi-line Lua blocks
    let re = Regex::new(r"(?s)<\?lua(.*?)\?>")?;
    let mut result = String::new();
    let mut last_end = 0;

    log::trace!("Found {} lua blocks to execute", re.captures_iter(input).count());
    for cap in re.captures_iter(input) {
        //log::debug!("Processing lua block: {}", &cap[1]);
        if let Some(m) = cap.get(0) {
            let match_start = m.start();
            let match_end = m.end();
            result.push_str(&input[last_end..match_start]);
            last_end = match_end;
        }

        let lua_code = &cap[1];
        
        // Reset output buffer for this block
        instance.lua.globals().set("_output_buffer", String::new())?;

        let chunk = instance.lua.load(lua_code);

        match chunk.eval::<Option<String>>() {
            Ok(output_opt) => {
              // combine both the output buffer and return value
              let mut final_output = instance.lua.globals().get::<String>("_output_buffer")?;
              if let Some(output) = output_opt {
                  final_output.push_str(&output);
              }
              result.push_str(&final_output);
            },
            Err(e) => return Err(e.into()),
        }

    }
    result.push_str(&input[last_end..]);
    Ok(result)
}

fn lua_request_to_table(lua: &Lua, request: &LuaRequest) -> Result<mlua::Table> {
    let table = lua.create_table()?;
    
    table.set("method", request.method.as_str())?;
    table.set("path", request.path.as_str())?;
    
    // Set query table
    let query_table = lua.create_table()?;
    for (key, value) in &request.query {
        query_table.set(key.as_str(), value.as_str())?;
    }
    table.set("query", query_table)?;
    
    // Set headers table
    let headers_table = lua.create_table()?;
    for (key, value) in &request.headers {
        headers_table.set(key.as_str(), value.as_str())?;
    }
    table.set("headers", headers_table)?;

    // Set body table
    let body_table = lua.create_table()?;
    if let Some(body) = &request.body {
        for (key, value) in body {
            body_table.set(key.as_str(), value.as_str())?;
        }
    }
    table.set("body", body_table)?;
    
    Ok(table)
}