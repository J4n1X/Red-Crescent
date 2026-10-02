use std::ffi::c_void;
use std::fmt;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};

use crate::call::Call;
use crate::convert::{FromLua, FromLuaMulti, IntoLua, IntoLuaMulti};
use crate::error::{Result, check};
use crate::ffi::{RcHandleRef, RcRef, RcType, RcValue, RcValueData};
use crate::handle::Handle;

/// A Lua value as a plugin sees it, borrowed from the call.
#[derive(Clone, Copy, Debug)]
pub enum Value<'c> {
    Nil,
    /// The null sentinel: `sqlite.NULL`, `json.decode("null")`. Converts to
    /// `None` like nil does.
    Null,
    Boolean(bool),
    Integer(i64),
    Number(f64),
    /// Lua strings are bytes; see [`Value::as_str`].
    String(&'c [u8]),
    Table(Table<'c>),
    Function(Function<'c>),
    /// An object of one of this plugin's `#[handle]` types.
    Handle(AnyHandle<'c>),
    /// Anything else: a coroutine, another library's userdata.
    Other(AnyRef<'c>),
}

impl<'c> Value<'c> {
    pub(crate) fn from_raw(call: Call<'c>, v: &RcValue) -> Self {
        // SAFETY (the union reads): each arm reads the member its tag names,
        // and the host keeps strings alive until the call returns.
        unsafe {
            match v.r#type {
                RcType::RC_NULL => Value::Null,
                RcType::RC_BOOL => Value::Boolean(v.as_.b != 0),
                RcType::RC_INT => Value::Integer(v.as_.i),
                RcType::RC_NUMBER => Value::Number(v.as_.n),
                RcType::RC_STRING => {
                    let s = v.as_.s;
                    Value::String(if s.len == 0 || s.ptr.is_null() {
                        &[]
                    } else {
                        std::slice::from_raw_parts(s.ptr.cast(), s.len)
                    })
                }
                RcType::RC_TABLE => Value::Table(Table {
                    call,
                    r: v.as_.r#ref,
                }),
                RcType::RC_FUNCTION => Value::Function(Function {
                    call,
                    r: v.as_.r#ref,
                }),
                RcType::RC_OTHER => Value::Other(AnyRef {
                    call,
                    r: v.as_.r#ref,
                }),
                RcType::RC_HANDLE => Value::Handle(AnyHandle {
                    raw: v.as_.handle,
                    _call: PhantomData,
                }),
                _ => Value::Nil,
            }
        }
    }

    pub(crate) fn to_raw(self) -> RcValue {
        match self {
            Value::Nil => RcValue::nil(),
            Value::Null => RcValue {
                r#type: RcType::RC_NULL,
                as_: RcValueData { i: 0 },
            },
            Value::Boolean(b) => RcValue::boolean(b),
            Value::Integer(i) => RcValue::int(i),
            Value::Number(n) => RcValue::number(n),
            Value::String(s) => RcValue::bytes(s),
            Value::Table(t) => RcValue::reference(RcType::RC_TABLE, t.r),
            Value::Function(f) => RcValue::reference(RcType::RC_FUNCTION, f.r),
            Value::Other(o) => RcValue::reference(RcType::RC_OTHER, o.r),
            Value::Handle(h) => RcValue {
                r#type: RcType::RC_HANDLE,
                as_: RcValueData { handle: h.raw },
            },
        }
    }

    /// The name Lua's `type()` would give.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Nil => "nil",
            Value::Boolean(_) => "boolean",
            Value::Integer(_) | Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Table(_) => "table",
            Value::Function(_) => "function",
            Value::Null | Value::Handle(_) | Value::Other(_) => "userdata",
        }
    }

    /// Nil or null.
    pub fn is_nil(&self) -> bool {
        matches!(self, Value::Nil | Value::Null)
    }

    /// The string, if it is one and valid UTF-8.
    pub fn as_str(&self) -> Option<&'c str> {
        match self {
            Value::String(s) => std::str::from_utf8(s).ok(),
            _ => None,
        }
    }

    /// Converts, as an argument would be.
    pub fn get<T: FromLua<'c>>(self) -> Result<T> {
        T::from_lua(self)
    }
}

/// A Lua table, by reference: changes are visible to the template.
#[derive(Clone, Copy)]
pub struct Table<'c> {
    pub(crate) call: Call<'c>,
    pub(crate) r: RcRef,
}

