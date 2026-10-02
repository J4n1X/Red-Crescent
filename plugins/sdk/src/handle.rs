use std::ffi::{CStr, c_void};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::call::Call;
use crate::error::{Result, check};
use crate::ffi::RcStatus;

/// A Rust type Lua can hold as an object with methods. Implemented by
/// `#[handle]` inside `#[plugin]`; returning one from a function hands it to
/// Lua, and Lua drops it when the object is collected.
///
/// # Safety
/// `type_slot` must be unique to the type; the macro guarantees it.
pub unsafe trait Handle: Sized + 'static {
    #[doc(hidden)]
    const NAME: &'static CStr;
    #[doc(hidden)]
    fn type_slot() -> &'static AtomicU32;
    #[doc(hidden)]
    fn handle_type() -> u32 {
        Self::type_slot().load(Ordering::Relaxed)
    }
}

/// A handle on its way to Lua; destroyed if it never gets there.
pub(crate) struct NewHandle {
    type_id: u32,
    ptr: *mut c_void,
    destroy: unsafe extern "C" fn(*mut c_void),
}

impl NewHandle {
    pub(crate) fn new<T: Handle>(value: T) -> Self {
        NewHandle {
            type_id: T::handle_type(),
            ptr: Box::into_raw(Box::new(value)).cast(),
            destroy: destroy::<T>,
        }
    }

    pub(crate) fn push(self, call: Call<'_>) -> Result<()> {
        let status = unsafe { (call.host().push_handle)(call.raw(), self.type_id, self.ptr) };
        if status == RcStatus::RC_OK {
            // Lua owns it now.
            std::mem::forget(self);
        }
        check(status)
    }
}

impl Drop for NewHandle {
    fn drop(&mut self) {
        unsafe { (self.destroy)(self.ptr) }
    }
}

/// `RcHandleType::destroy` for `T`. Must not unwind into the host.
pub(crate) unsafe extern "C" fn destroy<T>(ptr: *mut c_void) {
    let boxed = std::panic::AssertUnwindSafe(ptr as *mut T);
    let _ = std::panic::catch_unwind(move || drop(unsafe { Box::from_raw({ boxed }.0) }));
}
