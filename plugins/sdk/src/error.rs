use std::fmt;

use crate::ffi::RcStatus;

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A plugin failure, raised in Lua as `plugin.function: message`.
///
/// Anything implementing `std::error::Error` converts with `?`. A failed host
/// call (an error inside a Lua callback, `exit()`, the time limit) is carried
/// through unchanged, so Lua sees the original error rather than a copy.
pub struct Error(Kind);

enum Kind {
    Message(String),
    Host(RcStatus),
}

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Error(Kind::Message(message.into()))
    }

    /// True for an error that came back from Lua through a host call.
    pub fn is_from_lua(&self) -> bool {
        matches!(self.0, Kind::Host(_))
    }

    /// Prefixes a plugin's own message; a Lua error stays as it is.
    pub fn context(self, prefix: impl fmt::Display) -> Self {
        match self.0 {
            Kind::Message(m) => Error::new(format!("{prefix}: {m}")),
            host => Error(host),
        }
    }

    /// The message, or what Lua will be told, as a status for the host.
    pub(crate) fn into_parts(self) -> (Option<String>, RcStatus) {
        match self.0 {
            Kind::Message(m) => (Some(m), RcStatus::RC_ERROR),
            Kind::Host(status) => (None, status),
        }
    }
}

impl<E: std::error::Error> From<E> for Error {
    fn from(e: E) -> Self {
        Error::new(e.to_string())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Kind::Message(m) => f.write_str(m),
            Kind::Host(_) => f.write_str("a Lua error, on its way back to Lua"),
        }
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// `return Err(Error::new(format!(...)))`.
#[macro_export]
macro_rules! bail {
    ($($arg:tt)*) => {
        return ::core::result::Result::Err($crate::Error::new(::std::format!($($arg)*)))
    };
}

/// A host call's status as a `Result`. On `RC_ERROR` the host has already
/// recorded its message, so only the status travels.
pub(crate) fn check(status: RcStatus) -> Result<()> {
    match status {
        RcStatus::RC_OK => Ok(()),
        other => Err(Error(Kind::Host(other))),
    }
}
