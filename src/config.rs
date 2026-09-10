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
use mlua::{Lua, Table, Value};

pub const DEFAULT_BIND: &str = "127.0.0.1:8080";
pub const DEFAULT_SERVE_DIR: &str = "./demos";
pub const DEFAULT_DATA_DIR: &str = "./data";
pub const DEFAULT_INDEX: &str = "index.lhtml";
pub const DEFAULT_WORKERS: usize = 4;
pub const DEFAULT_TIMEOUT_MS: u64 = 5000;
pub const DEFAULT_MEMORY_LIMIT_MB: usize = 64;
/// Higher than the request cap: a thread is long-lived and hitting this kills
/// only that thread.
pub const DEFAULT_THREAD_MEMORY_LIMIT_MB: usize = 256;
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
#[derive(Parser, Debug, Clone, Default)]
#[command(version, about)]
pub struct CliArgs {
    /// Address to bind [default: 127.0.0.1:8080]
    #[arg(long, env = "RC_BIND")]
    pub bind: Option<String>,

    /// Directory to serve templates and static files from, and where
    /// rc_config.lua is looked for [default: ./demos]
    #[arg(long, env = "RC_SERVE_DIR")]
    pub serve_dir: Option<PathBuf>,

    /// Path to the config file [default: rc_config.lua in the serve directory]
    #[arg(long, env = "RC_CONFIG")]
    pub config: Option<PathBuf>,

    /// Ignore the config file entirely
    #[arg(long, default_value_t = false)]
    pub no_config: bool,

    /// Number of HTTP worker threads [default: 4]
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

    /// Directory of Lua C modules (.so) that `require` may load (repeatable).
    /// Naming any directory enables native modules, which switches the Lua
    /// state out of mlua's safe mode — see the README [default: none]
    #[arg(long = "c-module-dir", env = "RC_C_MODULE_DIRS", value_delimiter = ',')]
    pub c_module_dirs: Vec<PathBuf>,
}

impl CliArgs {
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

/// Settings read from `rc_config.lua`. Every field is optional; anything the
/// file omits keeps its flag value or default.
#[derive(Debug, Clone, Default)]
pub struct FileConfig {
    pub bind: Option<String>,
    pub workers: Option<usize>,
    pub timeout_ms: Option<u64>,
    pub memory_limit_mb: Option<usize>,
    pub max_body_size: Option<usize>,
    pub data_dir: Option<PathBuf>,
    pub max_upload_size: Option<usize>,
    pub max_upload_files: Option<usize>,
    pub static_files: Option<bool>,
    pub index: Option<String>,
    pub dev: Option<bool>,
    pub threads: Option<Vec<String>>,
    pub thread_timeout_ms: Option<u64>,
    pub thread_memory_limit_mb: Option<usize>,
    pub c_module_dirs: Option<Vec<PathBuf>>,
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
    pub dev: bool,
    pub threads: Vec<String>,
    /// Deadline for one awake stretch of a background thread. `None` — the
    /// default — means threads run without an execution deadline.
    pub thread_timeout_ms: Option<u64>,
    /// Memory cap for a background thread instance, for its whole life.
    pub thread_memory_limit_mb: usize,
    /// Directories `require` may load native `.so` modules from. Empty — the
    /// default — leaves C modules disabled entirely.
    pub c_module_dirs: Vec<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            bind: DEFAULT_BIND.to_string(),
            serve_dir: PathBuf::from(DEFAULT_SERVE_DIR),
            workers: DEFAULT_WORKERS,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            memory_limit_mb: DEFAULT_MEMORY_LIMIT_MB,
            max_body_size: DEFAULT_MAX_BODY_SIZE,
            data_dir: PathBuf::from(DEFAULT_DATA_DIR),
            max_upload_size: DEFAULT_MAX_UPLOAD_SIZE,
            max_upload_files: DEFAULT_MAX_UPLOAD_FILES,
            static_files: true,
            index: DEFAULT_INDEX.to_string(),
            dev: false,
            threads: Vec::new(),
            thread_timeout_ms: None,
            thread_memory_limit_mb: DEFAULT_THREAD_MEMORY_LIMIT_MB,
            c_module_dirs: Vec::new(),
        }
    }
}

