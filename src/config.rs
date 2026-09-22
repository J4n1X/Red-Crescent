//! Server configuration.
//!
//! Settings come from three places, in decreasing priority: command-line
//! flags, environment variables, and an optional `rc_config.lua` in the serve
//! directory. Anything still unset falls back to the defaults below.
//!
//! The file is Lua because the server already embeds it: no new dependency,
//! and limits read as arithmetic. It lives in the serve directory so an
//! application can ship its own settings; `.lua` is never served, so it stays
//! private.

use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Parser;
use mlua::{Lua, LuaSerdeExt, Table, Value};
use serde::Deserialize;

pub const DEFAULT_BIND: &str = "127.0.0.1:8080";
pub const DEFAULT_SERVE_DIR: &str = "./demos";
pub const DEFAULT_DATA_DIR: &str = "./data";
pub const DEFAULT_INDEX: &str = "index.lhtml";
/// One per core: a template rendering on its worker can use no more cores
/// than there are workers.
pub fn default_workers() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get())
}
pub const DEFAULT_TIMEOUT_MS: u64 = 5000;
pub const DEFAULT_MEMORY_LIMIT_MB: usize = 64;
/// Higher than the request cap: a thread is long-lived and hitting this kills
/// only that thread.
pub const DEFAULT_THREAD_MEMORY_LIMIT_MB: usize = 256;
/// Warm SQLite connections parked per database, per worker thread.
pub const DEFAULT_SQLITE_IDLE_CONNECTIONS: usize = 2;
/// A backstop against slow accumulation, so it is generous.
pub const DEFAULT_LUA_POOL_MAX_REQUESTS: u32 = 10_000;
/// Roughly forty pool handoffs' worth of CPU, and short enough that a
/// connection waiting behind it barely notices.
pub const DEFAULT_INLINE_RENDER_BUDGET_US: u64 = 1000;
pub const DEFAULT_MAX_BODY_SIZE: usize = 1024 * 1024;
pub const DEFAULT_MAX_UPLOAD_SIZE: usize = 256 * 1024 * 1024;
pub const DEFAULT_MAX_UPLOAD_FILES: usize = 256;

/// The file looked for inside the serve directory.
pub const CONFIG_FILE_NAME: &str = "rc_config.lua";

/// How long the config file itself may run before being aborted.
const CONFIG_EVAL_TIMEOUT: Duration = Duration::from_secs(10);

