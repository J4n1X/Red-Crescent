//! The Lua-facing `sqlite` module, backed by rusqlite (bundled SQLite).
//!
//! Database files are confined to the data directory. Parameters always go
//! through prepared-statement binding, so templates get injection-safe SQL
//! by construction.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use mlua::{LightUserData, Lua, Table, UserData, UserDataMethods, Value};
use rusqlite::Connection;
use rusqlite::types::Value as SqlValue;

pub fn register(lua: &Lua, data_dir: &Path, idle_limit: usize) -> mlua::Result<()> {
    let sqlite = lua.create_table()?;

    let dir = data_dir.to_path_buf();
    sqlite.set(
        "open",
        lua.create_function(move |_, relpath: String| {
            let path = resolve_db_path(&dir, &relpath)?;
            let conn = checkout(&path).map_err(mlua::Error::external)?;
            crate::api::mark_finalizers();
            Ok(LuaConnection {
                conn: Some(conn),
                path,
                idle_limit,
            })
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

thread_local! {
    /// Connections parked by a finished request, keyed by database path.
    ///
    /// SQLite opens lazily, so the first statement on a new connection pays
    /// ~220us for the file open and schema load against 0.7us on a warm one.
    /// Thread-local because `web::block` already runs on a long-lived pool, so
    /// no mutex or checkout protocol is needed. A database is shared between
    /// requests by definition, so this weakens no isolation guarantee.
    static IDLE: RefCell<HashMap<PathBuf, Vec<Connection>>> = RefCell::new(HashMap::new());
}

/// Take a parked connection, or open a fresh one. `journal_mode` is persistent
/// and `busy_timeout` is per-connection, so a reused one needs neither.
fn checkout(path: &Path) -> rusqlite::Result<Connection> {
    let parked = IDLE.with(|idle| {
        idle.borrow_mut()
            .get_mut(path)
            .and_then(|conns| conns.pop())
    });
    if let Some(conn) = parked {
        return Ok(conn);
    }
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.busy_timeout(Duration::from_secs(5))?;
    Ok(conn)
}

/// Park a connection for the next request, or drop it if it cannot be handed
/// on cleanly. Anything unexpected drops it; the next open just pays in full.
fn checkin(path: &Path, conn: Connection, limit: usize) {
    if limit == 0 {
        return;
    }
    // An aborted transaction would hand the next request an open write lock.
    if !conn.is_autocommit() && conn.execute_batch("ROLLBACK").is_err() {
        return;
    }
    // Temp tables live on the connection; discard rather than enumerate them.
    match conn.query_row("SELECT count(*) FROM sqlite_temp_master", [], |r| {
        r.get::<_, i64>(0)
    }) {
        Ok(0) => {}
        _ => return,
    }
    // Off on a fresh connection, so reset it rather than inherit last request's.
    if conn.execute_batch("PRAGMA foreign_keys = OFF").is_err() {
        return;
    }
    // `try_with`, not `with`: a pooled Lua state is dropped by its own
    // thread-local destructor, and if this one went first, `with` would panic
    // inside a destructor and abort the process. Dropping the connection is
    // the right answer at that point anyway.
    let _ = IDLE.try_with(|idle| {
        let mut idle = idle.borrow_mut();
        let conns = idle.entry(path.to_path_buf()).or_default();
        if conns.len() < limit {
            conns.push(conn);
        }
    });
}

struct LuaConnection {
    conn: Option<Connection>,
    path: PathBuf,
    idle_limit: usize,
}

impl LuaConnection {
    fn get(&self) -> mlua::Result<&Connection> {
        self.conn
            .as_ref()
            .ok_or_else(|| mlua::Error::runtime("attempt to use a closed sqlite connection"))
    }

    fn release(&mut self) {
        if let Some(conn) = self.conn.take() {
            checkin(&self.path, conn, self.idle_limit);
        }
    }
}

impl Drop for LuaConnection {
    fn drop(&mut self) {
        self.release();
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
            this.release();
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
