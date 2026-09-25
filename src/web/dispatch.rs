//! Where a template renders. Handing a render to the blocking pool costs
//! ~25us of CPU and two thread switches, more than a small page takes; running
//! on the worker stalls its other connections for as long as the render runs.
//! So templates with a record of fast renders run inline, and one slow render
//! sends a template back.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::Metadata;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use actix_web::web;

use crate::runtime::{RenderConfig, RenderError, RenderedResponse, RequestData, render};

/// Renders `abs` inline or on the blocking pool, and records how it went.
/// `Err` is a render that panicked or never ran.
pub(super) async fn render_dispatched(
    cfg: &Arc<RenderConfig>,
    abs: PathBuf,
    meta: Option<Metadata>,
    display_name: String,
    request: RequestData,
    budget: Duration,
) -> Result<Result<RenderedResponse, RenderError>, String> {
    let outcome = if !budget.is_zero() && runs_inline(&abs) {
        std::panic::catch_unwind(AssertUnwindSafe(|| {
            timed_render(cfg, &abs, meta.as_ref(), &display_name, &request, budget)
        }))
        .map_err(|_| "render panicked".to_string())
    } else {
        let cfg = Arc::clone(cfg);
        let path = abs.clone();
        web::block(move || {
            timed_render(&cfg, &path, meta.as_ref(), &display_name, &request, budget)
        })
        .await
        .map_err(|e| e.to_string())
    };

    if !budget.is_zero() {
        match &outcome {
            Ok(timed) => record_render(abs, timed.cost, timed.blocked, budget),
            Err(_) => record_render(abs, Duration::MAX, true, budget),
        }
    }
    outcome.map(|timed| timed.result)
}

/// A render and what dispatch needs to know about it.
struct Timed {
    result: Result<RenderedResponse, RenderError>,
    /// How long it held its thread: wall time if it waited on anything, CPU
    /// time otherwise, so being preempted on a busy box is not held against it.
    cost: Duration,
    /// Called something that can wait for seconds: `thread.join`, `process.run`.
    blocked: bool,
}

fn timed_render(
    cfg: &RenderConfig,
    abs: &Path,
    meta: Option<&std::fs::Metadata>,
    display_name: &str,
    request: &RequestData,
    budget: Duration,
) -> Timed {
    crate::api::take_blocking();
    let started = Instant::now();
    let usage = (!budget.is_zero()).then(ThreadUsage::now);
    let result = render(cfg, abs, meta, display_name, request);
    let wall = started.elapsed();
    // Within budget by the wall clock is within it by any measure.
    let cost = match usage {
        Some(usage) if wall > budget => usage.cost_since(wall),
        _ => wall,
    };
    Timed {
        result,
        cost,
        blocked: crate::api::take_blocking(),
    }
}

/// CPU time and voluntary context switches of the calling thread.
#[cfg(target_os = "linux")]
struct ThreadUsage {
    cpu: Duration,
    waits: i64,
}

#[cfg(target_os = "linux")]
impl ThreadUsage {
    fn now() -> Self {
        // getrusage's CPU times are tick-derived and jump by milliseconds, so
        // only the switch count comes from it.
        // SAFETY: both calls only write the struct they are handed.
        let (ru, ts) = unsafe {
            let mut ru: libc::rusage = std::mem::zeroed();
            libc::getrusage(libc::RUSAGE_THREAD, &mut ru);
            let mut ts: libc::timespec = std::mem::zeroed();
            libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts);
            (ru, ts)
        };
        ThreadUsage {
            cpu: Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32),
            waits: ru.ru_nvcsw,
        }
    }

    fn cost_since(&self, wall: Duration) -> Duration {
        let now = ThreadUsage::now();
        if now.waits > self.waits {
            wall
        } else {
            now.cpu.saturating_sub(self.cpu)
        }
    }
}

/// No per-thread accounting to separate waiting from preemption: wall time.
#[cfg(not(target_os = "linux"))]
struct ThreadUsage;

#[cfg(not(target_os = "linux"))]
impl ThreadUsage {
    fn now() -> Self {
        ThreadUsage
    }

    fn cost_since(&self, wall: Duration) -> Duration {
        wall
    }
}

