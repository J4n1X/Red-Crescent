//! Per-request Lua execution.
//!
//! States come from a thread-local pool, so nothing Lua-flavored crosses a
//! thread boundary and mlua's `send` feature stays off. Isolation comes from
//! the closed environment each request runs in, not from destroying the state
//! — see `api`. C modules can reach past it, so they turn pooling off.

use std::cell::{Cell, RefCell};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use mlua::{Lua, Table, Value};

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

/// Pool key: a state bakes its config's directories, cache and limits into its
/// closures. `new` is the only constructor and never repeats.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ConfigId(u64);

impl ConfigId {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        ConfigId(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// Everything the blocking render closure needs; all fields are Send.
#[derive(Clone)]
pub struct RenderConfig {
    pub id: ConfigId,
    /// Canonicalized serve directory (include/require are confined to it).
    pub serve_dir: PathBuf,
    /// Canonicalized data directory (sqlite/send_file/uploads are confined to it).
    pub data_dir: PathBuf,
    pub cache: Arc<TemplateCache>,
    pub limits: Limits,
    /// Request-size limits, surfaced to Lua so apps can show them in forms.
    pub max_body_size: usize,
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
    /// `Some` also means the state must be built in unsafe mode — see
    /// `new_lua` — and that pooling is off whatever `pool` says.
    pub c_module_path: Option<Arc<str>>,
    pub pool: bool,
    /// Requests one pooled state serves before retirement, in the style of
    /// php-fpm's `pm.max_requests`. A backstop; the heap stays flat on its own.
    pub pool_max_requests: u32,
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

/// State shared between the Rust-side API callbacks of one request. Reached
/// through a thread-local rather than captured, so the callbacks can be built
/// once per pooled state.
pub(crate) struct RenderState {
    pub out: RefCell<Vec<u8>>,
    pub cookies: RefCell<Vec<LuaCookie>>,
    pub redirect: RefCell<Option<(String, u16)>>,
    pub send_file: RefCell<Option<SendFileSpec>>,
    pub include_depth: Cell<u32>,
}

impl RenderState {
    fn new() -> Self {
        RenderState {
            out: RefCell::new(Vec::new()),
            cookies: RefCell::new(Vec::new()),
            redirect: RefCell::new(None),
            send_file: RefCell::new(None),
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

struct Pooled {
    lua: Lua,
    /// Address of `lua`'s state, handed to the watchdog each request.
    address: usize,
    config: ConfigId,
    requests: u32,
}

thread_local! {
    /// One, not a queue: a blocking thread renders one request at a time.
    static POOLED: RefCell<Option<Pooled>> = const { RefCell::new(None) };
}

/// Build a state and return it with the address the watchdog interrupts it by.
fn build_state(cfg: &RenderConfig) -> Result<(Lua, usize), RenderError> {
    let lua = new_lua(cfg);
    lua.set_memory_limit(cfg.limits.memory_bytes)
        .map_err(|e| RenderError::Lua(format!("failed to set memory limit: {e}")))?;

    // No hook is installed: Lua 5.4 traps every instruction once one is, which
    // profiled at 11.7% of a render, and a JIT trace never reaches one anyway.
    // The watchdog signals this thread to arm it, and only on expiry.
    lua.set_app_data(crate::template::StateChunks::default());
    api::install_request_api(&lua, cfg).map_err(|e| RenderError::Lua(e.to_string()))?;
    let address = api::state_address(&lua).map_err(|e| RenderError::Lua(e.to_string()))?;
    Ok((lua, address))
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

    // A native module can reach past the request environment, so it cannot
    // coexist with reuse. Opt-in, so the default path still pools.
    if !cfg.pool || cfg.c_module_path.is_some() {
        let (lua, address) = build_state(cfg)?;
        return render_on(&lua, address, cfg, &template, request);
    }

    let mut state = match POOLED.with(|p| p.borrow_mut().take()) {
        Some(pooled) if pooled.config == cfg.id => pooled,
        _ => {
            let (lua, address) = build_state(cfg)?;
            Pooled {
                lua,
                address,
                config: cfg.id,
                requests: 0,
            }
        }
    };
    state.requests += 1;
    // Left set by a state that was not pooled or not retained.
    api::take_finalizers();

    let result = render_on(&state.lua, state.address, cfg, &template, request);

    if let Some(state) = retain(state, cfg, &result) {
        POOLED.with(|p| *p.borrow_mut() = Some(state));
    }
    result
}

/// The state, if it may serve another request.
fn retain(
    state: Pooled,
    cfg: &RenderConfig,
    result: &Result<RenderedResponse, RenderError>,
) -> Option<Pooled> {
    // Heap already at the ceiling, and reclaiming it takes two collections.
    if matches!(result, Err(RenderError::Memory)) {
        return None;
    }
    if state.requests >= cfg.pool_max_requests {
        return None;
    }

    // First, so a replaced copy's contents are garbage by the collection.
    api::reset_request_env(&state.lua).ok()?;

    // Not heap growth but finalizer timing: destroying a state used to close
    // its sqlite connections and files, so a request ending mid-transaction
    // would hand the next one an open write lock, and a template's `__gc`
    // would run inside the next request. Only requests that made one pay.
    if api::take_finalizers() {
        state.lua.gc_collect().ok()?;
        // A finalizer that registered another would outlive this request.
        if api::take_finalizers() {
            return None;
        }
        api::reset_request_env(&state.lua).ok()?;
    }
    // `collectgarbage("stop")` must not outlive the request that called it.
    state.lua.gc_restart();
    // A pooled heap is mostly API tables that live as long as the state; a
    // minor collection skips them, an incremental cycle marks them all.
    #[cfg(feature = "lua54")]
    state.lua.gc_gen(0, 0);

    // Keeps a heavy request from leaving the next with no headroom.
    if state.lua.used_memory() > cfg.limits.memory_bytes / 2 {
        state.lua.gc_collect().ok()?;
        if state.lua.used_memory() > cfg.limits.memory_bytes / 2 {
            return None;
        }
    }
    Some(state)
}

fn render_on(
    lua: &Lua,
    address: usize,
    cfg: &RenderConfig,
    template: &Arc<crate::template::CompiledChunk>,
    request: &RequestData,
) -> Result<RenderedResponse, RenderError> {
    let state = RenderState::new();
    let _scope = api::RequestScope::new(&state);
    // Released on drop, so an unwind cannot leave a deadline armed against the
    // next request this thread serves.
    let deadline = crate::watchdog::Deadline::new(cfg.limits.timeout, address);

    let env = api::build_request_env(lua, request).map_err(|e| RenderError::Lua(e.to_string()))?;

    let exec_result = cfg
        .cache
        .chunk_for(lua, template, &env)
        .and_then(|chunk| chunk.call::<()>(()));

    // Catches a timeout even when a script pcall swallowed the signal: the
    // watchdog's flag outlives the raise.
    if deadline.slot().interrupted() {
        return Err(RenderError::Timeout);
    }

    if let Err(err) = exec_result {
        match classify_lua_error(&err) {
            LuaFailure::Exit => {} // clean early exit — fall through to the response
            LuaFailure::Memory => return Err(RenderError::Memory),
            LuaFailure::Error => return Err(RenderError::Lua(err.to_string())),
        }
    }

    let mut status: u16 = 200;
    let mut headers: Vec<(String, String)> = Vec::new();

    if let Ok(response_table) = env.get::<Table>("response") {
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
        if let Some(inner) = cause.downcast_ref::<mlua::Error>()
            && matches!(inner, mlua::Error::MemoryError(_))
        {
            return LuaFailure::Memory;
        }
    }
    LuaFailure::Error
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    fn config(dir: &Path) -> RenderConfig {
        let serve_dir = dir.canonicalize().unwrap();
        let data_dir = serve_dir.join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        RenderConfig {
            id: ConfigId::new(),
            serve_dir,
            data_dir,
            cache: Arc::new(TemplateCache::new(true)),
            limits: Limits {
                timeout: Duration::from_secs(5),
                memory_bytes: 32 * 1024 * 1024,
            },
            max_body_size: 4096,
            max_upload_size: 4096,
            max_upload_files: 4,
            threads: Arc::new(ThreadRegistry::new()),
            sqlite_idle_connections: 0,
            thread_limits: ThreadLimits {
                timeout: None,
                memory_bytes: 1024 * 1024,
            },
            c_module_path: None,
            pool: true,
            pool_max_requests: 10_000,
        }
    }

    fn request() -> RequestData {
        RequestData {
            method: "GET".to_string(),
            path: "/p.lhtml".to_string(),
            query_string: String::new(),
            headers: Vec::new(),
            cookies: Vec::new(),
            content_type: None,
            body: RequestBody::Raw(Vec::new()),
            remote_addr: None,
        }
    }

    fn body(cfg: &RenderConfig, page: &Path) -> String {
        let rendered = render(cfg, page, "p.lhtml", &request()).expect("render failed");
        String::from_utf8(rendered.body).unwrap()
    }

    /// Requests served by the state parked on this thread. Each test runs on
    /// its own thread, so this sees only its own.
    fn parked() -> Option<u32> {
        POOLED.with(|p| p.borrow().as_ref().map(|s| s.requests))
    }

    #[test]
    fn a_state_serves_request_after_request() {
        let dir = tempfile::tempdir().unwrap();
        let page = write(dir.path(), "p.lhtml", "ok");
        let cfg = config(dir.path());
        for _ in 0..3 {
            assert_eq!(body(&cfg, &page), "ok");
        }
        assert_eq!(parked(), Some(3), "state was not reused");
    }

    /// The attack the closed environment exists to stop: one request rewrites
    /// a library function, a global and a module's table; the next must see
    /// none of it, by either route into `string`.
    #[test]
    fn a_request_cannot_poison_the_next_one() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "m.lua", "return { n = 0 }");
        let page = write(
            dir.path(),
            "p.lhtml",
            r#"<?lua
local m = require("m")
print(("hello"):upper(), string.upper("x"), tostring(LEAKED), tostring(m.n))
m.n = m.n + 1
string.upper = function() return "POISONED" end
LEAKED = "leaked"
?>"#,
        );
        let cfg = config(dir.path());

        let first = body(&cfg, &page);
        assert_eq!(first.trim(), "HELLO X nil 0");
        assert_eq!(body(&cfg, &page).trim(), first.trim());
        assert_eq!(parked(), Some(2), "the test never exercised a reused state");
    }

    /// `getmetatable("").__index` is the real `string` table, which no copy
    /// can hide, so the function is not in the environment at all.
    #[test]
    fn the_string_metatable_is_out_of_reach() {
        let dir = tempfile::tempdir().unwrap();
        let page = write(
            dir.path(),
            "p.lhtml",
            "<?lua print(tostring(getmetatable), tostring(rawset)) ?>",
        );
        let cfg = config(dir.path());
        assert_eq!(body(&cfg, &page).trim(), "nil nil");
    }

    /// A chunk built by `load` gets the request's environment, not the shared
    /// globals behind it — otherwise it would be a way straight past the copy.
    #[test]
    fn a_loaded_chunk_stays_in_the_request_environment() {
        let dir = tempfile::tempdir().unwrap();
        let page = write(
            dir.path(),
            "p.lhtml",
            r#"<?lua
load("string.upper = function() return 'POISONED' end")()
print(string.upper("x"))
?>"#,
        );
        let cfg = config(dir.path());
        assert_eq!(body(&cfg, &page).trim(), "POISONED");
        assert_eq!(body(&cfg, &page).trim(), "POISONED", "the copy leaked");
    }

    /// The copies are reused, not rebuilt, while no request touches them.
    #[test]
    fn an_untouched_copy_is_reused() {
        let dir = tempfile::tempdir().unwrap();
        let page = write(dir.path(), "p.lhtml", "<?lua print(tostring(string)) ?>");
        let cfg = config(dir.path());
        assert_eq!(body(&cfg, &page), body(&cfg, &page));
    }

    /// A metatable changes no field, so the reset has to look for it.
    #[test]
    fn a_planted_metatable_does_not_survive_the_request() {
        let dir = tempfile::tempdir().unwrap();
        let page = write(
            dir.path(),
            "p.lhtml",
            r#"<?lua
print(tostring(string.planted))
setmetatable(string, { __index = function() return "PLANTED" end, __metatable = false })
?>"#,
        );
        let cfg = config(dir.path());
        assert_eq!(body(&cfg, &page).trim(), "nil");
        assert_eq!(body(&cfg, &page).trim(), "nil", "the metatable survived");
        assert_eq!(parked(), Some(2));
    }

    /// Every remaining entry still matches, so only the count gives it away.
    #[test]
    fn a_removed_entry_is_restored() {
        let dir = tempfile::tempdir().unwrap();
        let page = write(
            dir.path(),
            "p.lhtml",
            "<?lua print(type(string.upper)) string.upper = nil ?>",
        );
        let cfg = config(dir.path());
        assert_eq!(body(&cfg, &page).trim(), "function");
        assert_eq!(
            body(&cfg, &page).trim(),
            "function",
            "the entry stayed gone"
        );
    }

    /// Left to the incremental collector, the finalizer would run inside a
    /// later request and write into its response.
    #[test]
    fn a_finalizer_never_runs_in_a_later_request() {
        let dir = tempfile::tempdir().unwrap();
        let plant = write(
            dir.path(),
            "plant.lhtml",
            r#"<?lua setmetatable({}, { __gc = function() print("GHOST") end }) ?>ok"#,
        );
        let churn = write(
            dir.path(),
            "churn.lhtml",
            "<?lua for i = 1, 20000 do local t = { i } end ?>clean",
        );
        let cfg = config(dir.path());
        assert_eq!(body(&cfg, &plant), "ok");
        for _ in 0..3 {
            assert_eq!(body(&cfg, &churn), "clean");
        }
        assert_eq!(parked(), Some(4));
    }

    /// One that registers another would outlive the forced collection too.
    /// 5.1 has no table finalizers, so LuaJIT has nothing to retire.
    #[cfg(not(feature = "luajit"))]
    #[test]
    fn a_resurrecting_finalizer_retires_the_state() {
        let dir = tempfile::tempdir().unwrap();
        let page = write(
            dir.path(),
            "p.lhtml",
            r#"<?lua
local mt = {}
mt.__gc = function(o) setmetatable(o, mt) end
setmetatable({}, mt)
?>ok"#,
        );
        let cfg = config(dir.path());
        assert_eq!(body(&cfg, &page), "ok");
        assert_eq!(parked(), None, "a state with a live finalizer was parked");
    }

    #[test]
    fn a_stopped_collector_is_restarted() {
        let dir = tempfile::tempdir().unwrap();
        let page = write(
            dir.path(),
            "p.lhtml",
            r#"<?lua print(tostring(collectgarbage("isrunning"))) collectgarbage("stop") ?>"#,
        );
        let cfg = config(dir.path());
        assert_eq!(body(&cfg, &page).trim(), "true");
        assert_eq!(
            body(&cfg, &page).trim(),
            "true",
            "the collector stayed stopped"
        );
    }

    #[test]
    fn the_request_ceiling_retires_a_state() {
        let dir = tempfile::tempdir().unwrap();
        let page = write(dir.path(), "p.lhtml", "ok");
        let mut cfg = config(dir.path());
        cfg.pool_max_requests = 2;

        body(&cfg, &page);
        assert_eq!(parked(), Some(1));
        body(&cfg, &page);
        assert_eq!(parked(), None, "the ceiling did not retire the state");
    }

    #[test]
    fn exhausting_the_memory_limit_retires_a_state() {
        let dir = tempfile::tempdir().unwrap();
        let page = write(
            dir.path(),
            "p.lhtml",
            "<?lua local t = {} while true do t[#t + 1] = string.rep('x', 1024) end ?>",
        );
        let mut cfg = config(dir.path());
        cfg.limits.memory_bytes = 4 * 1024 * 1024;

        let Err(err) = render(&cfg, &page, "p.lhtml", &request()) else {
            panic!("the memory limit was not hit");
        };
        assert!(matches!(err, RenderError::Memory), "got {err:?}");
        assert_eq!(parked(), None, "a state at its ceiling was parked");

        // The next request gets a fresh one and is unaffected.
        let ok = write(dir.path(), "ok.lhtml", "fine");
        assert_eq!(body(&cfg, &ok), "fine");
    }

    #[test]
    fn pooling_can_be_turned_off() {
        let dir = tempfile::tempdir().unwrap();
        let page = write(dir.path(), "p.lhtml", "ok");
        let mut cfg = config(dir.path());
        cfg.pool = false;

        assert_eq!(body(&cfg, &page), "ok");
        assert_eq!(parked(), None);
    }

    /// A state bakes in its config, so one built for a different server must
    /// never be handed on.
    #[test]
    fn a_state_is_never_shared_between_configs() {
        let dir = tempfile::tempdir().unwrap();
        let page = write(dir.path(), "p.lhtml", "ok");
        let first = config(dir.path());
        let second = config(dir.path());

        body(&first, &page);
        assert_eq!(parked(), Some(1));
        body(&second, &page);
        assert_eq!(parked(), Some(1), "a state crossed configs");
    }
}