/// Red Crescent — a PHP-like web server rendering .lhtml templates with Lua.
///
/// Flags override environment variables, which override `rc_config.lua` in
/// the serve directory.
///
/// One struct serves both sources: clap derives the command line from it and
/// serde deserializes the Lua table into it. A new setting is therefore
/// declared once here, once in [`Config`], and merged in one line — rather
/// than in a hand-written parser that has to be kept in step.
#[derive(Parser, Deserialize, Debug, Clone, Default)]
#[command(version, about)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Address to bind [default: 127.0.0.1:8080]
    #[arg(long, env = "RC_BIND")]
    pub bind: Option<String>,

    /// Directory to serve templates and static files from, and where
    /// rc_config.lua is looked for [default: ./demos]
    #[arg(long, env = "RC_SERVE_DIR")]
    #[serde(skip)]
    pub serve_dir: Option<PathBuf>,

    /// Path to the config file [default: rc_config.lua in the serve directory]
    #[arg(long, env = "RC_CONFIG")]
    #[serde(skip)]
    pub config: Option<PathBuf>,

    /// Ignore the config file entirely
    #[arg(long, default_value_t = false)]
    #[serde(skip)]
    pub no_config: bool,

    /// Number of HTTP worker threads [default: one per CPU core]
    #[arg(long, env = "RC_WORKERS")]
    pub workers: Option<usize>,

    /// Per-request Lua execution time limit in milliseconds [default: 5000]
    #[arg(long, env = "RC_TIMEOUT_MS")]
    pub timeout_ms: Option<u64>,

    /// Per-request Lua memory limit in megabytes [default: 64]
    #[arg(long, env = "RC_MEMORY_LIMIT_MB")]
    pub memory_limit_mb: Option<usize>,

    /// Maximum request body size in bytes, non-multipart [default: 1048576]
    #[arg(long, env = "RC_MAX_BODY_SIZE")]
    pub max_body_size: Option<usize>,

    /// Writable data directory: sandbox for sqlite databases, uploads and
    /// send_file; must not be inside the serve directory [default: ./data]
    #[arg(long, env = "RC_DATA_DIR")]
    pub data_dir: Option<PathBuf>,

    /// Maximum total size of a multipart upload in bytes [default: 268435456]
    #[arg(long, env = "RC_MAX_UPLOAD_SIZE")]
    pub max_upload_size: Option<usize>,

    /// Maximum number of file parts in one upload [default: 256]
    #[arg(long, env = "RC_MAX_UPLOAD_FILES")]
    pub max_upload_files: Option<usize>,

    /// Serve non-.lhtml files as static assets [default: true]
    #[arg(long, env = "RC_STATIC_FILES", num_args = 0..=1, default_missing_value = "true")]
    pub static_files: Option<bool>,

    /// File served for `/` and directory paths [default: index.lhtml]
    #[arg(long, env = "RC_INDEX")]
    pub index: Option<String>,

    /// Template rendered when a path resolves to no file, relative to the
    /// serve directory — a front controller. Unset means 404 [default: none]
    #[arg(long, env = "RC_FALLBACK")]
    pub fallback: Option<String>,

    /// Dev mode: detailed error pages and no template caching
    #[arg(long, env = "RC_DEV", num_args = 0..=1, default_missing_value = "true")]
    pub dev: Option<bool>,

    /// Background Lua thread script spawned at boot, relative to the serve
    /// directory (repeatable)
    #[arg(long = "thread", env = "RC_THREADS", value_delimiter = ',')]
    pub threads: Vec<String>,

    /// Execution time limit for one awake stretch of a background thread, in
    /// milliseconds. 0 or unset means no deadline [default: none]
    #[arg(long, env = "RC_THREAD_TIMEOUT_MS")]
    pub thread_timeout_ms: Option<u64>,

    /// Lua memory limit for a background thread instance in megabytes
    /// [default: 256]
    #[arg(long, env = "RC_THREAD_MEMORY_LIMIT_MB")]
    pub thread_memory_limit_mb: Option<usize>,

    /// Warm SQLite connections kept per database, per worker thread. 0 opens
    /// a fresh connection every time [default: 2]
    #[arg(long, env = "RC_SQLITE_IDLE_CONNECTIONS")]
    pub sqlite_idle_connections: Option<usize>,

    /// Directory of Lua C modules (.so) that `require` may load (repeatable).
    /// Naming any directory enables native modules, which switches the Lua
    /// state out of mlua's safe mode — see the README [default: none]
    #[arg(long = "c-module-dir", env = "RC_C_MODULE_DIRS", value_delimiter = ',')]
    pub c_module_dirs: Vec<PathBuf>,

    /// Reuse Lua states between requests instead of building one per request.
    /// Always off when C modules are enabled [default: true]
    #[arg(long, env = "RC_LUA_POOL", num_args = 0..=1, default_missing_value = "true")]
    pub lua_pool: Option<bool>,

    /// Requests one pooled Lua state serves before it is retired
    /// [default: 10000]
    #[arg(long, env = "RC_LUA_POOL_MAX_REQUESTS")]
    pub lua_pool_max_requests: Option<u32>,

    /// Templates that reliably render within this many microseconds run on
    /// the HTTP worker instead of the blocking pool. 0 always uses the pool
    /// [default: 1000]
    #[arg(long, env = "RC_INLINE_RENDER_BUDGET_US")]
    pub inline_render_budget_us: Option<u64>,
}

impl Settings {
    /// The serve directory, which must be known before the config file can be
    /// found — so it is the one setting the file cannot provide.
    pub fn serve_dir(&self) -> PathBuf {
        self.serve_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from(DEFAULT_SERVE_DIR))
    }

    /// Where to look for the config file, if enabled.
    pub fn config_path(&self, serve_dir: &Path) -> Option<PathBuf> {
        if self.no_config {
            return None;
        }
        Some(
            self.config
                .clone()
                .unwrap_or_else(|| serve_dir.join(CONFIG_FILE_NAME)),
        )
    }
}

