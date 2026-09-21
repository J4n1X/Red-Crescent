//! Per-request Lua execution.
//!
//! Each request gets a fresh `Lua` instance, created and dropped inside the
//! blocking closure that calls [`render`] — nothing Lua-flavored crosses a
//! thread boundary, which is what lets us build without mlua's `send` feature
//! and gives perfect request isolation by construction.

use std::cell::{Cell, RefCell};
use std::fmt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mlua::{HookTriggers, Lua, Table, Value, VmState};

use crate::api;
use crate::template::{TemplateCache, TemplateError};
use crate::threads::ThreadRegistry;

#[derive(Clone, Copy)]
pub struct Limits {
    pub timeout: Duration,
    pub memory_bytes: usize,
}

/// Limits for a background thread, separate from the per-request [`Limits`].
///
/// Different threat model: a request comes from an anonymous stranger at any
/// rate and holds an HTTP worker; a thread is started by the operator's own
/// code and is capped at 64.
#[derive(Clone, Copy)]
pub struct ThreadLimits {
    /// Bounds one *awake stretch*; `sleep()` resets it. `None` (the default)
    /// means no deadline: a ten-minute computation and a spin loop look alike,
    /// and a runaway thread holds no connection.
    pub timeout: Option<Duration>,
    /// Applies for the instance's whole life, and defaulted higher than the
    /// request cap: a leaking long-lived thread would take the process with it.
    /// Hitting it kills only that thread, freeing its name for a respawn.
    pub memory_bytes: usize,
}

/// How often the timeout hook fires, in VM instructions. Low enough to catch
/// runaway loops quickly, high enough to keep overhead negligible.
const HOOK_INSTRUCTION_INTERVAL: u32 = 10_000;

/// Everything the blocking render closure needs; all fields are Send.
#[derive(Clone)]
pub struct RenderConfig {
    /// Canonicalized serve directory (include/require are confined to it).
    pub serve_dir: PathBuf,
    /// Canonicalized data directory (sqlite/send_file/uploads are confined to it).
    pub data_dir: PathBuf,
    pub cache: Arc<TemplateCache>,
    pub limits: Limits,
    /// Request-size limits, surfaced to Lua so apps can show them in forms.
    pub max_body_size: usize,
    /// Page output buffered in Lua before draining; JIT backends only.
    pub output_buffer_bytes: usize,
    pub max_upload_size: usize,
    pub max_upload_files: usize,
    /// Which named background threads are alive, process-wide.
    pub threads: Arc<ThreadRegistry>,
    /// Warm SQLite connections parked per database, per thread. 0 disables
    /// reuse, so every `sqlite.open` pays the full first-statement cost.
    pub sqlite_idle_connections: usize,
    /// Thread limits; `limits` above is the per-request budget.
    pub thread_limits: ThreadLimits,
    /// `package.cpath` for native modules, `None` when disabled (the default).
    /// `Some` also means the state must be built in unsafe mode — see `new_lua`.
    pub c_module_path: Option<Arc<str>>,
}

/// Build the Lua state for a request or a thread.
///
/// C modules need mlua's *unsafe* mode: a safe state stubs out
/// `package.loadlib` and the C searcher, so setting `cpath` alone does
/// nothing. `ALL_SAFE` is explicit — `unsafe_new()` would also load `debug`.
pub(crate) fn new_lua(cfg: &RenderConfig) -> Lua {
    if cfg.c_module_path.is_none() {
        return Lua::new();
    }
    // SAFETY: loading native code is inherently unsafe and mlua withdraws its
    // guarantees to match. The operator opted in by naming the directories.
    unsafe { Lua::unsafe_new_with(mlua::StdLib::ALL_SAFE, mlua::LuaOptions::default()) }
}

/// A file part of a multipart upload, already spooled to disk.
pub struct UploadedFile {
    /// The form field name the file arrived under.
    pub field: String,
    /// Client-supplied filename (display only — never used as a disk path).
    pub filename: String,
    pub content_type: Option<String>,
    pub size: u64,
    /// Spool location under `<data-dir>/.spool/`; Lua keeps the file by
    /// renaming it away, anything left here is deleted after the request.
    pub path: PathBuf,
}

pub enum RequestBody {
    Raw(Vec<u8>),
    Multipart {
        fields: Vec<(String, String)>,
        files: Vec<UploadedFile>,
    },
}

