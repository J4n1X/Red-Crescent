//! What `#[plugin]` expands to. Not a stable interface.

use std::ffi::{CStr, c_int, c_void};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicPtr, Ordering};

pub use crate::call::Call;
pub use crate::context::Context;
pub use crate::convert::{FromLua, IntoLua, IntoLuaMulti, Lowered};
pub use crate::error::{Error, Result};
pub use crate::ffi::{
    RC_FN_BLOCKING, RC_PLUGIN_ABI, RcCall, RcFunction, RcPluginContext, RcRegistryCOM, RcStatus,
};
pub use crate::handle::Handle;
pub use crate::value::{Value, Variadic};

use crate::convert::mismatch;
use crate::ffi::{RcHandleType, RcHost, RcMethod, RcValue};
use crate::handle::destroy;

/// Runs one plugin function: `f` gets the call, and its error or panic
/// becomes the status the host expects. Nothing unwinds into the host.
///
/// # Safety
/// `raw` is the call the host passed to the entry point.
pub unsafe fn run(raw: *mut RcCall, f: impl for<'c> FnOnce(&Call<'c>) -> Result<()>) -> RcStatus {
    // SAFETY: the host passed `raw` for this call; `'c` cannot outlive `f`.
    let call = unsafe { Call::from_raw(raw) };
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| f(&call)))
        .unwrap_or_else(|panic| Err(Error::new(format!("panicked: {}", panic_message(&panic)))));
    match result {
        Ok(()) => RcStatus::RC_OK,
        Err(e) => match e.into_parts() {
            (Some(msg), status) => {
                unsafe { (call.host().error)(raw, msg.as_ptr().cast(), msg.len()) };
                status
            }
            (None, status) => status,
        },
    }
}

fn panic_message(panic: &Box<dyn std::any::Any + Send>) -> &str {
    if let Some(s) = panic.downcast_ref::<&str>() {
        s
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s
    } else {
        "(no message)"
    }
}

/// The arguments of one call, read left to right, numbered as Lua sees them.
pub struct Args<'c> {
    call: Call<'c>,
    next: usize,
    /// 1 for a method, whose `self` is not counted.
    offset: usize,
}

impl<'c> Args<'c> {
    pub fn function(call: &Call<'c>) -> Self {
        Args {
            call: *call,
            next: 0,
            offset: 0,
        }
    }

    pub fn method(call: &Call<'c>) -> Self {
        Args {
            call: *call,
            next: 1,
            offset: 1,
        }
    }

    pub fn this<T: Handle>(&self) -> Result<&'c T> {
        handle_from_lua(self.call.arg(0)).map_err(|e| e.context("bad self (call methods with ':')"))
    }

    #[allow(clippy::should_implement_trait)]
    pub fn next<T: FromArgs<'c>>(&mut self) -> Result<T> {
        T::from_args(self)
    }

    fn take(&mut self) -> (usize, Value<'c>) {
        let i = self.next;
        self.next += 1;
        (i + 1 - self.offset, self.call.arg(i))
    }
}

/// One parameter's worth of arguments: one, or for [`Variadic`] the rest.
pub trait FromArgs<'c>: Sized {
    fn from_args(args: &mut Args<'c>) -> Result<Self>;
}

impl<'c, T: FromLua<'c>> FromArgs<'c> for T {
    fn from_args(args: &mut Args<'c>) -> Result<Self> {
        let (n, value) = args.take();
        T::from_lua(value).map_err(|e| e.context(format_args!("argument {n}")))
    }
}

impl<'c, T: FromLua<'c>> FromArgs<'c> for Variadic<T> {
    fn from_args(args: &mut Args<'c>) -> Result<Self> {
        let mut out = Vec::new();
        while args.next < args.call.nargs() {
            out.push(T::from_args(args)?);
        }
        Ok(Variadic(out))
    }
}

pub fn handle_from_lua<'c, T: Handle>(value: Value<'c>) -> Result<&'c T> {
    let name = T::NAME.to_str().unwrap_or("handle");
    match value {
        Value::Handle(h) => h
            .downcast::<T>()
            .ok_or_else(|| Error::new(format!("expected {name}, got another type of handle"))),
        other => Err(mismatch(name, &other)),
    }
}

/// What a plugin function may return: values, or a `Result` of them.
pub trait IntoReturn {
    fn push_results(self, call: &Call<'_>) -> Result<()>;
}

impl<T: IntoLuaMulti> IntoReturn for T {
    fn push_results(self, call: &Call<'_>) -> Result<()> {
        let mut values = Vec::new();
        self.into_lua_multi(call, &mut values)?;
        values.into_iter().try_for_each(|v| call.push(v))
    }
}

impl<T: IntoLuaMulti, E: Into<Error>> IntoReturn for std::result::Result<T, E> {
    fn push_results(self, call: &Call<'_>) -> Result<()> {
        self.map_err(Into::into)?.push_results(call)
    }
}

pub fn ret(call: &Call<'_>, value: impl IntoReturn) -> Result<()> {
    value.push_results(call)
}

/// An `RcMethod` a static array can hold.
#[repr(transparent)]
pub struct Method(RcMethod);

// SAFETY: a name pointing at a literal and a function pointer.
unsafe impl Sync for Method {}

impl Method {
    pub const fn new(name: &'static CStr, f: RcFunction, flags: u32) -> Self {
        Method(RcMethod {
            name_cstr: name.as_ptr(),
            fn_: f,
            flags,
        })
    }
}

/// The registry, during install.
pub struct Registry {
    raw: *mut RcRegistryCOM,
}

impl Registry {
    /// # Safety
    /// `userdata` is what `f` expects.
    pub unsafe fn function(
        &mut self,
        name: &'static CStr,
        f: RcFunction,
        flags: u32,
        userdata: Userdata,
    ) -> Result<()> {
        let r = unsafe { &*self.raw };
        let status = unsafe { (r.function)(self.raw, name.as_ptr(), f, userdata, flags) };
        registered(status, name)
    }