impl<'c> Table<'c> {
    /// `t[key]`, honouring metamethods.
    pub fn get<V: FromLua<'c>>(&self, key: impl IntoLua) -> Result<V> {
        let key = key.into_lua(&self.call)?;
        let key = key.raw()?;
        let mut out = RcValue::nil();
        check(unsafe { (self.call.host().table_get)(self.call.raw(), self.r, &key, &mut out) })?;
        V::from_lua(Value::from_raw(self.call, &out))
    }

    /// `t[key] = value`, honouring metamethods.
    pub fn set(&self, key: impl IntoLua, value: impl IntoLua) -> Result<()> {
        let key = key.into_lua(&self.call)?;
        let value = value.into_lua(&self.call)?;
        let (key, value) = (key.raw()?, value.raw()?);
        check(unsafe { (self.call.host().table_set)(self.call.raw(), self.r, &key, &value) })
    }

    /// `#t`.
    pub fn len(&self) -> Result<usize> {
        let mut n = 0;
        check(unsafe { (self.call.host().table_len)(self.call.raw(), self.r, &mut n) })?;
        Ok(n)
    }

    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// `t[#t + 1] = value`.
    pub fn push(&self, value: impl IntoLua) -> Result<()> {
        self.set(self.len()? + 1, value)
    }

    /// Every pair, in `next` order.
    pub fn pairs<K: FromLua<'c>, V: FromLua<'c>>(&self) -> Pairs<'c, K, V> {
        Pairs {
            table: *self,
            key: Some(RcValue::nil()),
            _kv: PhantomData,
        }
    }

    /// `t[1]` to `t[#t]`.
    pub fn sequence<V: FromLua<'c>>(&self) -> Sequence<'c, V> {
        Sequence {
            table: *self,
            next: 1,
            len: None,
            _v: PhantomData,
        }
    }

    /// Keeps the table for the rest of the request.
    pub fn retain(&self) -> Result<Retained<Table<'static>>> {
        Retained::new(self.call, self.r)
    }
}

pub struct Pairs<'c, K, V> {
    table: Table<'c>,
    /// None once the walk has ended.
    key: Option<RcValue>,
    _kv: PhantomData<fn() -> (K, V)>,
}

impl<'c, K: FromLua<'c>, V: FromLua<'c>> Iterator for Pairs<'c, K, V> {
    type Item = Result<(K, V)>;

    fn next(&mut self) -> Option<Self::Item> {
        let key = self.key.as_mut()?;
        let mut value = RcValue::nil();
        let call = self.table.call;
        let status = unsafe { (call.host().table_next)(call.raw(), self.table.r, key, &mut value) };
        if let Err(e) = check(status) {
            self.key = None;
            return Some(Err(e));
        }
        if key.r#type == RcType::RC_NIL {
            self.key = None;
            return None;
        }
        let pair = K::from_lua(Value::from_raw(call, key))
            .and_then(|k| Ok((k, V::from_lua(Value::from_raw(call, &value))?)));
        Some(pair)
    }
}

pub struct Sequence<'c, V> {
    table: Table<'c>,
    next: usize,
    len: Option<usize>,
    _v: PhantomData<fn() -> V>,
}

impl<'c, V: FromLua<'c>> Iterator for Sequence<'c, V> {
    type Item = Result<V>;

    fn next(&mut self) -> Option<Self::Item> {
        let len = match self.len {
            Some(len) => len,
            None => match self.table.len() {
                Ok(len) => *self.len.insert(len),
                Err(e) => {
                    self.len = Some(0);
                    return Some(Err(e));
                }
            },
        };
        if self.next > len {
            return None;
        }
        let i = self.next;
        self.next += 1;
        Some(
            self.table
                .get(i)
                .map_err(|e| e.context(format_args!("[{i}]"))),
        )
    }
}

/// A Lua function, by reference.
#[derive(Clone, Copy)]
pub struct Function<'c> {
    pub(crate) call: Call<'c>,
    pub(crate) r: RcRef,
}

