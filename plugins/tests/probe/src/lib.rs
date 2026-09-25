//! A plugin that exercises every part of the ABI, for tests/plugins.rs.

use std::ffi::{CStr, c_void};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use red_crescent_plugin::ffi::*;

static OPTIONS: Mutex<String> = Mutex::new(String::new());
static KEPT: Mutex<Option<RcRef>> = Mutex::new(None);
static DESTROYED: AtomicUsize = AtomicUsize::new(0);
static COUNTER_TYPE: AtomicU32 = AtomicU32::new(u32::MAX);

#[unsafe(no_mangle)]
pub extern "C" fn rc_plugin_abi() -> u32 {
    RC_PLUGIN_ABI
}

/// # Safety
/// Called by the server, once, with the pointers the ABI describes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rc_plugin_install(
    ctx: *const RcPluginContext,
    reg: *mut RcRegistryCOM,
) -> RcStatus {
    let ctx = unsafe { &*ctx };
    *OPTIONS.lock().unwrap() = unsafe { CStr::from_ptr(ctx.options_json_cstr) }
        .to_string_lossy()
        .into_owned();

    let functions: &[(&CStr, RcFunction, u32)] = &[
        (c"noop", noop, 0),
        (c"echo", echo, 0),
        (c"fail", fail, 0),
        (c"bogus", bogus, 0),
        (c"apply", apply, 0),
        (c"each", each, 0),
        (c"sum", sum, 0),
        (c"make", make, 0),
        (c"write", write, 0),
        (c"options", options, 0),
        (c"keep", keep, 0),
        (c"call_kept", call_kept, 0),
        (c"counter", counter, 0),
        (c"destroyed", destroyed, 0),
        (c"wait", noop, RC_FN_BLOCKING),
    ];
    let r = unsafe { &*reg };
    for (name, f, flags) in functions {
        let status = unsafe { (r.function)(reg, name.as_ptr(), *f, std::ptr::null_mut(), *flags) };
        if status != RcStatus::RC_OK {
            return status;
        }
    }

    static METHODS: [RcMethodSync; 2] = [
        RcMethodSync(RcMethod {
            name_cstr: c"inc".as_ptr(),
            fn_: counter_inc,
            flags: 0,
        }),
        RcMethodSync(RcMethod {
            name_cstr: c"get".as_ptr(),
            fn_: counter_get,
            flags: 0,
        }),
    ];
    let ty = RcHandleType {
        name_cstr: c"Counter".as_ptr(),
        methods: METHODS.as_ptr().cast(),
        nmethods: METHODS.len(),
        destroy: Some(counter_destroy),
        userdata: std::ptr::null_mut(),
    };
    let mut id = 0;
    let status = unsafe { (r.handle_type)(reg, &ty, &mut id) };
    COUNTER_TYPE.store(id, Ordering::Relaxed);
    status
}

/// `RcMethod` holds raw pointers, so a static array of them needs this.
#[repr(transparent)]
struct RcMethodSync(RcMethod);
unsafe impl Sync for RcMethodSync {}

fn host(call: *mut RcCall) -> &'static RcHost {
    unsafe { &*(*call).host }
}

fn args<'a>(call: *mut RcCall) -> &'a [RcValue] {
    unsafe { (*call).args() }
}

fn fail_with(call: *mut RcCall, msg: &str) -> RcStatus {
    unsafe { (host(call).error)(call, msg.as_ptr().cast(), msg.len()) };
    RcStatus::RC_ERROR
}

/// Returns early with any status but RC_OK.
macro_rules! check {
    ($e:expr) => {
        match $e {
            RcStatus::RC_OK => {}
            other => return other,
        }
    };
}

fn arg_ref(call: *mut RcCall, i: usize) -> Option<RcRef> {
    args(call).get(i).and_then(RcValue::as_ref)
}

unsafe extern "C" fn noop(_call: *mut RcCall, _ud: *mut c_void) -> RcStatus {
    RcStatus::RC_OK
}

/// Every argument back, unchanged.
unsafe extern "C" fn echo(call: *mut RcCall, _ud: *mut c_void) -> RcStatus {
    for v in args(call) {
        check!(unsafe { (host(call).push)(call, v) });
    }
    RcStatus::RC_OK
}

unsafe extern "C" fn fail(call: *mut RcCall, _ud: *mut c_void) -> RcStatus {
    let msg = args(call)
        .first()
        .and_then(|v| unsafe { v.as_bytes() })
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_else(|| "no message".into());
    fail_with(call, &msg)
}

/// Claims a failed host call that never happened.
unsafe extern "C" fn bogus(_call: *mut RcCall, _ud: *mut c_void) -> RcStatus {
    RcStatus::RC_PROPAGATE
}

/// apply(fn, ...) -> fn(...), up to 8 results.
unsafe extern "C" fn apply(call: *mut RcCall, _ud: *mut c_void) -> RcStatus {
    let Some(f) = arg_ref(call, 0) else {
        return fail_with(call, "apply: expected a function");
    };
    let rest = &args(call)[1..];
    let mut results = [RcValue::nil(); 8];
    let mut n = 0;
    check!(unsafe {
        (host(call).call)(
            call,
            f,
            rest.as_ptr(),
            rest.len(),
            results.as_mut_ptr(),
            8,
            &mut n,
        )
    });
    for v in &results[..n] {
        check!(unsafe { (host(call).push)(call, v) });
    }
    RcStatus::RC_OK
}