/// Fully resolved configuration used by the rest of the server.
#[derive(Debug, Clone)]
pub struct Config {
    pub bind: String,
    pub serve_dir: PathBuf,
    pub workers: usize,
    pub timeout_ms: u64,
    pub memory_limit_mb: usize,
    pub max_body_size: usize,
    pub data_dir: PathBuf,
    pub max_upload_size: usize,
    pub max_upload_files: usize,
    pub static_files: bool,
    pub index: String,
    /// Template for paths that resolve to nothing, relative to the serve
    /// directory. `None` — the default — makes those a 404.
    pub fallback: Option<String>,
    pub dev: bool,
    pub threads: Vec<String>,
    /// Deadline for one awake stretch of a background thread. `None` — the
    /// default — means threads run without an execution deadline.
    pub thread_timeout_ms: Option<u64>,
    /// Memory cap for a background thread instance, for its whole life.
    pub thread_memory_limit_mb: usize,
    /// Warm SQLite connections parked per database, per thread. 0 disables reuse.
    pub sqlite_idle_connections: usize,
    /// Directories `require` may load native `.so` modules from. Empty — the
    /// default — leaves C modules disabled entirely.
    pub c_module_dirs: Vec<PathBuf>,
    /// Reuse Lua states between requests. Ignored when `c_module_dirs` is
    /// non-empty, which forces a fresh state per request.
    pub lua_pool: bool,
    /// Requests one pooled state serves before it is retired.
    pub lua_pool_max_requests: u32,
    /// Render time under which a template may run on the HTTP worker; 0 never.
    pub inline_render_budget_us: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config::resolve(&Settings::default(), Settings::default())
    }
}

/// Flags win, then the file, then the built-in default.
fn pick<T>(flag: Option<T>, file: Option<T>, fallback: T) -> T {
    flag.or(file).unwrap_or(fallback)
}

/// A repeatable flag is "unset" when empty, so the file only applies then.
fn pick_list<T: Clone>(flag: &[T], file: Vec<T>) -> Vec<T> {
    if flag.is_empty() { file } else { flag.to_vec() }
}

impl Config {
    /// Merge the three sources: flags and environment win over the file,
    /// which wins over the defaults.
    pub fn resolve(cli: &Settings, file: Settings) -> Config {
        Config {
            serve_dir: cli.serve_dir(),
            bind: pick(cli.bind.clone(), file.bind, DEFAULT_BIND.to_string()),
            workers: pick(cli.workers, file.workers, default_workers()),
            timeout_ms: pick(cli.timeout_ms, file.timeout_ms, DEFAULT_TIMEOUT_MS),
            memory_limit_mb: pick(
                cli.memory_limit_mb,
                file.memory_limit_mb,
                DEFAULT_MEMORY_LIMIT_MB,
            ),
            max_body_size: pick(cli.max_body_size, file.max_body_size, DEFAULT_MAX_BODY_SIZE),
            data_dir: pick(
                cli.data_dir.clone(),
                file.data_dir,
                PathBuf::from(DEFAULT_DATA_DIR),
            ),
            max_upload_size: pick(
                cli.max_upload_size,
                file.max_upload_size,
                DEFAULT_MAX_UPLOAD_SIZE,
            ),
            max_upload_files: pick(
                cli.max_upload_files,
                file.max_upload_files,
                DEFAULT_MAX_UPLOAD_FILES,
            ),
            static_files: pick(cli.static_files, file.static_files, true),
            index: pick(cli.index.clone(), file.index, DEFAULT_INDEX.to_string()),
            fallback: cli.fallback.clone().or(file.fallback),
            dev: pick(cli.dev, file.dev, false),
            threads: pick_list(&cli.threads, file.threads),
            thread_timeout_ms: cli.thread_timeout_ms.or(file.thread_timeout_ms),
            thread_memory_limit_mb: pick(
                cli.thread_memory_limit_mb,
                file.thread_memory_limit_mb,
                DEFAULT_THREAD_MEMORY_LIMIT_MB,
            ),
            sqlite_idle_connections: pick(
                cli.sqlite_idle_connections,
                file.sqlite_idle_connections,
                DEFAULT_SQLITE_IDLE_CONNECTIONS,
            ),
            c_module_dirs: pick_list(&cli.c_module_dirs, file.c_module_dirs),
            lua_pool: pick(cli.lua_pool, file.lua_pool, true),
            lua_pool_max_requests: pick(
                cli.lua_pool_max_requests,
                file.lua_pool_max_requests,
                DEFAULT_LUA_POOL_MAX_REQUESTS,
            ),
            inline_render_budget_us: pick(
                cli.inline_render_budget_us,
                file.inline_render_budget_us,
                DEFAULT_INLINE_RENDER_BUDGET_US,
            ),
        }
    }
}