impl<'c> Function<'c> {
    /// Calls it with `args` (a value, a tuple, `()` or a [`Variadic`]). An
    /// error inside comes back as an [`Error`](crate::Error) that, returned
    /// with `?`, reaches Lua as the original error.
    pub fn call<R: FromLuaMulti<'c>>(&self, args: impl IntoLuaMulti) -> Result<R> {
        let mut lowered = Vec::new();
        args.into_lua_multi(&self.call, &mut lowered)?;
        let raw = lowered
            .iter()
            .map(|v| v.raw())
            .collect::<Result<Vec<_>>>()?;
        let mut results = vec![RcValue::nil(); R::MAX_RESULTS];
        let mut n = 0;
        check(unsafe {
            (self.call.host().call)(
                self.call.raw(),
                self.r,
                raw.as_ptr(),
                raw.len(),
                results.as_mut_ptr(),
                results.len(),
                &mut n,
            )
        })?;
        let values: Vec<Value<'c>> = results[..n]
            .iter()
            .map(|v| Value::from_raw(self.call, v))
            .collect();
        R::from_lua_multi(&values)
    }

    /// Keeps the function for the rest of the request.
    pub fn retain(&self) -> Result<Retained<Function<'static>>> {
        Retained::new(self.call, self.r)
    }
}

/// Any other Lua value, by reference; only good for passing back.
#[derive(Clone, Copy)]
pub struct AnyRef<'c> {
    pub(crate) call: Call<'c>,
    pub(crate) r: RcRef,
}

impl AnyRef<'_> {
    pub fn retain(&self) -> Result<Retained<AnyRef<'static>>> {
        Retained::new(self.call, self.r)
    }
}

/// An object of one of this plugin's `#[handle]` types, type unknown.
#[derive(Clone, Copy)]
pub struct AnyHandle<'c> {
    pub(crate) raw: RcHandleRef,
    _call: PhantomData<&'c ()>,
}

impl<'c> AnyHandle<'c> {
    pub fn downcast<T: Handle>(&self) -> Option<&'c T> {
        (self.raw.type_id == T::handle_type())
            // SAFETY: the type id says the host got this pointer from
            // `Box<T>`, and Lua keeps it alive while the call runs.
            .then(|| unsafe { &*(self.raw.ptr as *const c_void as *const T) })
    }
}

/// A reference kept past its call, until the request ends or it is released.
/// The host refuses it, rather than crashing, once the request is over.
pub struct Retained<T: Reference> {
    r: RcRef,
    _t: PhantomData<fn() -> T>,
}

impl<T: Reference> Retained<T> {
    fn new(call: Call<'_>, r: RcRef) -> Result<Self> {
        check(unsafe { (call.host().retain)(call.raw(), r) })?;
        Ok(Retained { r, _t: PhantomData })
    }

    /// The value again, in a later call.
    pub fn get<'c>(&self, call: &Call<'c>) -> T::At<'c> {
        T::at(*call, self.r)
    }

    pub fn release(self, call: &Call<'_>) -> Result<()> {
        check(unsafe { (call.host().release)(call.raw(), self.r) })
    }
}

impl<T: Reference> Clone for Retained<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: Reference> Copy for Retained<T> {}

/// The reference types a [`Retained`] can hold.
pub trait Reference: sealed::Sealed {
    type At<'c>;
    #[doc(hidden)]
    fn at<'c>(call: Call<'c>, r: RcRef) -> Self::At<'c>;
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for super::Table<'static> {}
    impl Sealed for super::Function<'static> {}
    impl Sealed for super::AnyRef<'static> {}
}

macro_rules! reference {
    ($($ty:ident),*) => {$(
        impl Reference for $ty<'static> {
            type At<'c> = $ty<'c>;
            fn at<'c>(call: Call<'c>, r: RcRef) -> $ty<'c> {
                $ty { call, r }
            }
        }

        impl fmt::Debug for $ty<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($ty), self.r.id)
            }
        }
    )*};
}

reference!(Table, Function, AnyRef);

impl fmt::Debug for AnyHandle<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AnyHandle({}, {:p})", self.raw.type_id, self.raw.ptr)
    }
}

/// Any number of values: as the last parameter it takes the remaining
/// arguments, as a return type it returns them all.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Variadic<T>(pub Vec<T>);

impl<T> Deref for Variadic<T> {
    type Target = Vec<T>;
    fn deref(&self) -> &Vec<T> {
        &self.0
    }
}

impl<T> DerefMut for Variadic<T> {
    fn deref_mut(&mut self) -> &mut Vec<T> {
        &mut self.0
    }
}

impl<T> FromIterator<T> for Variadic<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Variadic(iter.into_iter().collect())
    }
}

/// An owned byte string. `Vec<u8>` converts to a table of numbers, like any
/// `Vec`; this converts to a Lua string.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Bytes(pub Vec<u8>);

impl Deref for Bytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl From<Vec<u8>> for Bytes {
    fn from(v: Vec<u8>) -> Self {
        Bytes(v)
    }
}
