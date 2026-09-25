//! The hostname example from ABI.md: `hostname.get()` returns the machine's
//! name, `hostname.print()` writes it into the page.

use std::ffi::c_void;

use red_crescent_plugin::ffi::*;

#[unsafe(no_mangle)]
pub extern "C" fn rc_plugin_abi() -> u32 {
    RC_PLUGIN_ABI
}

/// # Safety
/// Called by the server, once, with the pointers the ABI describes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rc_plugin_install(
    _ctx: *const RcPluginContext,
    reg: *mut RcRegistryCOM,
) -> RcStatus {
    let r = unsafe { &*reg };
    for (name, f) in [(c"get", get as RcFunction), (c"print", print as RcFunction)] {
        let status = unsafe { (r.function)(reg, name.as_ptr(), f, std::ptr::null_mut(), 0) };
        if status != RcStatus::RC_OK {
            return status;
        }
    }
    RcStatus::RC_OK
}

fn hostname() -> String {
    gethostname::gethostname().to_string_lossy().into_owned()
}

/// hostname.get() -> string
unsafe extern "C" fn get(call: *mut RcCall, _userdata: *mut c_void) -> RcStatus {
    let name = hostname();
    let value = RcValue::string(&name);
    unsafe { ((*(*call).host).push)(call, &value) }
}

/// hostname.print(): writes the name into the page, unescaped like print().
unsafe extern "C" fn print(call: *mut RcCall, _userdata: *mut c_void) -> RcStatus {
    let name = hostname();
    unsafe { ((*(*call).host).write)(call, name.as_ptr().cast(), name.len()) }
}