impl Config {
    /// Merge the three sources: flags and environment win over the file,
    /// which wins over the defaults.
    pub fn resolve(cli: &CliArgs, file: FileConfig) -> Config {
        let defaults = Config::default();
        Config {
            serve_dir: cli.serve_dir(),
            bind: cli.bind.clone().or(file.bind).unwrap_or(defaults.bind),
            workers: cli.workers.or(file.workers).unwrap_or(defaults.workers),
            timeout_ms: cli
                .timeout_ms
                .or(file.timeout_ms)
                .unwrap_or(defaults.timeout_ms),
            memory_limit_mb: cli
                .memory_limit_mb
                .or(file.memory_limit_mb)
                .unwrap_or(defaults.memory_limit_mb),
            max_body_size: cli
                .max_body_size
                .or(file.max_body_size)
                .unwrap_or(defaults.max_body_size),
            data_dir: cli
                .data_dir
                .clone()
                .or(file.data_dir)
                .unwrap_or(defaults.data_dir),
            max_upload_size: cli
                .max_upload_size
                .or(file.max_upload_size)
                .unwrap_or(defaults.max_upload_size),
            max_upload_files: cli
                .max_upload_files
                .or(file.max_upload_files)
                .unwrap_or(defaults.max_upload_files),
            static_files: cli
                .static_files
                .or(file.static_files)
                .unwrap_or(defaults.static_files),
            index: cli.index.clone().or(file.index).unwrap_or(defaults.index),
            dev: cli.dev.or(file.dev).unwrap_or(defaults.dev),
            threads: if !cli.threads.is_empty() {
                cli.threads.clone()
            } else {
                file.threads.unwrap_or(defaults.threads)
            },
            thread_timeout_ms: cli.thread_timeout_ms.or(file.thread_timeout_ms),
            thread_memory_limit_mb: cli
                .thread_memory_limit_mb
                .or(file.thread_memory_limit_mb)
                .unwrap_or(defaults.thread_memory_limit_mb),
            c_module_dirs: if !cli.c_module_dirs.is_empty() {
                cli.c_module_dirs.clone()
            } else {
                file.c_module_dirs.unwrap_or(defaults.c_module_dirs)
            },
        }
    }
}

/// Load `rc_config.lua`. A missing file is not an error — it simply means
/// every setting comes from flags and defaults.
pub fn load_file_config(path: &Path) -> Result<FileConfig, String> {
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(FileConfig::default()),
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

    parse_table(&table).map_err(|e| format!("{}: {e}", path.display()))
}

fn parse_table(table: &Table) -> Result<FileConfig, String> {
    let mut config = FileConfig::default();

    for pair in table.pairs::<Value, Value>() {
        let (key, value) = pair.map_err(|e| e.to_string())?;
        let Value::String(key) = key else {
            return Err("configuration keys must be strings".to_string());
        };
        let key = key.to_string_lossy();

        match key.as_str() {
            "bind" => config.bind = Some(as_string(&value, &key)?),
            "workers" => config.workers = Some(as_usize(&value, &key)?),
            "timeout_ms" => config.timeout_ms = Some(as_u64(&value, &key)?),
            "memory_limit_mb" => config.memory_limit_mb = Some(as_usize(&value, &key)?),
            "max_body_size" => config.max_body_size = Some(as_usize(&value, &key)?),
            "data_dir" => config.data_dir = Some(PathBuf::from(as_string(&value, &key)?)),
            "max_upload_size" => config.max_upload_size = Some(as_usize(&value, &key)?),
            "max_upload_files" => config.max_upload_files = Some(as_usize(&value, &key)?),
            "static_files" => config.static_files = Some(as_bool(&value, &key)?),
            "index" => config.index = Some(as_string(&value, &key)?),
            "dev" => config.dev = Some(as_bool(&value, &key)?),
            "threads" => {
                let Value::Table(list) = &value else {
                    return Err("'threads' must be an array of script paths".to_string());
                };
                let mut scripts = Vec::new();
                for item in list.clone().sequence_values::<Value>() {
                    let item = item.map_err(|e| e.to_string())?;
                    scripts.push(as_string(&item, "threads")?);
                }
                config.threads = Some(scripts);
            }
            "thread_timeout_ms" => config.thread_timeout_ms = Some(as_u64(&value, &key)?),
            "thread_memory_limit_mb" => {
                config.thread_memory_limit_mb = Some(as_usize(&value, &key)?)
            }
            "c_module_dirs" => {
                let Value::Table(list) = &value else {
                    return Err("'c_module_dirs' must be an array of directory paths".to_string());
                };
                let mut dirs = Vec::new();
                for item in list.clone().sequence_values::<Value>() {
                    let item = item.map_err(|e| e.to_string())?;
                    dirs.push(PathBuf::from(as_string(&item, "c_module_dirs")?));
                }
                config.c_module_dirs = Some(dirs);
            }
            "serve_dir" => {
                return Err(
                    "'serve_dir' cannot be set here — the config file is found *inside* the \
                     serve directory. Use --serve-dir or RC_SERVE_DIR."
                        .to_string(),
                );
            }
            other => {
                return Err(format!(
                    "unknown setting '{other}'. Valid settings: bind, workers, timeout_ms, \
                     memory_limit_mb, max_body_size, data_dir, max_upload_size, \
                     max_upload_files, static_files, index, dev, threads, \
                     thread_timeout_ms, thread_memory_limit_mb, c_module_dirs"
                ));
            }
        }
    }

    Ok(config)
}

