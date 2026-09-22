//! Request execution deadlines, enforced from outside the VM.
//!
//! One thread owns the clock: rendering threads publish a deadline into a
//! [`Slot`], the watchdog marks it expired, and the VM asks its own slot. No
//! `Instant::now()` on the hot path.
//!
//! No hook is installed while a request runs: Lua 5.4 traps *every* instruction
//! once one is (11.7% of a render, profiled) and a JIT trace never reaches one.
//! On expiry the watchdog signals the rendering thread, whose handler arms
//! `lua_sethook(..., LUA_MASKCOUNT, 1)`.
//!
//! Signalling rather than arming from here is what makes it sound: the handler
//! runs *on the thread that owns the state*, so it never races that thread's
//! own `L->ci` updates. LuaJIT additionally needs `LUAJIT_ENABLE_CHECKHOOK`
//! for a trace to notice.

use std::cell::Cell;
use std::os::raw::c_int;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

/// Bounds the overshoot on a budget measured in seconds, for 100 wakeups a
/// second doing one atomic load per slot.
const TICK: Duration = Duration::from_millis(10);

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

fn now_micros() -> u64 {
    epoch().elapsed().as_micros() as u64
}

pub(crate) struct Slot {
    /// `lua_State` serving the current request, 0 when idle. An address, not
    /// a pointer, so the slot stays `Send`.
    state: AtomicUsize,
    /// Microseconds since [`epoch`], 0 when no request is running.
    deadline: AtomicU64,
    /// Bumped on every claim: the watchdog must not act on a deadline whose
    /// request has finished.
    generation: AtomicU64,
    /// `pthread_t` of the rendering thread, to signal it.
    thread: AtomicUsize,
    expired: AtomicBool,
    /// Set when the VM actually raised, which is what fails the request.
    /// Expiry alone does not: the deadline can pass inside a Rust callback the
    /// hook cannot interrupt, and discarding finished work helps nobody.
    interrupted: AtomicBool,
}

impl Slot {
    fn new() -> Self {
        Slot {
            state: AtomicUsize::new(0),
            deadline: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            thread: AtomicUsize::new(0),
            expired: AtomicBool::new(false),
            interrupted: AtomicBool::new(false),
        }
    }

    pub(crate) fn expired(&self) -> bool {
        self.expired.load(Ordering::Relaxed)
    }

    pub(crate) fn interrupted(&self) -> bool {
        self.interrupted.load(Ordering::Relaxed)
    }
}

static REGISTRY: RwLock<Vec<Arc<Slot>>> = RwLock::new(Vec::new());

thread_local! {
    /// Allocated once, never released: pool threads are long-lived and an idle
    /// slot costs one load per scan.
    static MY_SLOT: Arc<Slot> = register_slot();
    /// The state this thread is running, for the signal handler to arm. Const
    /// init and no destructor, so reading it from a handler is a TLS load.
    static HANDLER_STATE: Cell<usize> = const { Cell::new(0) };
}

fn register_slot() -> Arc<Slot> {
    let slot = Arc::new(Slot::new());
    REGISTRY
        .write()
        .expect("watchdog registry poisoned")
        .push(Arc::clone(&slot));
    start();
    slot
}

/// Claims this thread's slot. Released on drop, so an unwind cannot leave a
/// deadline armed.
pub(crate) struct Deadline {
    slot: Arc<Slot>,
}

impl Deadline {
    /// `state` addresses the `lua_State` about to run, which a JIT backend
    /// needs to be interrupted by.
    pub(crate) fn new(timeout: Duration, state: usize) -> Self {
        let slot = MY_SLOT.with(Arc::clone);
        slot.generation.fetch_add(1, Ordering::Relaxed);
        slot.expired.store(false, Ordering::Relaxed);
        slot.interrupted.store(false, Ordering::Relaxed);
        slot.state.store(state, Ordering::Relaxed);
        slot.thread
            .store(unsafe { libc::pthread_self() } as usize, Ordering::Relaxed);
        HANDLER_STATE.with(|s| s.set(state));
        // Last: a non-zero deadline is what makes the slot live.
        let deadline = now_micros().saturating_add(timeout.as_micros() as u64);
        slot.deadline.store(deadline.max(1), Ordering::Release);
        Deadline { slot }
    }

    pub(crate) fn slot(&self) -> &Arc<Slot> {
        &self.slot
    }
}

impl Drop for Deadline {
    fn drop(&mut self) {
        self.slot.deadline.store(0, Ordering::Release);
        self.slot.generation.fetch_add(1, Ordering::Relaxed);
        self.slot.state.store(0, Ordering::Relaxed);
        let _ = HANDLER_STATE.try_with(|s| s.set(0));
    }
}

/// Whether the calling hook should abort, recording that it did. Clones
/// nothing and leaves no `Drop` value live: a raise longjmps.
pub(crate) fn should_abort() -> bool {
    MY_SLOT
        .try_with(|slot| {
            if !slot.expired() {
                return false;
            }
            slot.interrupted.store(true, Ordering::Relaxed);
            true
        })
        .unwrap_or(false)
}

/// Chosen because nothing else in this process uses it: Rust's std and tokio
/// leave it alone, and Go picked it for preemption for the same reason.
const INTERRUPT_SIGNAL: c_int = libc::SIGURG;

/// Arms the timeout hook on the signalled thread.
extern "C" fn arm_hook(_sig: c_int) {
    let state = HANDLER_STATE.try_with(|s| s.get()).unwrap_or(0);
    if state == 0 {
        return;
    }
    // SAFETY: the address belongs to this thread's live state -- it is set
    // when a request claims the slot and cleared when it releases it, both on
    // this thread. A count of 1 makes the VM trap on its next instruction.
    unsafe {
        mlua::ffi::lua_sethook(
            state as *mut mlua::ffi::lua_State,
            Some(crate::api::timeout_hook),
            mlua::ffi::LUA_MASKCOUNT,
            1,
        );
    }
}

fn install_handler() {
    // SAFETY: `arm_hook` touches only a const-init thread-local and Lua's hook
    // fields; it allocates nothing and takes no lock.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = arm_hook as *const () as usize;
        action.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(INTERRUPT_SIGNAL, &action, std::ptr::null_mut());
    }
}

fn start() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        install_handler();
        std::thread::Builder::new()
            .name("rc-watchdog".to_string())
            .spawn(run)
            .expect("failed to start the execution watchdog");
    });
}

fn run() {
    loop {
        std::thread::sleep(TICK);
        let now = now_micros();
        let slots = REGISTRY.read().expect("watchdog registry poisoned");
        for slot in slots.iter() {
            let generation = slot.generation.load(Ordering::Acquire);
            let deadline = slot.deadline.load(Ordering::Acquire);
            if deadline == 0 || now < deadline {
                continue;
            }
            slot.expired.store(true, Ordering::Relaxed);
            interrupt(slot);
            // If the request ended while we decided, the flag belongs to
            // nobody: clear it rather than let the next one inherit it.
            if slot.generation.load(Ordering::Acquire) != generation {
                slot.expired.store(false, Ordering::Relaxed);
            }
        }
    }
}

/// Poke the rendering thread so its handler arms the hook. Repeated each tick
/// while the deadline stands, so a `pcall` around the raise cannot escape.
fn interrupt(slot: &Slot) {
    let thread = slot.thread.load(Ordering::Relaxed);
    if thread == 0 {
        return;
    }
    // SAFETY: a non-zero deadline means that thread is inside a render. A late
    // signal is harmless: the handler no-ops on a cleared state.
    unsafe { libc::pthread_kill(thread as libc::pthread_t, INTERRUPT_SIGNAL) };
}
