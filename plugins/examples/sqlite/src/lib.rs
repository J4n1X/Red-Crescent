//! The built-in `sqlite` module (src/sqlite_api.rs) as a plugin, with the
//! same Lua API: `open`, `NULL`, and a connection's `query`, `execute` and
//! `close`. The built-in holds the name `sqlite`, so load this one under
//! another, e.g. `sqlite3`.
//!
//! Options: `idle_connections`, warm connections kept per database and
//! worker thread (default 2, 0 turns reuse off), like
//! `--sqlite-idle-connections`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use red_crescent_plugin::prelude::*;
use rusqlite::types::{Value as Sql, ValueRef};

#[derive(serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Options {
    idle_connections: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            idle_connections: 2,
        }
    }
}

#[plugin]
mod sqlite {
    use super::*;

    struct Sqlite {
        data_dir: PathBuf,
        idle_limit: usize,
    }

    #[install]
    fn install(ctx: &Context) -> Result<Sqlite> {
        let options: Options = ctx.options()?;
        Ok(Sqlite {
            data_dir: ctx.data_dir().to_path_buf(),
            idle_limit: options.idle_connections,
        })
    }

    /// Binds SQL NULL where a bare `nil` would end the parameter array.
    #[constant]
    const NULL: Value<'static> = Value::Null;

    /// sqlite.open(path) -> Connection, relative to the data directory.
    #[function]
    fn open(sqlite: &Sqlite, path: &str) -> Result<Connection> {
        let path = resolve_db_path(&sqlite.data_dir, path)?;
        let conn = checkout(&path)?;
        Ok(Connection {
            conn: RefCell::new(Some(conn)),
            path,
            idle_limit: sqlite.idle_limit,
        })
    }

    #[handle]
    impl Connection {
        /// db:query(sql, params) -> { { column = value, ... }, ... }
        fn query<'c>(
            &self,
            call: &Call<'c>,
            sql: &str,
            params: Option<Table>,
        ) -> Result<Table<'c>> {
            let conn = self.conn.borrow();
            let mut stmt = live(&conn)?.prepare(sql)?;
            let names: Vec<String> = stmt.column_names().into_iter().map(String::from).collect();
            let mut rows = stmt.query(rusqlite::params_from_iter(bind(params)?))?;
            let result = call.new_table()?;
            let mut n = 0;
            while let Some(row) = rows.next()? {
                let table = call.new_table()?;
                for (i, name) in names.iter().enumerate() {
                    table.set(name, column(row.get_ref(i)?))?;
                }
                n += 1;
                result.set(n, table)?;
            }
            Ok(result)
        }

        /// db:execute(sql, params) -> { changes = n, last_insert_rowid = id }
        fn execute<'c>(
            &self,
            call: &Call<'c>,
            sql: &str,
            params: Option<Table>,
        ) -> Result<Table<'c>> {
            let conn = self.conn.borrow();
            let conn = live(&conn)?;
            let changes = conn.execute(sql, rusqlite::params_from_iter(bind(params)?))?;
            call.table_from([
                ("changes", changes as i64),
                ("last_insert_rowid", conn.last_insert_rowid()),
            ])
        }

        /// db:close(): parks the connection early; collection does it anyway.
        fn close(&self) {
            self.release();
        }
    }
}

pub struct Connection {
    conn: RefCell<Option<rusqlite::Connection>>,
    path: PathBuf,
    idle_limit: usize,
}

impl Connection {
    fn release(&self) {
        if let Some(conn) = self.conn.borrow_mut().take() {
            checkin(&self.path, conn, self.idle_limit);
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.release();
    }
}

fn live(conn: &Option<rusqlite::Connection>) -> Result<&rusqlite::Connection> {
    conn.as_ref()
        .ok_or_else(|| Error::new("attempt to use a closed sqlite connection"))
}

/// The parameter array as SQL values. A bare `nil` ends a Lua array, so
/// NULL is `sqlite.NULL`, or an explicit count in `n` (table.pack style).
fn bind(params: Option<Table>) -> Result<Vec<Sql>> {
    let Some(params) = params else {
        return Ok(Vec::new());
    };
    let count = match params.get::<Option<i64>>("n")? {
        Some(n) if n >= 0 => n as usize,
        _ => params.len()?,
    };
    (1..=count)
        .map(|i| {
            Ok(match params.get::<Value>(i)? {
                Value::Nil | Value::Null => Sql::Null,
                Value::Boolean(b) => Sql::Integer(b.into()),
                Value::Integer(n) => Sql::Integer(n),
                Value::Number(n) => Sql::Real(n),
                Value::String(s) => match std::str::from_utf8(s) {
                    Ok(text) => Sql::Text(text.to_owned()),
                    Err(_) => Sql::Blob(s.to_vec()),
                },
                other => bail!("unsupported sqlite parameter type: {}", other.type_name()),
            })
        })
        .collect()
}

/// A column as Lua sees it: NULL is nil, TEXT and BLOB are strings.
fn column(value: ValueRef<'_>) -> Value<'_> {
    match value {
        ValueRef::Null => Value::Nil,
        ValueRef::Integer(n) => Value::Integer(n),
        ValueRef::Real(n) => Value::Number(n),
        ValueRef::Text(s) | ValueRef::Blob(s) => Value::String(s),
    }
}

/// A database path strictly inside the data directory, with its parent
/// directories created.
fn resolve_db_path(data_dir: &Path, relpath: &str) -> Result<PathBuf> {
    let rel = Path::new(relpath);
    if rel.is_absolute() || rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        bail!("path must be relative and inside the data directory: {relpath}");
    }
    let path = data_dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::new(format!("cannot create parent directory: {e}")))?;
        // A symlink inside the data directory could still point out of it.
        if !parent.canonicalize()?.starts_with(data_dir) {
            bail!("path escapes the data directory: {relpath}");
        }
    }
    Ok(path)
}

thread_local! {
    /// Connections parked by a finished request, keyed by database path. See
    /// src/sqlite_api.rs for why: a warm connection saves the ~220us a fresh
    /// one pays on its first statement. Handles never cross threads, so a
    /// connection is parked on the thread that used it.
    static IDLE: RefCell<HashMap<PathBuf, Vec<rusqlite::Connection>>> =
        RefCell::new(HashMap::new());
}

fn checkout(path: &Path) -> rusqlite::Result<rusqlite::Connection> {
    let parked = IDLE.with(|idle| idle.borrow_mut().get_mut(path).and_then(Vec::pop));
    if let Some(conn) = parked {
        return Ok(conn);
    }
    let conn = rusqlite::Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.busy_timeout(Duration::from_secs(5))?;
    Ok(conn)
}

/// Parks a connection for the next request, or drops it if it cannot be
/// handed on cleanly.
fn checkin(path: &Path, conn: rusqlite::Connection, limit: usize) {
    if limit == 0 {
        return;
    }
    // An aborted transaction would hand the next request an open write lock.
    if !conn.is_autocommit() && conn.execute_batch("ROLLBACK").is_err() {
        return;
    }
    // Temp tables live on the connection; discard rather than enumerate them.
    let temp = conn.query_row("SELECT count(*) FROM sqlite_temp_master", [], |r| {
        r.get::<_, i64>(0)
    });
    if temp != Ok(0) || conn.execute_batch("PRAGMA foreign_keys = OFF").is_err() {
        return;
    }
    // `try_with`: a state retired at thread exit may outlive this.
    let _ = IDLE.try_with(|idle| {
        let mut idle = idle.borrow_mut();
        let conns = idle.entry(path.to_path_buf()).or_default();
        if conns.len() < limit {
            conns.push(conn);
        }
    });
}
