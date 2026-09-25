//! `rc_plugin.h`, as `#[repr(C)]` Rust. Keep the two in step: a field out of
//! order here is a silent memory-safety bug on the other side.
//!
//! Status and type tags are integer newtypes rather than enums: a value from
//! C outside an enum's variants would be undefined behavior in Rust.

#![allow(non_camel_case_types)]

use std::ffi::{c_char, c_int, c_void};

pub const RC_PLUGIN_ABI: u32 = 1;

#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RcStatus(pub c_int);

impl RcStatus {
    pub const RC_OK: RcStatus = RcStatus(0);
    /// The plugin failed; the message was set with `host.error`.
    pub const RC_ERROR: RcStatus = RcStatus(1);
    /// A host call failed; return this unchanged.
    pub const RC_PROPAGATE: RcStatus = RcStatus(2);
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RcType(pub c_int);

impl RcType {
    pub const RC_NIL: RcType = RcType(0);
    pub const RC_BOOL: RcType = RcType(1);
    pub const RC_INT: RcType = RcType(2);
    pub const RC_NUMBER: RcType = RcType(3);
    pub const RC_STRING: RcType = RcType(4);
    pub const RC_TABLE: RcType = RcType(5);
    pub const RC_FUNCTION: RcType = RcType(6);
    pub const RC_HANDLE: RcType = RcType(7);
    pub const RC_OTHER: RcType = RcType(8);
}

/// A Lua table, function or other value, owned by Lua.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RcRef {
    pub id: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct RcStr {
    pub ptr: *const c_char,
    pub len: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct RcHandleRef {
    pub type_id: u32,
    pub ptr: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union RcValueData {
    pub b: c_int,
    pub i: i64,
    pub n: f64,
    pub s: RcStr,
    pub r#ref: RcRef,
    pub handle: RcHandleRef,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct RcValue {
    pub r#type: RcType,
    pub as_: RcValueData,
}

impl RcValue {
    pub fn nil() -> Self {
        RcValue {
            r#type: RcType::RC_NIL,
            as_: RcValueData { i: 0 },
        }
    }

    pub fn boolean(b: bool) -> Self {
        RcValue {
            r#type: RcType::RC_BOOL,
            as_: RcValueData { b: b as c_int },
        }
    }

    pub fn int(i: i64) -> Self {
        RcValue {
            r#type: RcType::RC_INT,
            as_: RcValueData { i },
        }
    }

    pub fn number(n: f64) -> Self {
        RcValue {
            r#type: RcType::RC_NUMBER,
            as_: RcValueData { n },
        }
    }

    /// Borrows `bytes`; valid for as long as they are.
    pub fn bytes(bytes: &[u8]) -> Self {
        RcValue {
            r#type: RcType::RC_STRING,
            as_: RcValueData {
                s: RcStr {
                    ptr: bytes.as_ptr().cast(),
                    len: bytes.len(),
                },
            },
        }
    }

    pub fn string(s: &str) -> Self {
        Self::bytes(s.as_bytes())
    }

    pub fn reference(r#type: RcType, r: RcRef) -> Self {
        RcValue {
            r#type,
            as_: RcValueData { r#ref: r },
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        if self.r#type == RcType::RC_INT {
            Some(unsafe { self.as_.i })
        } else {
            None
        }
    }

    pub fn as_number(&self) -> Option<f64> {
        match self.r#type {
            RcType::RC_NUMBER => Some(unsafe { self.as_.n }),
            RcType::RC_INT => Some(unsafe { self.as_.i } as f64),
            _ => None,
        }
    }

    /// # Safety
    /// The value must be one the host handed over during the current call.
    pub unsafe fn as_bytes(&self) -> Option<&[u8]> {
        if self.r#type != RcType::RC_STRING {
            return None;
        }
        let s = unsafe { self.as_.s };
        if s.len == 0 {
            return Some(&[]);
        }
        Some(unsafe { std::slice::from_raw_parts(s.ptr.cast(), s.len) })
    }

    pub fn as_ref(&self) -> Option<RcRef> {
        matches!(
            self.r#type,
            RcType::RC_TABLE | RcType::RC_FUNCTION | RcType::RC_OTHER
        )
        .then(|| unsafe { self.as_.r#ref })
    }

    pub fn as_handle(&self) -> Option<RcHandleRef> {
        if self.r#type == RcType::RC_HANDLE {
            Some(unsafe { self.as_.handle })
        } else {
            None
        }
    }
}

#[repr(C)]
pub struct RcCall {
    pub host: *const RcHost,
    pub args: *const RcValue,
    pub nargs: usize,
    /// The host's; do not touch.
    pub internal: *mut c_void,
}

impl RcCall {
    /// # Safety
    /// `self` must be the call the host passed in, still running.
    pub unsafe fn args(&self) -> &[RcValue] {
        if self.nargs == 0 {
            return &[];
        }
        unsafe { std::slice::from_raw_parts(self.args, self.nargs) }
    }
}

pub type RcFunction = unsafe extern "C" fn(call: *mut RcCall, userdata: *mut c_void) -> RcStatus;

#[repr(C)]
pub struct RcPluginContext {
    pub size: u32,
    pub abi: u32,
    pub server_version_cstr: *const c_char,
    pub name_cstr: *const c_char,
    pub serve_dir_cstr: *const c_char,
    pub data_dir_cstr: *const c_char,
    pub options_json_cstr: *const c_char,
    pub host: *const RcHost,
}

/// May wait (network, disk): keeps the template off the inline path.
pub const RC_FN_BLOCKING: u32 = 1 << 0;

#[repr(C)]
pub struct RcMethod {
    pub name_cstr: *const c_char,
    pub fn_: RcFunction,
    pub flags: u32,
}

#[repr(C)]
pub struct RcHandleType {
    pub name_cstr: *const c_char,
    /// `obj:method(...)` passes the object as `args[0]`.
    pub methods: *const RcMethod,
    pub nmethods: usize,
    /// Runs when Lua collects the object; must not call the host.
    pub destroy: Option<unsafe extern "C" fn(ptr: *mut c_void)>,
    /// Handed to every method.
    pub userdata: *mut c_void,
}

#[repr(C)]
pub struct RcRegistryCOM {
    pub size: u32,
    pub function: unsafe extern "C" fn(
        this: *mut RcRegistryCOM,
        name_cstr: *const c_char,
        fn_: RcFunction,
        userdata: *mut c_void,
        flags: u32,
    ) -> RcStatus,
    pub handle_type: unsafe extern "C" fn(
        this: *mut RcRegistryCOM,
        r#type: *const RcHandleType,
        type_id: *mut u32,
    ) -> RcStatus,
}

#[repr(C)]
pub struct RcHost {
    pub size: u32,

    pub push: unsafe extern "C" fn(call: *mut RcCall, value: *const RcValue) -> RcStatus,
    pub push_handle:
        unsafe extern "C" fn(call: *mut RcCall, type_id: u32, ptr: *mut c_void) -> RcStatus,
    pub error: unsafe extern "C" fn(call: *mut RcCall, msg: *const c_char, len: usize) -> RcStatus,

    pub write:
        unsafe extern "C" fn(call: *mut RcCall, bytes: *const c_char, len: usize) -> RcStatus,

    pub call: unsafe extern "C" fn(
        call: *mut RcCall,
        func: RcRef,
        args: *const RcValue,
        nargs: usize,
        results: *mut RcValue,
        max_results: usize,
        nresults: *mut usize,
    ) -> RcStatus,

    pub new_table: unsafe extern "C" fn(call: *mut RcCall, out: *mut RcRef) -> RcStatus,
    pub table_get: unsafe extern "C" fn(
        call: *mut RcCall,
        table: RcRef,
        key: *const RcValue,
        out: *mut RcValue,
    ) -> RcStatus,
    pub table_set: unsafe extern "C" fn(
        call: *mut RcCall,
        table: RcRef,
        key: *const RcValue,
        value: *const RcValue,
    ) -> RcStatus,
    pub table_next: unsafe extern "C" fn(
        call: *mut RcCall,
        table: RcRef,
        key: *mut RcValue,
        value: *mut RcValue,
    ) -> RcStatus,
    pub table_len:
        unsafe extern "C" fn(call: *mut RcCall, table: RcRef, out: *mut usize) -> RcStatus,

    pub retain: unsafe extern "C" fn(call: *mut RcCall, r: RcRef) -> RcStatus,
    pub release: unsafe extern "C" fn(call: *mut RcCall, r: RcRef) -> RcStatus,

    pub log: unsafe extern "C" fn(level: c_int, msg: *const c_char, len: usize),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    /// Compiles a probe against `rc_plugin.h` with the system C compiler and
    /// compares every size and field offset with this file. Skipped, loudly,
    /// where there is no `cc`.
    #[test]
    fn layouts_match_the_header() {
        let dir = std::env::temp_dir().join(format!("rc-abi-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let header = concat!(env!("CARGO_MANIFEST_DIR"), "/../rc_plugin.h");
        let source = dir.join("probe.c");
        std::fs::write(
            &source,
            format!(
                r#"#include "{header}"
#include <stdio.h>
#define S(t) printf(#t " %zu\n", sizeof(t));
#define O(t, f) printf(#t "." #f " %zu\n", offsetof(t, f));
int main(void) {{
    S(RcValue) S(RcCall) S(RcPluginContext) S(RcMethod) S(RcHandleType)
    S(RcRegistryCOM) S(RcHost) S(RcStatus) S(RcType)
    O(RcValue, as) O(RcCall, internal) O(RcPluginContext, host)
    O(RcMethod, flags) O(RcHandleType, userdata) O(RcRegistryCOM, handle_type)
    O(RcHost, write) O(RcHost, call) O(RcHost, table_len) O(RcHost, log)
    return 0;
}}"#
            ),
        )
        .unwrap();
        let exe = dir.join("probe");
        match std::process::Command::new("cc")
            .arg(&source)
            .arg("-o")
            .arg(&exe)
            .status()
        {
            Ok(status) => assert!(status.success(), "rc_plugin.h did not compile"),
            Err(e) => {
                eprintln!("SKIPPED: no C compiler to check rc_plugin.h against ({e})");
                return;
            }
        }
        let output = std::process::Command::new(&exe).output().unwrap();
        let c = String::from_utf8(output.stdout).unwrap();

        let rust = [
            ("RcValue", size_of::<RcValue>()),
            ("RcCall", size_of::<RcCall>()),
            ("RcPluginContext", size_of::<RcPluginContext>()),
            ("RcMethod", size_of::<RcMethod>()),
            ("RcHandleType", size_of::<RcHandleType>()),
            ("RcRegistryCOM", size_of::<RcRegistryCOM>()),
            ("RcHost", size_of::<RcHost>()),
            ("RcStatus", size_of::<RcStatus>()),
            ("RcType", size_of::<RcType>()),
            ("RcValue.as", offset_of!(RcValue, as_)),
            ("RcCall.internal", offset_of!(RcCall, internal)),
            ("RcPluginContext.host", offset_of!(RcPluginContext, host)),
            ("RcMethod.flags", offset_of!(RcMethod, flags)),
            ("RcHandleType.userdata", offset_of!(RcHandleType, userdata)),
            (
                "RcRegistryCOM.handle_type",
                offset_of!(RcRegistryCOM, handle_type),
            ),
            ("RcHost.write", offset_of!(RcHost, write)),
            ("RcHost.call", offset_of!(RcHost, call)),
            ("RcHost.table_len", offset_of!(RcHost, table_len)),
            ("RcHost.log", offset_of!(RcHost, log)),
        ]
        .iter()
        .map(|(name, n)| format!("{name} {n}\n"))
        .collect::<String>();
        assert_eq!(c, rust);
    }
}
