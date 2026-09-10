//! Pure Lua background threads.
//!
//! `thread.spawn(name, "script.lua", args)` runs a script on its own OS thread
//! in a fresh Lua instance; the script owns its loop via `sleep(seconds)`.
//! Spawning is idempotent by name, so a template can call it per request to
//! keep a worker alive.
//!
//! Threads use their own limits, and have no execution deadline by default: a
//! worker computing for ten minutes looks identical to a spin loop, and a
//! runaway thread holds no connection.
//!
//! Names are reusable, run ids are not. Status and results key off the run id
//! so a caller cannot read the outcome of a later run that took the same name.
//! The registry does not survive a restart; durable state belongs in SQLite.

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use mlua::{HookTriggers, LuaSerdeExt, MultiValue, VmState};

use crate::api;
use crate::runtime::RenderConfig;

const MAX_THREADS: usize = 64;
const MAX_SLEEP_SECS: f64 = 86400.0;
const MAX_NAME_LEN: usize = 64;

/// Finished runs kept before the oldest is dropped; bounds the registry.
pub const MAX_FINISHED_RUNS: usize = 256;

/// Ceiling on `thread.join`. A join blocks a worker and the timeout hook
/// cannot fire inside it, so it must not be able to wait forever.
pub const MAX_JOIN_SECS: f64 = 300.0;

/// Used when `thread.join` is called without an explicit timeout.
pub const DEFAULT_JOIN_SECS: f64 = 30.0;

/// How often a sleeping thread looks up to see whether it has been killed.
const KILL_CHECK_INTERVAL: Duration = Duration::from_millis(100);

/// Largest serialized `args` payload accepted by `thread.spawn`.
const MAX_ARGS_BYTES: usize = 1024 * 1024;

thread_local! {
    /// Lets `join` refuse a self-join instead of blocking until the cap.
    static CURRENT_RUN: Cell<Option<u64>> = const { Cell::new(None) };
}

/// The run id of the calling thread, or `None` on a request thread.
pub fn current_run_id() -> Option<u64> {
    CURRENT_RUN.with(|c| c.get())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RunStatus {
    Running,
    Finished,
    Failed,
    /// Ended by `thread.kill`; distinct from `Failed` — a stop is not a fault.
    Cancelled,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Running => "running",
            RunStatus::Finished => "finished",
            RunStatus::Failed => "failed",
            RunStatus::Cancelled => "cancelled",
        }
    }
}

/// One execution of a thread script.
pub struct Run {
    pub name: String,
    pub status: RunStatus,
    /// JSON of the return value. `None` if it returned nothing, or something
    /// unserializable — logged, not failed, since the script did succeed.
    pub result: Option<String>,
    /// Failure text, set only when `status` is [`RunStatus::Failed`].
    pub error: Option<String>,
    /// Raised by `thread.kill`; the hook and `sleep` watch it.
    cancel: Arc<AtomicBool>,
}

#[derive(Default)]
struct Inner {
    runs: HashMap<u64, Run>,
    /// Name -> the run holding it. Live runs only, so names free themselves.
    live: HashMap<String, u64>,
    /// Finished run ids in completion order, for oldest-first eviction.
    finished: VecDeque<u64>,
    next_id: u64,
}

/// Live threads and past runs, shared by every Lua instance in the process.
pub struct ThreadRegistry {
    inner: Mutex<Inner>,
    /// Signalled when a run leaves `Running`, so `join` waits instead of polling.
    done: Condvar,
}

