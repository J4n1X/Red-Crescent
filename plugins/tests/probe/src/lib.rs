//! A plugin that exercises every part of the ABI, through the SDK, for
//! tests/plugins.rs.

use std::cell::Cell;
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use red_crescent_plugin::ffi::{RcCall, RcStatus};
use red_crescent_plugin::prelude::*;

static KEPT: Mutex<Option<Retained<Function<'static>>>> = Mutex::new(None);
static DESTROYED: AtomicUsize = AtomicUsize::new(0);

/// A second handle type, sharing the method name `get` with Counter.
pub struct Label(String);

pub struct Counter {
    n: Cell<i64>,
}

impl Drop for Counter {
    fn drop(&mut self) {
        DESTROYED.fetch_add(1, Ordering::Relaxed);
    }
}

#[plugin]
mod probe {
    use super::*;

    struct Probe {
        options: &'static str,
    }

    #[install]
    fn install(ctx: &Context) -> Probe {
        Probe {
            options: ctx.options_json(),
        }
    }

    #[function]
    fn noop() {}

    #[function(blocking)]
    fn wait() {}

    /// Every argument back, unchanged.
    #[function]
    fn echo(values: Variadic<Value>) -> Variadic<Value> {
        values
    }

    #[function]
    fn fail(message: Option<String>) -> Result<()> {
        Err(Error::new(message.unwrap_or_else(|| "no message".into())))
    }

    #[function]
    fn panic() {
        panic!("on purpose");
    }

    /// Claims a failed host call that never happened.
    #[function(raw)]
    unsafe extern "C" fn bogus(_call: *mut RcCall, _ud: *mut c_void) -> RcStatus {
        RcStatus::RC_PROPAGATE
    }

    /// apply(fn, ...) -> fn(...)
    #[function]
    fn apply<'c>(f: Function<'c>, args: Variadic<Value<'c>>) -> Result<Variadic<Value<'c>>> {
        f.call(args)
    }

    /// each(t, fn): fn(k, v) for every pair, in `next` order.
    #[function]
    fn each(t: Table, f: Function) -> Result<()> {
        for pair in t.pairs::<Value, Value>() {
            f.call::<()>(pair?)?;
        }
        Ok(())
    }

    /// sum(t): t[1] + ... + t[#t].
    #[function]
    fn sum(t: Table) -> Result<f64> {
        t.sequence::<f64>().sum()
    }

    /// make(n) -> { 1, 2, ..., n }
    #[function]
    fn make(n: i64) -> Vec<i64> {
        (1..=n).collect()
    }

    #[function]
    fn write(call: &Call, bytes: &[u8]) -> Result<()> {
        call.write(bytes)
    }

    #[function]
    fn options(probe: &Probe) -> &'static str {
        probe.options
    }

    /// keep(fn): retains fn for call_kept(), which may come in a later call.
    #[function]
    fn keep(f: Function) -> Result<()> {
        *KEPT.lock().unwrap() = Some(f.retain()?);
        Ok(())
    }

    #[function]
    fn call_kept<'c>(call: &Call<'c>) -> Result<Value<'c>> {
        let Some(kept) = *KEPT.lock().unwrap() else {
            bail!("nothing kept");
        };
        kept.get(call).call(())
    }

    /// counter(start) -> a Counter handle.
    #[function]
    fn counter(start: Option<i64>) -> Counter {
        Counter {
            n: Cell::new(start.unwrap_or(0)),
        }
    }

    #[handle]
    impl Counter {
        fn inc(&self) {
            self.n.set(self.n.get() + 1);
        }

        fn get(&self) -> i64 {
            self.n.get()
        }
    }

    #[function]
    fn label(text: String) -> Label {
        Label(text)
    }

    #[handle]
    impl Label {
        fn get(&self) -> &str {
            &self.0
        }
    }

    #[function]
    fn destroyed() -> usize {
        DESTROYED.load(Ordering::Relaxed)
    }
}