/// Lua's `^` yields a float, so `2^20` arrives as `1048576.0` and serde
/// rejects it for an integer field. Rewriting whole floats as integers keeps
/// arithmetic working for every numeric setting without per-field annotations.
fn integerize(lua: &Lua, value: Value) -> mlua::Result<Value> {
    const LIMIT: f64 = i64::MAX as f64;
    match value {
        Value::Number(n) if n.is_finite() && n.fract() == 0.0 && n.abs() <= LIMIT => {
            Ok(Value::Integer(n as i64))
        }
        Value::Table(table) => {
            let out = lua.create_table()?;
            for pair in table.pairs::<Value, Value>() {
                let (key, value) = pair?;
                out.set(key, integerize(lua, value)?)?;
            }
            Ok(Value::Table(out))
        }
        other => Ok(other),
    }
}

/// Load `rc_config.lua`. A missing file is not an error — it simply means
/// every setting comes from flags and defaults.
pub fn load_file_config(path: &Path) -> Result<Settings, String> {
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Settings::default()),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };

    let lua = Lua::new();
    // The config file is operator code like any template, but a runaway loop
    // here would hang startup with no page to show, so it gets a hard cap.
    let deadline = std::time::Instant::now() + CONFIG_EVAL_TIMEOUT;
    lua.set_global_hook(
        mlua::HookTriggers::new().every_nth_instruction(10_000),
        move |_lua, _debug| {
            if std::time::Instant::now() >= deadline {
                Err(mlua::Error::runtime(
                    "config file took too long to evaluate",
                ))
            } else {
                Ok(mlua::VmState::Continue)
            }
        },
    )
    .map_err(|e| format!("{}: {e}", path.display()))?;

    let table: Table = lua
        .load(source.as_str())
        .set_name(format!("@{}", path.display()))
        .eval()
        .map_err(|e| format!("{}: {e}", path.display()))?;

    // serde would report this as an unknown field; the reason is worth stating.
    if table.contains_key("serve_dir").unwrap_or(false) {
        return Err(format!(
            "{}: 'serve_dir' cannot be set here — the config file is found \
             *inside* the serve directory. Use --serve-dir or RC_SERVE_DIR.",
            path.display()
        ));
    }

    let value =
        integerize(&lua, Value::Table(table)).map_err(|e| format!("{}: {e}", path.display()))?;
    lua.from_value(value)
        .map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests run in parallel threads, so each call needs its own file.
    static NEXT_CONFIG_FILE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn file_config_from(source: &str) -> Result<Settings, String> {
        let n = NEXT_CONFIG_FILE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("rc-cfg-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(CONFIG_FILE_NAME);
        std::fs::write(&path, source).unwrap();
        let result = load_file_config(&path);
        std::fs::remove_dir_all(&dir).ok();
        result
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let config = load_file_config(Path::new("/nonexistent/rc_config.lua")).unwrap();
        assert!(config.bind.is_none());
    }

    #[test]
    fn reads_settings_including_arithmetic() {
        let config = file_config_from(
            r#"return {
                bind = "0.0.0.0:9000",
                max_upload_size = 8 * 1024 * 1024 * 1024,
                max_body_size = 2^20,
                dev = true,
                threads = { "jobs/a.lua", "jobs/b.lua" },
            }"#,
        )
        .unwrap();
        assert_eq!(config.bind.as_deref(), Some("0.0.0.0:9000"));
        assert_eq!(config.max_upload_size, Some(8 * 1024 * 1024 * 1024));
        assert_eq!(config.max_body_size, Some(1024 * 1024));
        assert_eq!(config.dev, Some(true));
        assert_eq!(config.threads, ["jobs/a.lua", "jobs/b.lua"]);
    }

    #[test]
    fn typos_are_rejected_rather_than_ignored() {
        let err = file_config_from("return { max_upload_sizes = 5 }").unwrap_err();
        assert!(err.contains("unknown field"), "{err}");
        assert!(err.contains("max_upload_sizes"), "{err}");
        // serde lists the valid keys itself, so the hint cannot go stale.
        assert!(err.contains("max_upload_size"), "{err}");
    }

    #[test]
    fn serve_dir_is_rejected_with_an_explanation() {
        let err = file_config_from(r#"return { serve_dir = "/tmp" }"#).unwrap_err();
        assert!(err.contains("--serve-dir"), "{err}");
    }

    #[test]
    fn wrong_types_are_rejected() {
        assert!(file_config_from(r#"return { workers = "many" }"#).is_err());
        assert!(file_config_from("return { dev = 1 }").is_err());
        assert!(file_config_from("return { workers = 1.5 }").is_err());
        assert!(file_config_from(r#"return { threads = "one.lua" }"#).is_err());
    }

    #[test]
    fn a_non_table_return_is_rejected() {
        assert!(file_config_from("return 42").is_err());
        assert!(file_config_from("-- nothing returned").is_err());
    }

    #[test]
    fn reads_c_module_dirs() {
        let config =
            file_config_from(r#"return { c_module_dirs = { "/usr/lib/lua/5.4", "/opt/lua" } }"#)
                .unwrap();
        assert_eq!(
            config.c_module_dirs,
            [PathBuf::from("/usr/lib/lua/5.4"), PathBuf::from("/opt/lua")]
        );
    }

    #[test]
    fn c_module_dirs_must_be_an_array() {
        assert!(file_config_from(r#"return { c_module_dirs = "/usr/lib" }"#).is_err());
        assert!(file_config_from(r#"return { c_module_dirs = true }"#).is_err());
    }

    #[test]
    fn fallback_is_unset_by_default_and_readable_from_the_file() {
        assert!(Config::default().fallback.is_none());
        let config = file_config_from(r#"return { fallback = "app.lhtml" }"#).unwrap();
        assert_eq!(config.fallback.as_deref(), Some("app.lhtml"));
        assert!(file_config_from("return { fallback = true }").is_err());
    }

    #[test]
    fn reads_sqlite_idle_connections() {
        let config = file_config_from("return { sqlite_idle_connections = 8 }").unwrap();
        assert_eq!(config.sqlite_idle_connections, Some(8));
        assert!(file_config_from("return { sqlite_idle_connections = 'x' }").is_err());
    }

    #[test]
    fn c_modules_are_off_when_unset() {
        let resolved = Config::resolve(&Settings::default(), Settings::default());
        assert!(resolved.c_module_dirs.is_empty());
    }

    #[test]
    fn flags_win_over_the_file_which_wins_over_defaults() {
        let cli = Settings {
            bind: Some("1.2.3.4:80".to_string()),
            ..Default::default()
        };
        let file = Settings {
            bind: Some("9.9.9.9:99".to_string()),
            workers: Some(16),
            ..Default::default()
        };
        let resolved = Config::resolve(&cli, file);
        assert_eq!(resolved.bind, "1.2.3.4:80", "flag must win");
        assert_eq!(resolved.workers, 16, "file must fill the gap");
        assert_eq!(
            resolved.timeout_ms, DEFAULT_TIMEOUT_MS,
            "default must fill the rest"
        );
    }
}
