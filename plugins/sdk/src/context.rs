use std::ffi::{CStr, c_char};
use std::path::Path;

use crate::error::{Error, Result};
use crate::ffi::RcPluginContext;

/// What a plugin learns at install. The host keeps these strings for the
/// life of the process, so they are `'static`.
#[derive(Clone, Copy, Debug)]
pub struct Context {
    name: &'static str,
    server_version: &'static str,
    serve_dir: &'static str,
    data_dir: &'static str,
    options_json: &'static str,
}

impl Context {
    /// # Safety
    /// `raw` is the context the host passed to `rc_plugin_install`.
    pub(crate) unsafe fn from_raw(raw: &RcPluginContext) -> Self {
        let s = |p: *const c_char| -> &'static str {
            if p.is_null() {
                return "";
            }
            unsafe { CStr::from_ptr(p) }.to_str().unwrap_or("")
        };
        Context {
            name: s(raw.name_cstr),
            server_version: s(raw.server_version_cstr),
            serve_dir: s(raw.serve_dir_cstr),
            data_dir: s(raw.data_dir_cstr),
            options_json: s(raw.options_json_cstr),
        }
    }

    /// The plugin's key in rc_config.lua, which is its Lua table's name.
    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn server_version(&self) -> &'static str {
        self.server_version
    }

    pub fn serve_dir(&self) -> &'static Path {
        Path::new(self.serve_dir)
    }

    /// The writable data directory; canonical, so `starts_with` checks work.
    pub fn data_dir(&self) -> &'static Path {
        Path::new(self.data_dir)
    }

    /// The plugin's `options` from rc_config.lua, as JSON; `{}` if none.
    pub fn options_json(&self) -> &'static str {
        self.options_json
    }

    /// The options, deserialized.
    pub fn options<T: serde::de::DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_str(self.options_json).map_err(|e| Error::new(format!("options: {e}")))
    }
}
