//! One plugin call: arguments out, results back, and the host functions a
//! plugin calls in between.
//!
//! Nothing here lets a Lua error unwind through a plugin's frames. Every host
//! function that touches Lua goes through mlua's protected API and reports
//! failure as a status; the error itself waits in the call's state and is
//! raised by the wrapper once the plugin has returned.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};

use mlua::{AnyUserData, Lua, MetaMethod, MultiValue, Table, UserData, UserDataMethods, Value};
use red_crescent_plugin::ffi::{
    RC_FN_BLOCKING, RcCall, RcHandleRef, RcHost, RcRef, RcStatus, RcType, RcValue, RcValueData,
};

use super::{Function, Plugins};

const METHODS_KEY: &str = "rc_plugin_methods";
const NEXT_KEY: &str = "rc_plugin_next";

/// Bits of a reference id that name its slot; the rest name its call.
const SLOT_BITS: u32 = 24;
const SLOT_MASK: u64 = (1 << SLOT_BITS) - 1;

/// Unique per call, process-wide, so a reference from one call is refused by
/// every other, on any thread.
static NEXT_SERIAL: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// References a plugin retained, until the request ends.
    static RETAINED: RefCell<HashMap<u64, Value>> = RefCell::new(HashMap::new());
}

/// Drops every retained reference: a pooled state serves the next request
/// next, and nothing from this one may be reachable from it.
pub(crate) fn end_request() {
    let _ = RETAINED.try_with(|r| r.borrow_mut().clear());
}

/// Per-call state, behind `RcCall::internal`.
struct CallState<'a> {
    lua: &'a Lua,
    plugins: &'static Plugins,
    serial: u64,
    /// Values references point at, and values whose string bytes a plugin
    /// was lent; both live until the call returns.
    slots: Vec<Value>,
    /// Handles seen in this call, so one can be passed back.
    handles: Vec<AnyUserData>,
    results: Vec<Value>,
    /// Set by `host.error`.
    message: Option<String>,
    /// A failed host call, re-raised on RC_PROPAGATE.
    error: Option<mlua::Error>,
}

impl<'a> CallState<'a> {
    fn new(lua: &'a Lua, plugins: &'static Plugins) -> Self {
        CallState {
            lua,
            plugins,
            serial: NEXT_SERIAL.fetch_add(1, Ordering::Relaxed),
            slots: Vec::new(),
            handles: Vec::new(),
            results: Vec::new(),
            message: None,
            error: None,
        }
    }

    fn slot(&mut self, value: Value) -> Result<RcRef, String> {
        let index = self.slots.len() as u64;
        if index > SLOT_MASK {
            return Err("too many references in one call".into());
        }
        self.slots.push(value);
        Ok(RcRef {
            id: (self.serial << SLOT_BITS) | index,
        })
    }

    fn deref(&self, r: RcRef) -> Result<Value, String> {
        if r.id >> SLOT_BITS == self.serial
            && let Some(v) = self.slots.get((r.id & SLOT_MASK) as usize)
        {
            return Ok(v.clone());
        }
        RETAINED
            .with(|m| m.borrow().get(&r.id).cloned())
            .ok_or_else(|| "a stale or foreign reference".to_string())
    }

    /// `keep` holds on to a string the plugin is lent that nothing else keeps
    /// alive; arguments are already held by the wrapper.
    fn rc_from(&mut self, value: &Value, keep: bool) -> Result<RcValue, String> {
        Ok(match value {
            Value::Nil => RcValue::nil(),
            Value::Boolean(b) => RcValue::boolean(*b),
            Value::Integer(i) => RcValue::int(*i),
            Value::Number(n) => RcValue::number(*n),
            Value::String(s) => {
                // Lua never moves a string, so the bytes stay put for as long
                // as the string lives.
                let v = RcValue::bytes(&s.as_bytes());
                if keep {
                    self.slots.push(value.clone());
                }
                v
            }
            Value::Table(_) => RcValue::reference(RcType::RC_TABLE, self.slot(value.clone())?),
            Value::Function(_) => {
                RcValue::reference(RcType::RC_FUNCTION, self.slot(value.clone())?)
            }
            Value::UserData(ud) => match ud.borrow::<Handle>() {
                Ok(h) => {
                    let v = RcValue {
                        r#type: RcType::RC_HANDLE,
                        as_: RcValueData {
                            handle: RcHandleRef {
                                type_id: h.type_id,
                                ptr: h.ptr,
                            },
                        },
                    };
                    drop(h);
                    self.handles.push(ud.clone());
                    v
                }
                Err(_) => RcValue::reference(RcType::RC_OTHER, self.slot(value.clone())?),
            },
            other => RcValue::reference(RcType::RC_OTHER, self.slot(other.clone())?),
        })
    }

