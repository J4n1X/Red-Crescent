use anyhow::Result;
use mlua::{Lua, LuaSerdeExt, MultiValue, Value};

pub fn params_to_string(lua: &Lua, params: MultiValue) -> Result<String> {
    let stringified_params = params
        .into_iter()
        .map(|value| value_to_string(lua, value))
        .collect::<Result<Vec<String>>>()?
        .join(" ");

    Ok(stringified_params)
}

fn value_to_string(lua: &Lua, value: Value) -> Result<String> {
    Ok(match value {
        Value::Nil => "nil".to_string(),
        Value::Boolean(boolean) => boolean.to_string(),
        Value::Integer(integer) => integer.to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(string) => string.to_string_lossy(),

        Value::Function(_) => "<function>".to_string(),

        value => serde_json::to_string(&lua.from_value::<serde_json::Value>(value)?)?,
    })
}
