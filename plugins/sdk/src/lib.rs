//! Writing Red Crescent plugins in Rust.
//!
//! ```ignore
//! use red_crescent_plugin::prelude::*;
//!
//! #[plugin]
//! mod greet {
//!     use super::*;
//!
//!     /// greet.hello(name) -> string
//!     #[function]
//!     fn hello(name: &str) -> String {
//!         format!("Hello, {name}!")
//!     }
//! }
//! ```
//!
//! See [`plugin`] for what goes inside the module, and `plugins/ABI.md` for
//! the C interface underneath. `ffi` mirrors `rc_plugin.h` one to one; the
//! server uses it too, so both sides share one layout.

mod call;
mod context;
mod convert;
mod error;
pub mod ffi;
mod handle;
mod value;

#[doc(hidden)]
#[path = "private.rs"]
pub mod __private;

pub use call::Call;
pub use context::Context;
pub use convert::{FromLua, FromLuaMulti, IntoLua, IntoLuaMulti, Lowered};
pub use error::{Error, Result};
pub use handle::Handle;
pub use value::{
    AnyHandle, AnyRef, Bytes, Function, Pairs, Reference, Retained, Sequence, Table, Value,
    Variadic,
};

/// Records go to the server's log, under the `plugin` target.
pub use log;

#[cfg(feature = "macros")]
pub use red_crescent_plugin_macros::plugin;

pub mod prelude {
    #[cfg(feature = "macros")]
    pub use crate::plugin;
    pub use crate::{
        AnyHandle, Bytes, Call, Context, Error, Function, Result, Retained, Table, Value, Variadic,
        bail,
    };
}