    fn lua_from(&self, value: &RcValue) -> Result<Value, String> {
        // SAFETY (the union reads): each arm reads the member its tag names.
        Ok(match value.r#type {
            RcType::RC_NIL => Value::Nil,
            RcType::RC_BOOL => Value::Boolean(unsafe { value.as_.b } != 0),
            RcType::RC_INT => Value::Integer(unsafe { value.as_.i } as _),
            RcType::RC_NUMBER => Value::Number(unsafe { value.as_.n }),
            RcType::RC_STRING => {
                let s = unsafe { value.as_.s };
                let bytes: &[u8] = if s.len == 0 {
                    &[]
                } else if s.ptr.is_null() {
                    return Err("a string with a null pointer".into());
                } else {
                    unsafe { std::slice::from_raw_parts(s.ptr.cast(), s.len) }
                };
                Value::String(self.lua.create_string(bytes).map_err(|e| e.to_string())?)
            }
            RcType::RC_TABLE | RcType::RC_FUNCTION | RcType::RC_OTHER => {
                self.deref(unsafe { value.as_.r#ref })?
            }
            RcType::RC_HANDLE => {
                let h = unsafe { value.as_.handle };
                let ud = self
                    .handles
                    .iter()
                    .find(|ud| {
                        ud.borrow::<Handle>()
                            .is_ok_and(|x| x.type_id == h.type_id && x.ptr == h.ptr)
                    })
                    .ok_or("a handle this call was not given")?;
                Value::UserData(ud.clone())
            }
            RcType(t) => return Err(format!("unknown value type {t}")),
        })
    }

    fn table(&self, r: RcRef) -> Result<Table, String> {
        match self.deref(r)? {
            Value::Table(t) => Ok(t),
            other => Err(format!(
                "expected a table reference, got a {}",
                other.type_name()
            )),
        }
    }
}

/// A plugin-owned object Lua holds.
struct Handle {
    type_id: u32,
    ptr: *mut c_void,
    name: &'static str,
    destroy: Option<unsafe extern "C" fn(*mut c_void)>,
}

impl Drop for Handle {
    fn drop(&mut self) {
        if let Some(destroy) = self.destroy {
            // SAFETY: the plugin handed us `ptr` with this destructor.
            unsafe { destroy(self.ptr) };
        }
    }
}

impl UserData for Handle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::Index, |lua, this, key: Value| {
            let all: Table = lua.named_registry_value(METHODS_KEY)?;
            match all.raw_get::<Option<Table>>(this.type_id + 1)? {
                Some(methods) => methods.raw_get::<Value>(key),
                None => Ok(Value::Nil),
            }
        });
        methods.add_meta_method(MetaMethod::ToString, |_, this, ()| {
            Ok(format!("{}: {:p}", this.name, this.ptr))
        });
    }
}