/// Fast renders needed before a template leaves the pool, doubled per demotion.
const PROMOTE_AFTER: u32 = 16;
const MAX_BACKOFF: u32 = 12;

/// One template's dispatch record.
#[derive(Default)]
struct Dispatch {
    inline: bool,
    streak: u32,
    demotions: u32,
    /// Made a blocking call once, so it may again, for longer than any budget.
    pinned: bool,
}

impl Dispatch {
    /// Whether this render sent the template back to the pool.
    fn record(&mut self, cost: Duration, blocked: bool, budget: Duration) -> bool {
        if blocked {
            self.pinned = true;
        }
        if self.pinned || cost > budget {
            let demoted = self.inline;
            if demoted {
                self.demotions = (self.demotions + 1).min(MAX_BACKOFF);
            }
            self.inline = false;
            self.streak = 0;
            return demoted;
        }
        if !self.inline {
            self.streak += 1;
            self.inline = self.streak >= PROMOTE_AFTER << self.demotions;
        }
        false
    }
}

thread_local! {
    /// Per HTTP worker, which is a single thread, so no lock. Each learns alone.
    static DISPATCH: RefCell<HashMap<PathBuf, Dispatch>> = RefCell::new(HashMap::new());
}

fn runs_inline(abs: &Path) -> bool {
    DISPATCH.with(|d| d.borrow().get(abs).is_some_and(|t| t.inline))
}

fn record_render(abs: PathBuf, cost: Duration, blocked: bool, budget: Duration) {
    let demoted = DISPATCH.with(|d| {
        let mut d = d.borrow_mut();
        match d.get_mut(&abs) {
            Some(t) => t.record(cost, blocked, budget),
            None => d
                .entry(abs.clone())
                .or_default()
                .record(cost, blocked, budget),
        }
    });
    if demoted {
        log::debug!(
            "{}: back to the blocking pool after {cost:?}{}",
            abs.display(),
            if blocked { " (blocking call)" } else { "" }
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUDGET: Duration = Duration::from_millis(1);
    const FAST: Duration = Duration::from_micros(100);
    const SLOW: Duration = Duration::from_millis(5);

    fn promoted(d: &mut Dispatch) -> u32 {
        let mut renders = 0;
        while !d.inline {
            d.record(FAST, false, BUDGET);
            renders += 1;
            assert!(renders < 1 << 20, "never promoted");
        }
        renders
    }

    #[test]
    fn a_run_of_fast_renders_promotes() {
        let mut d = Dispatch::default();
        assert!(!d.inline, "a template starts on the pool");
        assert_eq!(promoted(&mut d), PROMOTE_AFTER);
    }

    #[test]
    fn a_slow_render_demotes_and_backs_off() {
        let mut d = Dispatch::default();
        promoted(&mut d);
        d.record(SLOW, false, BUDGET);
        assert!(!d.inline);
        assert_eq!(promoted(&mut d), PROMOTE_AFTER * 2);
        d.record(SLOW, false, BUDGET);
        assert_eq!(promoted(&mut d), PROMOTE_AFTER * 4);
    }

    /// Nothing was stalled, so there is nothing to back off from.
    #[test]
    fn a_slow_render_on_the_pool_only_restarts_the_run() {
        let mut d = Dispatch::default();
        for _ in 0..PROMOTE_AFTER - 1 {
            d.record(FAST, false, BUDGET);
        }
        d.record(SLOW, false, BUDGET);
        assert_eq!(promoted(&mut d), PROMOTE_AFTER);
    }

    /// Idle CPU-wise, but it held the thread: that is what stalls a worker.
    #[test]
    fn waiting_is_charged_as_wall_time() {
        let usage = ThreadUsage::now();
        let started = Instant::now();
        std::thread::sleep(Duration::from_millis(20));
        assert!(usage.cost_since(started.elapsed()) >= Duration::from_millis(20));
    }

    #[test]
    fn a_blocking_call_pins_to_the_pool() {
        let mut d = Dispatch::default();
        promoted(&mut d);
        d.record(FAST, true, BUDGET);
        for _ in 0..1 << 20 {
            d.record(FAST, false, BUDGET);
        }
        assert!(!d.inline);
    }
}