impl Default for ThreadRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ThreadRegistry {
    pub fn new() -> Self {
        ThreadRegistry {
            inner: Mutex::new(Inner::default()),
            done: Condvar::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("thread registry poisoned")
    }

    pub fn is_running(&self, name: &str) -> bool {
        self.lock().live.contains_key(name)
    }

    /// Claim `name`, or report the run already holding it. `(id, false)` hands
    /// back the live run's id so a losing racer can watch what the winner started.
    fn claim(&self, name: &str) -> Result<(u64, bool), String> {
        let mut inner = self.lock();
        if let Some(&existing) = inner.live.get(name) {
            return Ok((existing, false));
        }
        if inner.live.len() >= MAX_THREADS {
            return Err(format!("thread limit ({MAX_THREADS}) reached"));
        }
        inner.next_id += 1;
        let id = inner.next_id;
        inner.runs.insert(
            id,
            Run {
                name: name.to_string(),
                status: RunStatus::Running,
                result: None,
                error: None,
                cancel: Arc::new(AtomicBool::new(false)),
            },
        );
        inner.live.insert(name.to_string(), id);
        Ok((id, true))
    }

    /// End a run and free its name. Idempotent, so the panic safety net cannot
    /// overwrite a real result.
    fn finish(&self, id: u64, status: RunStatus, result: Option<String>, error: Option<String>) {
        {
            let mut inner = self.lock();
            let Some(run) = inner.runs.get_mut(&id) else {
                return;
            };
            if run.status != RunStatus::Running {
                return;
            }
            run.status = status;
            run.result = result;
            run.error = error;
            let name = run.name.clone();

            // Only if this run still holds it; a later run may already own it.
            if inner.live.get(&name) == Some(&id) {
                inner.live.remove(&name);
            }

            inner.finished.push_back(id);
            while inner.finished.len() > MAX_FINISHED_RUNS {
                if let Some(old) = inner.finished.pop_front() {
                    inner.runs.remove(&old);
                }
            }
        }
        self.done.notify_all();
    }

    /// Undo a claim whose OS thread never started.
    fn abandon(&self, id: u64) {
        self.finish(
            id,
            RunStatus::Failed,
            None,
            Some("thread could not be started".to_string()),
        );
    }

    pub fn status(&self, id: u64) -> Option<RunStatus> {
        self.lock().runs.get(&id).map(|r| r.status)
    }

    /// The cancellation flag a run's own hook watches.
    fn cancel_flag(&self, id: u64) -> Option<Arc<AtomicBool>> {
        self.lock().runs.get(&id).map(|r| Arc::clone(&r.cancel))
    }

    /// Ask a run to stop; false if it is unknown or already over.
    ///
    /// Cooperative: an OS thread cannot be stopped from outside, so this raises
    /// a flag the hook and `sleep` check. A thread parked in `os.execute` will
    /// not notice until that call returns.
    pub fn request_kill(&self, id: u64) -> bool {
        let inner = self.lock();
        match inner.runs.get(&id) {
            Some(run) if run.status == RunStatus::Running => {
                run.cancel.store(true, Ordering::Relaxed);
                true
            }
            _ => false,
        }
    }

    /// Tells a cancellation apart from an error once the script has unwound.
    fn was_cancelled(&self, id: u64) -> bool {
        self.lock()
            .runs
            .get(&id)
            .is_some_and(|r| r.cancel.load(Ordering::Relaxed))
    }

    /// Block until run `id` leaves `Running`, or `timeout` elapses. `None` for
    /// an unknown or evicted id; a timeout still reports `Running`.
    pub fn join(
        &self,
        id: u64,
        timeout: Duration,
    ) -> Option<(RunStatus, Option<String>, Option<String>)> {
        let inner = self.lock();
        let (inner, _) = self
            .done
            .wait_timeout_while(inner, timeout, |inner| {
                inner.runs.get(&id).map(|r| r.status) == Some(RunStatus::Running)
            })
            .expect("thread registry poisoned");
        inner
            .runs
            .get(&id)
            .map(|r| (r.status, r.result.clone(), r.error.clone()))
    }
}

/// Resolve a thread script path: relative to the serve directory, must exist
/// and be a `.lua` file.
pub fn resolve_script(serve_dir: &Path, rel: &str) -> Result<PathBuf, String> {
    let path = serve_dir
        .join(rel.trim_start_matches('/'))
        .canonicalize()
        .map_err(|e| format!("thread script '{rel}': not found: {e}"))?;
    if !path.starts_with(serve_dir) {
        return Err(format!(
            "thread script '{rel}': escapes the serve directory"
        ));
    }
    if !path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("lua"))
    {
        return Err(format!("thread script '{rel}': must be a .lua file"));
    }
    Ok(path)
}

/// Marks the run failed if the thread unwound without finishing it.
struct ReleaseGuard {
    registry: Arc<ThreadRegistry>,
    id: u64,
}

impl Drop for ReleaseGuard {
    fn drop(&mut self) {
        self.registry.finish(
            self.id,
            RunStatus::Failed,
            None,
            Some("thread ended unexpectedly".to_string()),
        );
    }
}

/// Spawn `rel` under `name` unless that name is already running.
///
/// Returns `(run id, spawned)`. When `spawned` is false the id belongs to the
/// run already holding the name and **`args` are ignored** — check the flag.
pub fn spawn_named(
    cfg: RenderConfig,
    name: &str,
    rel: &str,
    args_json: Option<String>,
) -> Result<(u64, bool), String> {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return Err(format!("thread name must be 1..={MAX_NAME_LEN} characters"));
    }
    if let Some(args) = &args_json
        && args.len() > MAX_ARGS_BYTES
    {
        return Err(format!(
            "thread args are {} bytes, over the {MAX_ARGS_BYTES} byte limit",
            args.len()
        ));
    }
    let path = resolve_script(&cfg.serve_dir, rel)?;
    let (id, spawned) = cfg.threads.claim(name)?;
    if !spawned {
        return Ok((id, false));
    }

    let registry = Arc::clone(&cfg.threads);
    // Kept out of the closure so the error path below can still reach it.
    let registry_for_undo = Arc::clone(&cfg.threads);
    let cancel = cfg
        .threads
        .cancel_flag(id)
        .expect("run was just claimed, so it exists");
    let name = name.to_string();
    let display = rel.to_string();
    let spawned_thread = std::thread::Builder::new()
        .name(format!("lua:{name}"))
        .spawn(move || {
            CURRENT_RUN.with(|c| c.set(Some(id)));
            let guard = ReleaseGuard {
                registry: Arc::clone(&registry),
                id,
            };
            log::info!("thread '{name}' started ({display}, run {id})");
            match run_thread(&cfg, &display, &path, args_json, cancel) {
                Ok(result) => {
                    log::info!("thread '{name}' finished (run {id})");
                    registry.finish(id, RunStatus::Finished, result, None);
                }
                // A killed thread unwinds through the same error path as a
                // broken one; the flag is what separates them.
                Err(e) if registry.was_cancelled(id) => {
                    log::info!("thread '{name}' killed (run {id})");
                    registry.finish(id, RunStatus::Cancelled, None, Some(e));
                }
                Err(e) => {
                    log::error!("thread '{name}' died: {e} (run {id})");
                    registry.finish(id, RunStatus::Failed, None, Some(e));
                }
            }
            drop(guard);
        });

    match spawned_thread {
        Ok(_) => Ok((id, true)),
        Err(e) => {
            // The claim was made but no thread will release it; undo it here.
            registry_for_undo.abandon(id);
            Err(format!("failed to spawn thread: {e}"))
        }
    }
}

