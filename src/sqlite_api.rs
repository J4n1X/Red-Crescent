//! The Lua-facing `sqlite` module, backed by rusqlite (bundled SQLite).
//!
//! Database files are confined to the data directory. Parameters always go
//! through prepared-statement binding, so templates get injection-safe SQL
//! by construction.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use mlua::{LightUserData, Lua, Table, UserData, UserDataMethods, Value};
use rusqlite::Connection;
use rusqlite::types::Value as SqlValue;

pub fn register(lua: &Lua, data_dir: &Path) -> mlua::Result<()> {
    let sqlite = lua.create_table()?;

    let dir = data_dir.to_path_buf();
    sqlite.set(
        "open",
        lua.create_function(move |_, relpath: String| {
            let path = resolve_db_path(&dir, &relpath)?;
            let conn = Connection::open(&path).map_err(mlua::Error::external)?;
            conn.pragma_update(None, "journal_mode", "WAL")
                .map_err(mlua::Error::external)?;
            conn.busy_timeout(Duration::from_secs(5))
                .map_err(mlua::Error::external)?;
            Ok(LuaConnection(Some(conn)))
        })?,
    )?;

    // SQL NULL sentinel: a plain `nil` inside a Lua array truncates it, so
    // parameters after it would silently vanish. `value or sqlite.NULL` is
    // the safe way to bind an optional value.
    sqlite.set(
        "NULL",
        Value::LightUserData(LightUserData(std::ptr::null_mut())),
    )?;

    lua.globals().set("sqlite", &sqlite)?;
    lua.register_module("sqlite", sqlite)?;
    Ok(())
}

/// Resolve a database path strictly inside the data directory, creating
/// parent directories as needed.
fn resolve_db_path(data_dir: &Path, relpath: &str) -> mlua::Result<PathBuf> {
    let rel = Path::new(relpath);
    if rel.is_absolute() || rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(mlua::Error::runtime(format!(
            "sqlite.open: path must be relative and inside the data directory: {relpath}"
        )));
    }
    let path = data_dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            mlua::Error::runtime(format!("sqlite.open: cannot create parent directory: {e}"))
        })?;
        // Symlinks inside data-dir could still point elsewhere; canonicalize
        // the (now existing) parent and re-check.
        let canonical_parent = parent.canonicalize().map_err(mlua::Error::external)?;
        if !canonical_parent.starts_with(data_dir) {
            return Err(mlua::Error::runtime(format!(
                "sqlite.open: path escapes the data directory: {relpath}"
            )));
        }
    }
    Ok(path)
}

struct LuaConnection(Option<Connection>);

impl LuaConnection {
    fn get(&self) -> mlua::Result<&Connection> {
        self.0
            .as_ref()
            .ok_or_else(|| mlua::Error::runtime("attempt to use a closed sqlite connection"))
    }
}

impl UserData for LuaConnection {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method(
            "query",
            |lua, this, (sql, params): (String, Option<Table>)| {
                let conn = this.get()?;
                let values = lua_params_to_sql(params)?;
                let mut stmt = conn.prepare(&sql).map_err(mlua::Error::external)?;
                let names: Vec<String> =
                    stmt.column_names().iter().map(|s| s.to_string()).collect();
                let mut rows = stmt
                    .query(rusqlite::params_from_iter(values))
                    .map_err(mlua::Error::external)?;

                let result = lua.create_table()?;
                while let Some(row) = rows.next().map_err(mlua::Error::external)? {
                    let row_table = lua.create_table()?;
                    for (i, name) in names.iter().enumerate() {
                        let value: SqlValue = row.get(i).map_err(mlua::Error::external)?;
                        row_table.set(name.as_str(), sql_value_to_lua(lua, value)?)?;
                    }
                    result.push(row_table)?;
                }
                Ok(result)
            },
        );

        methods.add_method(
            "execute",
            |lua, this, (sql, params): (String, Option<Table>)| {
                let conn = this.get()?;
                let values = lua_params_to_sql(params)?;
                let changes = conn
                    .execute(&sql, rusqlite::params_from_iter(values))
                    .map_err(mlua::Error::external)?;
                let result = lua.create_table()?;
                result.set("changes", changes as i64)?;
                result.set("last_insert_rowid", conn.last_insert_rowid())?;
                Ok(result)
            },
        );

        methods.add_method_mut("close", |_, this, ()| {
            this.0.take();
            Ok(())
        });
    }
}

/// Convert the Lua parameter array to SQL values. Bind NULL with
/// `sqlite.NULL` (preferred) or pass an explicit count via an `n` field
/// (table.pack style) — a bare `nil` inside an array truncates it in Lua.
fn lua_params_to_sql(params: Option<Table>) -> mlua::Result<Vec<SqlValue>> {
    let Some(params) = params else {
        return Ok(Vec::new());
    };
    let count = match params.raw_get::<Option<i64>>("n")? {
        Some(n) if n >= 0 => n,
        _ => params.raw_len() as i64,
    };
    let mut values = Vec::new();
    for i in 1..=count {
        values.push(match params.raw_get::<Value>(i)? {
            Value::Nil => SqlValue::Null,
            Value::LightUserData(l) if l.0.is_null() => SqlValue::Null,
            Value::Boolean(b) => SqlValue::Integer(b.into()),
            Value::Integer(n) => SqlValue::Integer(n),
            Value::Number(n) => SqlValue::Real(n),
            Value::String(s) => match s.to_str() {
                Ok(text) => SqlValue::Text(text.to_string()),
                Err(_) => SqlValue::Blob(s.as_bytes().to_vec()),
            },
            other => {
                return Err(mlua::Error::runtime(format!(
                    "unsupported sqlite parameter type: {}",
                    other.type_name()
                )));
            }
        });
    }
    Ok(values)
}

fn sql_value_to_lua(lua: &Lua, value: SqlValue) -> mlua::Result<Value> {
    Ok(match value {
        SqlValue::Null => Value::Nil,
        SqlValue::Integer(n) => Value::Integer(n),
        SqlValue::Real(n) => Value::Number(n),
        SqlValue::Text(s) => Value::String(lua.create_string(&s)?),
        SqlValue::Blob(b) => Value::String(lua.create_string(&b)?),
    })
}