/// Plain-data snapshot of the HTTP request, built on the async side.
pub struct RequestData {
    pub method: String,
    pub path: String,
    pub query_string: String,
    /// Header names lowercased; repeated headers pre-joined with ", ".
    pub headers: Vec<(String, String)>,
    pub cookies: Vec<(String, String)>,
    pub content_type: Option<String>,
    pub body: RequestBody,
    /// Socket peer, IP only; `None` when the connection has no address.
    /// Strictly the socket peer — behind a proxy that is the proxy, and
    /// unwrapping `X-Forwarded-For` is the application's call, not ours.
    pub remote_addr: Option<String>,
}

/// A cookie queued from Lua via `response.set_cookie{...}`.
pub struct LuaCookie {
    pub name: String,
    pub value: String,
    pub path: Option<String>,
    pub domain: Option<String>,
    pub max_age: Option<i64>,
    pub http_only: bool,
    pub secure: bool,
    pub same_site: Option<String>,
}

/// A file the template asked to serve instead of its rendered output.
pub struct SendFileSpec {
    pub path: PathBuf,
    pub download_name: Option<String>,
    pub content_type: Option<String>,
}

pub struct RenderedResponse {
    pub body: Vec<u8>,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub cookies: Vec<LuaCookie>,
    pub send_file: Option<SendFileSpec>,
}

#[derive(Debug)]
pub enum RenderError {
    NotFound,
    Io(String),
    Parse {
        file: String,
        line: u32,
        msg: String,
    },
    Lua(String),
    Timeout,
    Memory,
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RenderError::NotFound => write!(f, "template not found"),
            RenderError::Io(e) => write!(f, "I/O error: {e}"),
            RenderError::Parse { file, line, msg } => {
                write!(f, "template parse error in {file}:{line}: {msg}")
            }
            RenderError::Lua(msg) => write!(f, "Lua error: {msg}"),
            RenderError::Timeout => write!(f, "template execution timed out"),
            RenderError::Memory => write!(f, "template exceeded the memory limit"),
        }
    }
}

/// State shared between the Rust-side API callbacks of one request.
pub(crate) struct RenderState {
    pub out: RefCell<Vec<u8>>,
    pub cookies: RefCell<Vec<LuaCookie>>,
    pub redirect: RefCell<Option<(String, u16)>>,
    pub send_file: RefCell<Option<SendFileSpec>>,
    pub timed_out: Cell<bool>,
    pub include_depth: Cell<u32>,
}

impl RenderState {
    fn new() -> Self {
        RenderState {
            out: RefCell::new(Vec::new()),
            cookies: RefCell::new(Vec::new()),
            redirect: RefCell::new(None),
            send_file: RefCell::new(None),
            timed_out: Cell::new(false),
            include_depth: Cell::new(0),
        }
    }
}

/// Raised by `exit()`/`redirect()` to unwind out of the template cleanly.
#[derive(Debug)]
pub(crate) struct ExitSignal;

impl fmt::Display for ExitSignal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "template requested early exit")
    }
}

impl std::error::Error for ExitSignal {}

/// Raised from the instruction hook once the deadline passes.
#[derive(Debug)]
struct TimeoutSignal;

impl fmt::Display for TimeoutSignal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "execution time limit exceeded")
    }
}

impl std::error::Error for TimeoutSignal {}

pub(crate) fn template_error_to_render_error(e: TemplateError, file: &str) -> RenderError {
    match e {
        TemplateError::NotFound => RenderError::NotFound,
        TemplateError::Io(msg) => RenderError::Io(msg),
        TemplateError::Parse { line, msg } => RenderError::Parse {
            file: file.to_string(),
            line,
            msg,
        },
    }
}