    pub fn constant(&mut self, name: &'static CStr, value: impl IntoConstant) -> Result<()> {
        let value = value
            .into_constant()
            .map_err(|e| e.context(format_args!("constant {name:?}")))?;
        let r = unsafe { &*self.raw };
        registered(
            unsafe { (r.constant)(self.raw, name.as_ptr(), &value) },
            name,
        )
    }

    /// # Safety
    /// `userdata` is what the methods expect.
    pub unsafe fn handle<T: Handle>(
        &mut self,
        methods: &'static [Method],
        userdata: Userdata,
    ) -> Result<()> {
        let spec = RcHandleType {
            name_cstr: T::NAME.as_ptr(),
            methods: methods.as_ptr().cast(),
            nmethods: methods.len(),
            destroy: Some(destroy::<T>),
            userdata,
        };
        let mut id = u32::MAX;
        let r = unsafe { &*self.raw };
        registered(
            unsafe { (r.handle_type)(self.raw, &spec, &mut id) },
            T::NAME,
        )?;
        T::type_slot().store(id, Ordering::Relaxed);
        Ok(())
    }
}

/// What `#[constant]` accepts.
pub trait IntoConstant {
    fn into_constant(self) -> Result<RcValue>;
}

impl IntoConstant for Value<'static> {
    fn into_constant(self) -> Result<RcValue> {
        match self {
            Value::Table(_) | Value::Function(_) | Value::Handle(_) | Value::Other(_) => Err(
                Error::new(format!("a {} cannot be a constant", self.type_name())),
            ),
            scalar => Ok(scalar.to_raw()),
        }
    }
}

impl IntoConstant for &'static str {
    fn into_constant(self) -> Result<RcValue> {
        Ok(RcValue::string(self))
    }
}

impl IntoConstant for bool {
    fn into_constant(self) -> Result<RcValue> {
        Ok(RcValue::boolean(self))
    }
}

impl IntoConstant for i64 {
    fn into_constant(self) -> Result<RcValue> {
        Ok(RcValue::int(self))
    }
}

impl IntoConstant for f64 {
    fn into_constant(self) -> Result<RcValue> {
        Ok(RcValue::number(self))
    }
}

fn registered(status: RcStatus, name: &CStr) -> Result<()> {
    if status == RcStatus::RC_OK {
        Ok(())
    } else {
        Err(Error::new(format!("registering {name:?} was refused")))
    }
}

/// `rc_plugin_install`: sets up logging, then runs `f`. A failure is logged,
/// since install has no other way to say why.
///
/// # Safety
/// The pointers are the ones the host passed to `rc_plugin_install`.
pub unsafe fn install(
    ctx: *const RcPluginContext,
    reg: *mut RcRegistryCOM,
    f: impl FnOnce(&Context, &mut Registry) -> Result<()>,
) -> RcStatus {
    let raw = unsafe { &*ctx };
    HOST.store(raw.host as *mut RcHost, Ordering::Relaxed);
    if log::set_logger(&HostLogger).is_ok() {
        log::set_max_level(log::LevelFilter::Debug);
    }
    let context = unsafe { Context::from_raw(raw) };
    let mut registry = Registry { raw: reg };
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| f(&context, &mut registry)))
        .unwrap_or_else(|panic| Err(Error::new(format!("panicked: {}", panic_message(&panic)))));
    match result {
        Ok(()) => RcStatus::RC_OK,
        Err(e) => {
            log::error!("install of plugin '{}' failed: {e}", context.name());
            RcStatus::RC_ERROR
        }
    }
}

static HOST: AtomicPtr<RcHost> = AtomicPtr::new(std::ptr::null_mut());

/// Sends the `log` crate's records to the server's log.
struct HostLogger;

impl log::Log for HostLogger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        let host = HOST.load(Ordering::Relaxed);
        if host.is_null() {
            return;
        }
        let level: c_int = match record.level() {
            log::Level::Error => 3,
            log::Level::Warn => 2,
            log::Level::Info => 1,
            log::Level::Debug | log::Level::Trace => 0,
        };
        let msg = record.args().to_string();
        unsafe { ((*host).log)(level, msg.as_ptr().cast(), msg.len()) };
    }

    fn flush(&self) {}
}

/// The plugin's state, leaked at install and handed to every function and
/// method as their userdata. Per install, not a static: a process that loads
/// the library twice (the tests do) gets the same globals but two states.
pub type Userdata = *mut c_void;

pub fn leak_state<S: Send + Sync + 'static>(state: S) -> Userdata {
    Box::into_raw(Box::new(state)).cast()
}

/// # Safety
/// `userdata` came from `leak_state::<S>`.
pub unsafe fn state<'s, S>(userdata: Userdata) -> &'s S {
    unsafe { &*(userdata as *const S) }
}