/// each(t, fn): fn(k, v) for every pair, in `next` order.
unsafe extern "C" fn each(call: *mut RcCall, _ud: *mut c_void) -> RcStatus {
    let (Some(t), Some(f)) = (arg_ref(call, 0), arg_ref(call, 1)) else {
        return fail_with(call, "each: expected a table and a function");
    };
    let h = host(call);
    let mut key = RcValue::nil();
    let mut value = RcValue::nil();
    loop {
        check!(unsafe { (h.table_next)(call, t, &mut key, &mut value) });
        if key.r#type == RcType::RC_NIL {
            return RcStatus::RC_OK;
        }
        let pair = [key, value];
        let mut n = 0;
        check!(unsafe { (h.call)(call, f, pair.as_ptr(), 2, std::ptr::null_mut(), 0, &mut n) });
    }
}

/// sum(t): t[1] + ... + t[#t].
unsafe extern "C" fn sum(call: *mut RcCall, _ud: *mut c_void) -> RcStatus {
    let Some(t) = arg_ref(call, 0) else {
        return fail_with(call, "sum: expected a table");
    };
    let h = host(call);
    let mut len = 0;
    check!(unsafe { (h.table_len)(call, t, &mut len) });
    let mut total = 0.0;
    for i in 1..=len {
        let mut v = RcValue::nil();
        check!(unsafe { (h.table_get)(call, t, &RcValue::int(i as i64), &mut v) });
        total += v.as_number().unwrap_or(0.0);
    }
    unsafe { (h.push)(call, &RcValue::number(total)) }
}

/// make(n) -> { 1, 2, ..., n }
unsafe extern "C" fn make(call: *mut RcCall, _ud: *mut c_void) -> RcStatus {
    let n = args(call).first().and_then(RcValue::as_int).unwrap_or(0);
    let h = host(call);
    let mut t = RcRef { id: 0 };
    check!(unsafe { (h.new_table)(call, &mut t) });
    for i in 1..=n {
        check!(unsafe { (h.table_set)(call, t, &RcValue::int(i), &RcValue::int(i)) });
    }
    unsafe { (h.push)(call, &RcValue::reference(RcType::RC_TABLE, t)) }
}

unsafe extern "C" fn write(call: *mut RcCall, _ud: *mut c_void) -> RcStatus {
    let Some(bytes) = args(call).first().and_then(|v| unsafe { v.as_bytes() }) else {
        return fail_with(call, "write: expected a string");
    };
    unsafe { (host(call).write)(call, bytes.as_ptr().cast(), bytes.len()) }
}

unsafe extern "C" fn options(call: *mut RcCall, _ud: *mut c_void) -> RcStatus {
    let options = OPTIONS.lock().unwrap().clone();
    unsafe { (host(call).push)(call, &RcValue::string(&options)) }
}

/// keep(fn): retains fn for call_kept(), which may come in a later call.
unsafe extern "C" fn keep(call: *mut RcCall, _ud: *mut c_void) -> RcStatus {
    let Some(f) = arg_ref(call, 0) else {
        return fail_with(call, "keep: expected a function");
    };
    check!(unsafe { (host(call).retain)(call, f) });
    *KEPT.lock().unwrap() = Some(f);
    RcStatus::RC_OK
}

unsafe extern "C" fn call_kept(call: *mut RcCall, _ud: *mut c_void) -> RcStatus {
    let Some(f) = *KEPT.lock().unwrap() else {
        return fail_with(call, "nothing kept");
    };
    let mut results = [RcValue::nil(); 1];
    let mut n = 0;
    check!(unsafe {
        (host(call).call)(
            call,
            f,
            std::ptr::null(),
            0,
            results.as_mut_ptr(),
            1,
            &mut n,
        )
    });
    if n == 1 {
        check!(unsafe { (host(call).push)(call, &results[0]) });
    }
    RcStatus::RC_OK
}

/// counter(start) -> a Counter handle.
unsafe extern "C" fn counter(call: *mut RcCall, _ud: *mut c_void) -> RcStatus {
    let start = args(call).first().and_then(RcValue::as_int).unwrap_or(0);
    let ptr = Box::into_raw(Box::new(start)) as *mut c_void;
    let status =
        unsafe { (host(call).push_handle)(call, COUNTER_TYPE.load(Ordering::Relaxed), ptr) };
    if status != RcStatus::RC_OK {
        drop(unsafe { Box::from_raw(ptr as *mut i64) });
    }
    status
}

fn counter_of<'a>(call: *mut RcCall) -> Option<&'a mut i64> {
    let h = args(call).first()?.as_handle()?;
    (h.type_id == COUNTER_TYPE.load(Ordering::Relaxed))
        .then(|| unsafe { &mut *(h.ptr as *mut i64) })
}

unsafe extern "C" fn counter_inc(call: *mut RcCall, _ud: *mut c_void) -> RcStatus {
    let Some(n) = counter_of(call) else {
        return fail_with(call, "inc: not a Counter");
    };
    *n += 1;
    RcStatus::RC_OK
}

unsafe extern "C" fn counter_get(call: *mut RcCall, _ud: *mut c_void) -> RcStatus {
    let Some(n) = counter_of(call) else {
        return fail_with(call, "get: not a Counter");
    };
    unsafe { (host(call).push)(call, &RcValue::int(*n)) }
}

unsafe extern "C" fn counter_destroy(ptr: *mut c_void) {
    drop(unsafe { Box::from_raw(ptr as *mut i64) });
    DESTROYED.fetch_add(1, Ordering::Relaxed);
}

unsafe extern "C" fn destroyed(call: *mut RcCall, _ud: *mut c_void) -> RcStatus {
    let n = DESTROYED.load(Ordering::Relaxed) as i64;
    unsafe { (host(call).push)(call, &RcValue::int(n)) }
}