/// Render one template. Must run on a blocking thread — Lua executes
/// synchronously in here, up to the configured time limit.
pub fn render(
    cfg: &RenderConfig,
    template_abs: &Path,
    display_name: &str,
    request: &RequestData,
) -> Result<RenderedResponse, RenderError> {
    let template = cfg
        .cache
        .load(template_abs, display_name)
        .map_err(|e| template_error_to_render_error(e, display_name))?;

    let lua = new_lua(cfg);
    lua.set_memory_limit(cfg.limits.memory_bytes)
        .map_err(|e| RenderError::Lua(format!("failed to set memory limit: {e}")))?;

    let state = Rc::new(RenderState::new());
    // Points the raw `_out` at this request's buffer; restored on drop.
    let _out_scope = api::OutputScope::new(&state);

    let deadline = Instant::now() + cfg.limits.timeout;
    {
        let st = Rc::clone(&state);
        lua.set_global_hook(
            HookTriggers::new().every_nth_instruction(HOOK_INSTRUCTION_INTERVAL),
            move |_lua, _debug| {
                // Once expired, keep erroring on every trigger so a script-level
                // pcall can delay the abort by at most one hook interval.
                if st.timed_out.get() || Instant::now() >= deadline {
                    st.timed_out.set(true);
                    Err(mlua::Error::external(TimeoutSignal))
                } else {
                    Ok(VmState::Continue)
                }
            },
        )
        .map_err(|e| RenderError::Lua(format!("failed to install timeout hook: {e}")))?;
    }

    api::install(&lua, &state, cfg, request).map_err(|e| RenderError::Lua(e.to_string()))?;

    let exec_result = cfg
        .cache
        .chunk(&lua, &template)
        .and_then(|chunk| chunk.call::<()>(()));

    // On a JIT backend `_out` accumulates in Lua, so drain whatever is left
    // before the body is read. Runs whatever the outcome: exit() and
    // redirect() unwind through here and still keep their output.
    #[cfg(feature = "luajit")]
    if let Ok(flush) = lua.named_registry_value::<mlua::Function>(api::OUT_FLUSH_KEY) {
        let _ = flush.call::<()>(());
    }

    // The flag catches timeouts even when a script pcall swallowed the signal.
    if state.timed_out.get() {
        return Err(RenderError::Timeout);
    }

    if let Err(err) = exec_result {
        match classify_lua_error(&err) {
            LuaFailure::Exit => {} // clean early exit — fall through to the response
            LuaFailure::Timeout => return Err(RenderError::Timeout),
            LuaFailure::Memory => return Err(RenderError::Memory),
            LuaFailure::Error => return Err(RenderError::Lua(err.to_string())),
        }
    }

    let mut status: u16 = 200;
    let mut headers: Vec<(String, String)> = Vec::new();

    if let Ok(response_table) = lua.globals().get::<Table>("response") {
        match response_table.get::<Value>("status") {
            Ok(Value::Nil) => {}
            Ok(value) => match status_from_value(&value) {
                Some(n) => status = n,
                None => log::warn!(
                    "ignoring invalid response.status from Lua ({})",
                    value.type_name()
                ),
            },
            Err(_) => {}
        }

        if let Ok(headers_table) = response_table.get::<Table>("headers") {
            for pair in headers_table.pairs::<String, Value>() {
                let Ok((name, value)) = pair else {
                    log::warn!("ignoring response header with non-string name");
                    continue;
                };
                match header_value_to_string(&value) {
                    Some(v) => headers.push((name, v)),
                    None => log::warn!(
                        "ignoring response header '{name}' with unsupported value type {}",
                        value.type_name()
                    ),
                }
            }
        }
    }

    if let Some((url, redirect_status)) = state.redirect.borrow_mut().take() {
        status = redirect_status;
        headers.push(("Location".to_string(), url));
    }

    Ok(RenderedResponse {
        body: state.out.take(),
        status,
        headers,
        cookies: state.cookies.take(),
        send_file: state.send_file.borrow_mut().take(),
    })
}

/// Accept integers and numeric strings (PHP-style leniency) in the 100..=999
/// range that actix's `StatusCode` supports.
fn status_from_value(value: &Value) -> Option<u16> {
    let n = match value {
        Value::Integer(n) => *n,
        Value::String(s) => s.to_string_lossy().trim().parse::<i64>().ok()?,
        _ => return None,
    };
    (100..=999).contains(&n).then_some(n as u16)
}

fn header_value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.to_string_lossy()),
        Value::Integer(n) => Some(n.to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

enum LuaFailure {
    Exit,
    Timeout,
    Memory,
    Error,
}

/// Walk the error chain to find our control signals; every Rust→Lua boundary
/// wraps errors in another `CallbackError` layer, so a direct downcast on the
/// top-level error would miss them.
fn classify_lua_error(err: &mlua::Error) -> LuaFailure {
    if matches!(err, mlua::Error::MemoryError(_)) {
        return LuaFailure::Memory;
    }
    for cause in err.chain() {
        if cause.downcast_ref::<ExitSignal>().is_some() {
            return LuaFailure::Exit;
        }
        if cause.downcast_ref::<TimeoutSignal>().is_some() {
            return LuaFailure::Timeout;
        }
        if let Some(inner) = cause.downcast_ref::<mlua::Error>()
            && matches!(inner, mlua::Error::MemoryError(_))
        {
            return LuaFailure::Memory;
        }
    }
    LuaFailure::Error
}