/// Build the thread's Lua instance and run the script, returning its
/// JSON-encoded return value if it produced a serializable one.
fn run_thread(
    cfg: &RenderConfig,
    display: &str,
    path: &Path,
    args_json: Option<String>,
    cancel: Arc<AtomicBool>,
) -> Result<Option<String>, String> {
    let source =
        std::fs::read_to_string(path).map_err(|e| format!("failed to read script: {e}"))?;

    let lua = crate::runtime::new_lua(cfg);
    lua.set_memory_limit(cfg.thread_limits.memory_bytes)
        .map_err(|e| format!("failed to set memory limit: {e}"))?;

    // The deadline is optional, the hook is not: it is also how a kill reaches
    // a running script. A pcall cannot swallow either for long, since both
    // conditions persist and the next firing errors again.
    let deadline = cfg
        .thread_limits
        .timeout
        .map(|t| Rc::new(Cell::new(Instant::now() + t)));
    {
        let deadline = deadline.clone();
        let cancel = Arc::clone(&cancel);
        lua.set_global_hook(
            HookTriggers::new().every_nth_instruction(10_000),
            move |_lua, _debug| {
                if cancel.load(Ordering::Relaxed) {
                    return Err(mlua::Error::runtime("thread was killed"));
                }
                if let Some(deadline) = &deadline
                    && Instant::now() >= deadline.get()
                {
                    return Err(mlua::Error::runtime(
                        "thread ran too long without sleeping (execution time limit)",
                    ));
                }
                Ok(VmState::Continue)
            },
        )
        .map_err(|e| format!("failed to install thread hook: {e}"))?;
    }

    api::install_core(&lua, cfg).map_err(|e| e.to_string())?;

    // Threads have no page to write to; print goes to the log instead.
    lua.globals()
        .set(
            "print",
            lua.create_function(|lua, params: MultiValue| {
                let message = api::params_to_string(lua, params)?;
                log::info!("[thread] {message}");
                Ok(())
            })
            .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;

    // Rebuilt from JSON: a Lua value belongs to the state that made it and
    // cannot cross into another one.
    if let Some(json) = args_json {
        let parsed: serde_json::Value =
            serde_json::from_str(&json).map_err(|e| format!("thread args are not valid: {e}"))?;
        let value = lua
            .to_value(&parsed)
            .map_err(|e| format!("thread args could not be rebuilt: {e}"))?;
        lua.globals()
            .set("args", value)
            .map_err(|e| format!("failed to set args: {e}"))?;
    }

    // Slices rather than one long sleep, so a kill lands promptly instead of
    // after the full nap.
    let timeout = cfg.thread_limits.timeout;
    let dl = deadline.clone();
    lua.globals()
        .set(
            "sleep",
            lua.create_function(move |_, seconds: f64| {
                if !seconds.is_finite() || !(0.0..=MAX_SLEEP_SECS).contains(&seconds) {
                    return Err(mlua::Error::runtime(format!(
                        "sleep: seconds must be in 0..={MAX_SLEEP_SECS}"
                    )));
                }
                let wake_at = Instant::now() + Duration::from_secs_f64(seconds);
                while Instant::now() < wake_at {
                    if cancel.load(Ordering::Relaxed) {
                        return Err(mlua::Error::runtime("thread was killed"));
                    }
                    let left = wake_at.saturating_duration_since(Instant::now());
                    std::thread::sleep(left.min(KILL_CHECK_INTERVAL));
                }
                if let (Some(dl), Some(timeout)) = (&dl, timeout) {
                    dl.set(Instant::now() + timeout);
                }
                Ok(())
            })
            .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;

    let returned: mlua::Value = lua
        .load(&source)
        .set_name(format!("@{display}"))
        .call(())
        .map_err(|e| e.to_string())?;

    if matches!(returned, mlua::Value::Nil) {
        return Ok(None);
    }
    // Unserializable is not a failed run: log it and leave the result empty.
    match lua.from_value::<serde_json::Value>(returned) {
        Ok(json) => Ok(serde_json::to_string(&json).ok()),
        Err(e) => {
            log::warn!("thread '{display}' returned a value that cannot be serialized: {e}");
            Ok(None)
        }
    }
}