/// What the plugin passes back into the host: the state behind `call`.
///
/// # Safety
/// `call` must be one `invoke` created and is still running.
unsafe fn state<'s>(call: *mut RcCall) -> &'s mut CallState<'s> {
    unsafe { &mut *((*call).internal as *mut CallState<'s>) }
}

/// Runs a host function body, turning a panic or an error message into a
/// status. Nothing may unwind out of an `extern "C"` function.
fn guarded(call: *mut RcCall, f: impl FnOnce(&mut CallState) -> Result<(), Failure>) -> RcStatus {
    if call.is_null() {
        return RcStatus::RC_ERROR;
    }
    let st = unsafe { state(call) };
    match std::panic::catch_unwind(AssertUnwindSafe(|| f(st))) {
        Ok(Ok(())) => RcStatus::RC_OK,
        Ok(Err(Failure::Misuse(msg))) => {
            st.message = Some(msg);
            RcStatus::RC_ERROR
        }
        Ok(Err(Failure::Lua(e))) => {
            st.error = Some(e);
            RcStatus::RC_PROPAGATE
        }
        Err(_) => {
            st.message = Some("the host panicked".into());
            RcStatus::RC_ERROR
        }
    }
}

enum Failure {
    /// The plugin used the ABI wrongly: RC_ERROR, with this message.
    Misuse(String),
    /// Lua raised: RC_PROPAGATE, and the wrapper re-raises it.
    Lua(mlua::Error),
}

impl From<String> for Failure {
    fn from(msg: String) -> Self {
        Failure::Misuse(msg)
    }
}

impl From<mlua::Error> for Failure {
    fn from(e: mlua::Error) -> Self {
        Failure::Lua(e)
    }
}

fn non_null<'p, T>(ptr: *const T, what: &str) -> Result<&'p T, Failure> {
    unsafe { ptr.as_ref() }.ok_or_else(|| Failure::Misuse(format!("{what} is null")))
}

unsafe extern "C" fn push(call: *mut RcCall, value: *const RcValue) -> RcStatus {
    guarded(call, |st| {
        let v = st.lua_from(non_null(value, "value")?)?;
        st.results.push(v);
        Ok(())
    })
}

unsafe extern "C" fn push_handle(call: *mut RcCall, type_id: u32, ptr: *mut c_void) -> RcStatus {
    guarded(call, |st| {
        let ty = st
            .plugins
            .handle_types
            .get(type_id as usize)
            .ok_or_else(|| format!("unknown handle type {type_id}"))?;
        let ud = st.lua.create_userdata(Handle {
            type_id,
            ptr,
            name: ty.name,
            destroy: ty.destroy,
        })?;
        // Closes it at the end of the request rather than whenever the
        // collector gets to it; see `runtime::retain`.
        crate::api::mark_finalizers();
        st.handles.push(ud.clone());
        st.results.push(Value::UserData(ud));
        Ok(())
    })
}

unsafe extern "C" fn error(call: *mut RcCall, msg: *const c_char, len: usize) -> RcStatus {
    guarded(call, |st| {
        let bytes: &[u8] = if msg.is_null() || len == 0 {
            b"plugin error"
        } else {
            unsafe { std::slice::from_raw_parts(msg.cast(), len) }
        };
        st.message = Some(String::from_utf8_lossy(bytes).into_owned());
        Ok(())
    })
}

unsafe extern "C" fn write(call: *mut RcCall, bytes: *const c_char, len: usize) -> RcStatus {
    guarded(call, |_| {
        if len == 0 {
            return Ok(());
        }
        let bytes = unsafe {
            std::slice::from_raw_parts(non_null(bytes, "bytes")? as *const c_char as *const u8, len)
        };
        crate::api::emit_output(bytes)?;
        Ok(())
    })
}

unsafe extern "C" fn call_fn(
    call: *mut RcCall,
    func: RcRef,
    args: *const RcValue,
    nargs: usize,
    results: *mut RcValue,
    max_results: usize,
    nresults: *mut usize,
) -> RcStatus {
    guarded(call, |st| {
        let func = match st.deref(func)? {
            Value::Function(f) => f,
            other => return Err(format!("call: not a function but a {}", other.type_name()).into()),
        };
        let mut lua_args = Vec::with_capacity(nargs);
        for i in 0..nargs {
            lua_args.push(st.lua_from(unsafe { &*args.add(i) })?);
        }
        let returned: MultiValue = func.call(MultiValue::from_vec(lua_args))?;
        let n = returned.len().min(max_results);
        for (i, v) in returned.iter().take(n).enumerate() {
            let rc = st.rc_from(v, true)?;
            unsafe { *results.add(i) = rc };
        }
        if !nresults.is_null() {
            unsafe { *nresults = n };
        }
        Ok(())
    })
}

unsafe extern "C" fn new_table(call: *mut RcCall, out: *mut RcRef) -> RcStatus {
    guarded(call, |st| {
        let t = st.lua.create_table()?;
        let r = st.slot(Value::Table(t))?;
        unsafe { *out = r };
        Ok(())
    })
}

unsafe extern "C" fn table_get(
    call: *mut RcCall,
    table: RcRef,
    key: *const RcValue,
    out: *mut RcValue,
) -> RcStatus {
    guarded(call, |st| {
        let t = st.table(table)?;
        let key = st.lua_from(non_null(key, "key")?)?;
        let v: Value = t.get(key)?;
        let rc = st.rc_from(&v, true)?;
        unsafe { *out = rc };
        Ok(())
    })
}

unsafe extern "C" fn table_set(
    call: *mut RcCall,
    table: RcRef,
    key: *const RcValue,
    value: *const RcValue,
) -> RcStatus {
    guarded(call, |st| {
        let t = st.table(table)?;
        let key = st.lua_from(non_null(key, "key")?)?;
        let value = st.lua_from(non_null(value, "value")?)?;
        t.set(key, value)?;
        Ok(())
    })
}

unsafe extern "C" fn table_next(
    call: *mut RcCall,
    table: RcRef,
    key: *mut RcValue,
    value: *mut RcValue,
) -> RcStatus {
    guarded(call, |st| {
        let t = st.table(table)?;
        let k = st.lua_from(non_null(key, "key")?)?;
        let next: mlua::Function = st.lua.named_registry_value(NEXT_KEY)?;
        let (k, v): (Value, Value) = next.call((t, k))?;
        let (k, v) = (st.rc_from(&k, true)?, st.rc_from(&v, true)?);
        unsafe {
            *key = k;
            *value = v;
        }
        Ok(())
    })
}

unsafe extern "C" fn table_len(call: *mut RcCall, table: RcRef, out: *mut usize) -> RcStatus {
    guarded(call, |st| {
        let n = st.table(table)?.len()?;
        unsafe { *out = n.max(0) as usize };
        Ok(())
    })
}

unsafe extern "C" fn retain(call: *mut RcCall, r: RcRef) -> RcStatus {
    guarded(call, |st| {
        let v = st.deref(r)?;
        RETAINED.with(|m| m.borrow_mut().insert(r.id, v));
        Ok(())
    })
}

unsafe extern "C" fn release(call: *mut RcCall, r: RcRef) -> RcStatus {
    guarded(call, |_| {
        RETAINED.with(|m| m.borrow_mut().remove(&r.id));
        Ok(())
    })
}

unsafe extern "C" fn log(level: c_int, msg: *const c_char, len: usize) {
    if msg.is_null() {
        return;
    }
    let text = String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(msg.cast(), len) });
    let level = match level {
        0 => log::Level::Debug,
        1 => log::Level::Info,
        2 => log::Level::Warn,
        _ => log::Level::Error,
    };
    log::log!(target: "plugin", level, "{text}");
}

