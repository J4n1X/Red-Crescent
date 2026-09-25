//! Bindings for writing Red Crescent plugins. `ffi` mirrors `rc_plugin.h`
//! one to one; the server uses it too, so both sides share one layout.
//! See `plugins/ABI.md` for the rules behind each entry.

pub mod ffi;