fn as_string(value: &Value, key: &str) -> Result<String, String> {
    match value {
        Value::String(s) => Ok(s.to_string_lossy()),
        other => Err(format!(
            "'{key}' must be a string, got {}",
            other.type_name()
        )),
    }
}

fn as_bool(value: &Value, key: &str) -> Result<bool, String> {
    match value {
        Value::Boolean(b) => Ok(*b),
        other => Err(format!(
            "'{key}' must be true or false, got {}",
            other.type_name()
        )),
    }
}

/// Accepts integers, and floats that are whole numbers — Lua's `^` operator
/// produces floats, so `2^20` should still be a valid size.
fn as_f64_whole(value: &Value, key: &str) -> Result<f64, String> {
    let n = match value {
        Value::Integer(n) => *n as f64,
        Value::Number(n) => *n,
        other => {
            return Err(format!(
                "'{key}' must be a number, got {}",
                other.type_name()
            ));
        }
    };
    if !n.is_finite() || n.fract() != 0.0 {
        return Err(format!("'{key}' must be a whole number, got {n}"));
    }
    if n < 0.0 {
        return Err(format!("'{key}' must not be negative, got {n}"));
    }
    Ok(n)
}

fn as_usize(value: &Value, key: &str) -> Result<usize, String> {
    Ok(as_f64_whole(value, key)? as usize)
}

fn as_u64(value: &Value, key: &str) -> Result<u64, String> {
    Ok(as_f64_whole(value, key)? as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests run in parallel threads, so each call needs its own file.
    static NEXT_CONFIG_FILE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn file_config_from(source: &str) -> Result<FileConfig, String> {
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
        assert_eq!(
            config.threads.as_deref(),
            Some(&["jobs/a.lua".to_string(), "jobs/b.lua".to_string()][..])
        );
    }

    #[test]
    fn typos_are_rejected_rather_than_ignored() {
        let err = file_config_from("return { max_upload_sizes = 5 }").unwrap_err();
        assert!(err.contains("unknown setting"), "{err}");
        assert!(err.contains("max_upload_sizes"), "{err}");
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
            config.c_module_dirs.as_deref(),
            Some(&[PathBuf::from("/usr/lib/lua/5.4"), PathBuf::from("/opt/lua")][..])
        );
    }

    #[test]
    fn c_module_dirs_must_be_an_array() {
        assert!(file_config_from(r#"return { c_module_dirs = "/usr/lib" }"#).is_err());
        assert!(file_config_from(r#"return { c_module_dirs = true }"#).is_err());
    }

    #[test]
    fn c_modules_are_off_when_unset() {
        let resolved = Config::resolve(&CliArgs::default(), FileConfig::default());
        assert!(resolved.c_module_dirs.is_empty());
    }

    #[test]
    fn flags_win_over_the_file_which_wins_over_defaults() {
        let cli = CliArgs {
            bind: Some("1.2.3.4:80".to_string()),
            ..Default::default()
        };
        let file = FileConfig {
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