pub(super) static HOST: RcHost = RcHost {
    size: size_of::<RcHost>() as u32,
    push,
    push_handle,
    error,
    write,
    call: call_fn,
    new_table,
    table_get,
    table_set,
    table_next,
    table_len,
    retain,
    release,
    log,
};

/// The Lua function for `f`.
pub(super) fn wrap(
    lua: &Lua,
    plugins: &'static Plugins,
    f: &'static Function,
) -> mlua::Result<mlua::Function> {
    lua.create_function(move |lua, args: MultiValue| invoke(lua, plugins, f, args))
}

fn invoke(
    lua: &Lua,
    plugins: &'static Plugins,
    f: &'static Function,
    args: MultiValue,
) -> mlua::Result<MultiValue> {
    if f.flags & RC_FN_BLOCKING != 0 {
        crate::api::mark_blocking();
    }
    let mut st = CallState::new(lua, plugins);
    let mut rc_args = Vec::with_capacity(args.len());
    for v in args.iter() {
        rc_args.push(st.rc_from(v, false).map_err(mlua::Error::runtime)?);
    }
    let mut call = RcCall {
        host: &HOST,
        args: rc_args.as_ptr(),
        nargs: rc_args.len(),
        internal: &mut st as *mut CallState as *mut c_void,
    };
    // SAFETY: the plugin's function, called as the ABI says. A panic reaching
    // an `extern "C"` boundary aborts rather than unwinding into here.
    let status = unsafe { (f.func)(&mut call, f.userdata) };
    drop(args);
    match status {
        RcStatus::RC_OK => Ok(MultiValue::from_vec(std::mem::take(&mut st.results))),
        RcStatus::RC_ERROR => Err(mlua::Error::runtime(format!(
            "{}: {}",
            f.qualified,
            st.message.take().unwrap_or_else(|| "failed".into())
        ))),
        RcStatus::RC_PROPAGATE => Err(st.error.take().unwrap_or_else(|| {
            mlua::Error::runtime(format!(
                "{}: returned RC_PROPAGATE with no failed host call to propagate",
                f.qualified
            ))
        })),
        RcStatus(other) => Err(mlua::Error::runtime(format!(
            "{}: returned unknown status {other}",
            f.qualified
        ))),
    }
}

/// Per-state pieces the host functions look up: handle method tables and the
/// real `next`, which a template's environment cannot replace.
pub(super) fn install(lua: &Lua, plugins: &'static Plugins) -> mlua::Result<()> {
    lua.set_named_registry_value(METHODS_KEY, super::method_tables(lua, plugins)?)?;
    lua.set_named_registry_value(NEXT_KEY, lua.globals().raw_get::<mlua::Function>("next")?)
}
