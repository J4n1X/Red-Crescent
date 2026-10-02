use std::fmt;
use std::marker::PhantomData;

use crate::convert::{IntoLua, Lowered};
use crate::error::{Result, check};
use crate::ffi::{RcCall, RcHost, RcRef, RcValue};
use crate::value::{Table, Value};

/// One running call from Lua. Everything borrowed from it (strings, tables,
/// functions) lives for `'c`, which ends when the plugin function returns.
///
/// `Copy` and `!Send`: references and handles never cross threads.
#[derive(Clone, Copy)]
pub struct Call<'c> {
    raw: *mut RcCall,
    /// Invariant, so `'c` can be neither stretched nor shrunk.
    _call: PhantomData<fn(&'c ()) -> &'c ()>,
}

impl<'c> Call<'c> {
    /// # Safety
    /// `raw` is the call the host passed in, and it runs for all of `'c`.
    pub(crate) unsafe fn from_raw(raw: *mut RcCall) -> Self {
        Call {
            raw,
            _call: PhantomData,
        }
    }

    pub(crate) fn raw(&self) -> *mut RcCall {
        self.raw
    }

    pub(crate) fn host(&self) -> &'c RcHost {
        unsafe { &*(*self.raw).host }
    }

    fn raw_args(&self) -> &'c [RcValue] {
        unsafe { (*self.raw).args() }
    }

    /// The number of arguments Lua passed, including `self` for a method.
    pub fn nargs(&self) -> usize {
        self.raw_args().len()
    }

    /// Argument `i`, from 0; nil past the end, as in Lua.
    pub fn arg(&self, i: usize) -> Value<'c> {
        self.raw_args()
            .get(i)
            .map_or(Value::Nil, |v| Value::from_raw(*self, v))
    }

    pub fn args(&self) -> impl ExactSizeIterator<Item = Value<'c>> + 'c {
        let call = *self;
        self.raw_args()
            .iter()
            .map(move |v| Value::from_raw(call, v))
    }

    /// Writes into the page, unescaped, like `print()`.
    pub fn write(&self, bytes: impl AsRef<[u8]>) -> Result<()> {
        let bytes = bytes.as_ref();
        check(unsafe { (self.host().write)(self.raw, bytes.as_ptr().cast(), bytes.len()) })
    }

    pub fn new_table(&self) -> Result<Table<'c>> {
        let mut r = RcRef { id: 0 };
        check(unsafe { (self.host().new_table)(self.raw, &mut r) })?;
        Ok(Table { call: *self, r })
    }

    /// A table with these key/value pairs.
    pub fn table_from<K: IntoLua, V: IntoLua>(
        &self,
        pairs: impl IntoIterator<Item = (K, V)>,
    ) -> Result<Table<'c>> {
        let table = self.new_table()?;
        for (k, v) in pairs {
            table.set(k, v)?;
        }
        Ok(table)
    }

    /// A sequence: `{ values[1], values[2], ... }`.
    pub fn sequence_from<V: IntoLua>(
        &self,
        values: impl IntoIterator<Item = V>,
    ) -> Result<Table<'c>> {
        self.table_from((1..).zip(values))
    }

    /// Appends a return value.
    pub(crate) fn push(&self, value: Lowered<'_>) -> Result<()> {
        match value.into_new_handle() {
            Ok(handle) => handle.push(*self),
            Err(value) => {
                let raw = value.raw()?;
                check(unsafe { (self.host().push)(self.raw, &raw) })
            }
        }
    }
}

impl fmt::Debug for Call<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Call({} args)", self.nargs())
    }
}
