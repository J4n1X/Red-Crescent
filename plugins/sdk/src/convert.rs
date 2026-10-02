//! Conversions between Rust and Lua values.
//!
//! Strings are `&str`, `String`, `&[u8]` and [`Bytes`]; `Vec<T>` and the
//! maps are tables. Numbers convert both ways between Lua's integers and
//! floats when no precision is lost. `Option<T>` is nil (or null) or a `T`.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;
use std::marker::PhantomData;

use crate::call::Call;
use crate::error::{Error, Result};
use crate::ffi::RcValue;
use crate::handle::{Handle, NewHandle};
use crate::value::{AnyHandle, AnyRef, Bytes, Function, Table, Value, Variadic};

/// A value converted for the host, opaque. Borrowed strings stay borrowed;
/// owned ones are kept here until the host has copied them. Implement
/// [`IntoLua`] by converting to a type that already does:
///
/// ```ignore
/// impl IntoLua for Celsius {
///     fn into_lua<'a>(self, call: &Call<'_>) -> Result<Lowered<'a>> {
///         self.0.into_lua(call)
///     }
/// }
/// ```
pub struct Lowered<'a>(Repr<'a>);

enum Repr<'a> {
    Raw(RcValue, PhantomData<&'a [u8]>),
    Owned(Box<[u8]>),
    Handle(NewHandle),
}

impl<'a> Lowered<'a> {
    fn value(raw: RcValue) -> Self {
        Lowered(Repr::Raw(raw, PhantomData))
    }

    fn owned(bytes: impl Into<Box<[u8]>>) -> Self {
        Lowered(Repr::Owned(bytes.into()))
    }

    #[doc(hidden)]
    pub fn handle<T: Handle>(value: T) -> Self {
        Lowered(Repr::Handle(NewHandle::new(value)))
    }

    /// Valid while `self` is.
    pub(crate) fn raw(&self) -> Result<RcValue> {
        match &self.0 {
            Repr::Raw(raw, _) => Ok(*raw),
            Repr::Owned(bytes) => Ok(RcValue::bytes(bytes)),
            Repr::Handle(_) => Err(Error::new(
                "a new handle can only be returned to Lua, not stored or passed to it",
            )),
        }
    }

    pub(crate) fn into_new_handle(self) -> std::result::Result<NewHandle, Self> {
        match self.0 {
            Repr::Handle(h) => Ok(h),
            other => Err(Lowered(other)),
        }
    }
}

/// Rust to Lua.
pub trait IntoLua {
    fn into_lua<'a>(self, call: &Call<'_>) -> Result<Lowered<'a>>
    where
        Self: 'a;
}

/// Lua to Rust. `'c` is the call's lifetime, so borrowed strings and
/// references cannot outlive it.
pub trait FromLua<'c>: Sized {
    fn from_lua(value: Value<'c>) -> Result<Self>;
}

/// Any number of values: a single value, `()`, a tuple or a [`Variadic`].
pub trait IntoLuaMulti {
    fn into_lua_multi<'a>(self, call: &Call<'_>, out: &mut Vec<Lowered<'a>>) -> Result<()>
    where
        Self: 'a;
}

/// What a Lua function call returns, read into Rust.
pub trait FromLuaMulti<'c>: Sized {
    /// How many results to ask for.
    const MAX_RESULTS: usize;
    fn from_lua_multi(values: &[Value<'c>]) -> Result<Self>;
}

pub(crate) fn mismatch(expected: &str, got: &Value<'_>) -> Error {
    Error::new(format!("expected {expected}, got {}", got.type_name()))
}

impl IntoLua for Value<'_> {
    fn into_lua<'a>(self, _: &Call<'_>) -> Result<Lowered<'a>>
    where
        Self: 'a,
    {
        Ok(Lowered::value(self.to_raw()))
    }
}

impl<'c> FromLua<'c> for Value<'c> {
    fn from_lua(value: Value<'c>) -> Result<Self> {
        Ok(value)
    }
}

macro_rules! by_value {
    ($($ty:ident => $variant:ident, $name:literal;)*) => {$(
        impl IntoLua for $ty<'_> {
            fn into_lua<'a>(self, _: &Call<'_>) -> Result<Lowered<'a>>
            where
                Self: 'a,
            {
                Ok(Lowered::value(Value::$variant(self).to_raw()))
            }
        }

        impl<'c> FromLua<'c> for $ty<'c> {
            fn from_lua(value: Value<'c>) -> Result<Self> {
                match value {
                    Value::$variant(v) => Ok(v),
                    other => Err(mismatch($name, &other)),
                }
            }
        }
    )*};
}

by_value! {
    Table => Table, "table";
    Function => Function, "function";
    AnyRef => Other, "userdata";
    AnyHandle => Handle, "a handle";
}

impl IntoLua for bool {
    fn into_lua<'a>(self, _: &Call<'_>) -> Result<Lowered<'a>> {
        Ok(Lowered::value(RcValue::boolean(self)))
    }
}

impl FromLua<'_> for bool {
    fn from_lua(value: Value<'_>) -> Result<Self> {
        match value {
            Value::Boolean(b) => Ok(b),
            other => Err(mismatch("boolean", &other)),
        }
    }
}

/// An integer from either Lua number subtype, if it is one exactly.
fn integer(value: &Value<'_>) -> Result<i64> {
    match *value {
        Value::Integer(i) => Ok(i),
        Value::Number(n) if n.fract() == 0.0 && n >= -(2f64.powi(63)) && n < 2f64.powi(63) => {
            Ok(n as i64)
        }
        Value::Number(_) => Err(Error::new("number has no integer representation")),
        ref other => Err(mismatch("integer", other)),
    }
}

macro_rules! integers {
    ($($ty:ty),*) => {$(
        impl IntoLua for $ty {
            fn into_lua<'a>(self, _: &Call<'_>) -> Result<Lowered<'a>> {
                let i = i64::try_from(self).map_err(|_| {
                    Error::new(format!("{self} does not fit in a Lua integer"))
                })?;
                Ok(Lowered::value(RcValue::int(i)))
            }
        }

        impl FromLua<'_> for $ty {
            fn from_lua(value: Value<'_>) -> Result<Self> {
                let i = integer(&value)?;
                <$ty>::try_from(i).map_err(|_| {
                    Error::new(format!("{i} is out of range for {}", stringify!($ty)))
                })
            }
        }
    )*};
}

integers!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize);

macro_rules! floats {
    ($($ty:ty),*) => {$(
        impl IntoLua for $ty {
            fn into_lua<'a>(self, _: &Call<'_>) -> Result<Lowered<'a>> {
                Ok(Lowered::value(RcValue::number(self as f64)))
            }
        }

        impl FromLua<'_> for $ty {
            fn from_lua(value: Value<'_>) -> Result<Self> {
                match value {
                    Value::Number(n) => Ok(n as $ty),
                    Value::Integer(i) => Ok(i as $ty),
                    other => Err(mismatch("number", &other)),
                }
            }
        }
    )*};
}

floats!(f32, f64);

impl IntoLua for &str {
    fn into_lua<'a>(self, _: &Call<'_>) -> Result<Lowered<'a>>
    where
        Self: 'a,
    {
        Ok(Lowered::value(RcValue::string(self)))
    }
}

impl IntoLua for &[u8] {
    fn into_lua<'a>(self, _: &Call<'_>) -> Result<Lowered<'a>>
    where
        Self: 'a,
    {
        Ok(Lowered::value(RcValue::bytes(self)))
    }
}

impl IntoLua for &String {
    fn into_lua<'a>(self, call: &Call<'_>) -> Result<Lowered<'a>>
    where
        Self: 'a,
    {
        self.as_str().into_lua(call)
    }
}

impl IntoLua for String {
    fn into_lua<'a>(self, _: &Call<'_>) -> Result<Lowered<'a>> {
        Ok(Lowered::owned(self.into_bytes()))
    }
}

impl IntoLua for Cow<'_, str> {
    fn into_lua<'a>(self, call: &Call<'_>) -> Result<Lowered<'a>>
    where
        Self: 'a,
    {
        match self {
            Cow::Borrowed(s) => s.into_lua(call),
            Cow::Owned(s) => s.into_lua(call),
        }
    }
}

impl IntoLua for Bytes {
    fn into_lua<'a>(self, _: &Call<'_>) -> Result<Lowered<'a>> {
        Ok(Lowered::owned(self.0))
    }
}

impl<'c> FromLua<'c> for &'c [u8] {
    fn from_lua(value: Value<'c>) -> Result<Self> {
        match value {
            Value::String(s) => Ok(s),
            other => Err(mismatch("string", &other)),
        }
    }
}

impl<'c> FromLua<'c> for &'c str {
    fn from_lua(value: Value<'c>) -> Result<Self> {
        let bytes = <&[u8]>::from_lua(value)?;
        std::str::from_utf8(bytes).map_err(|_| Error::new("string is not valid UTF-8"))
    }
}

impl FromLua<'_> for String {
    fn from_lua(value: Value<'_>) -> Result<Self> {
        <&str>::from_lua(value).map(str::to_owned)
    }
}

impl FromLua<'_> for Bytes {
    fn from_lua(value: Value<'_>) -> Result<Self> {
        <&[u8]>::from_lua(value).map(|b| Bytes(b.to_vec()))
    }
}

impl<T: IntoLua> IntoLua for Option<T> {
    fn into_lua<'a>(self, call: &Call<'_>) -> Result<Lowered<'a>>
    where
        Self: 'a,
    {
        match self {
            Some(v) => v.into_lua(call),
            None => Ok(Lowered::value(RcValue::nil())),
        }
    }
}

impl<'c, T: FromLua<'c>> FromLua<'c> for Option<T> {
    fn from_lua(value: Value<'c>) -> Result<Self> {
        match value {
            Value::Nil | Value::Null => Ok(None),
            v => T::from_lua(v).map(Some),
        }
    }
}

impl<T: IntoLua> IntoLua for Vec<T> {
    fn into_lua<'a>(self, call: &Call<'_>) -> Result<Lowered<'a>>
    where
        Self: 'a,
    {
        Ok(Lowered::value(
            Value::Table(call.sequence_from(self)?).to_raw(),
        ))
    }
}

impl<'c, T: FromLua<'c>> FromLua<'c> for Vec<T> {
    fn from_lua(value: Value<'c>) -> Result<Self> {
        Table::from_lua(value)?.sequence().collect()
    }
}

macro_rules! maps {
    ($($map:ident: $($bound:path),*;)*) => {$(
        impl<K: IntoLua, V: IntoLua> IntoLua for $map<K, V> {
            fn into_lua<'a>(self, call: &Call<'_>) -> Result<Lowered<'a>>
            where
                Self: 'a,
            {
                Ok(Lowered::value(Value::Table(call.table_from(self)?).to_raw()))
            }
        }

        impl<'c, K: FromLua<'c> $(+ $bound)*, V: FromLua<'c>> FromLua<'c> for $map<K, V> {
            fn from_lua(value: Value<'c>) -> Result<Self> {
                Table::from_lua(value)?.pairs().collect()
            }
        }
    )*};
}

maps! {
    BTreeMap: Ord;
    HashMap: Eq, Hash;
}

impl<T: IntoLua> IntoLuaMulti for T {
    fn into_lua_multi<'a>(self, call: &Call<'_>, out: &mut Vec<Lowered<'a>>) -> Result<()>
    where
        Self: 'a,
    {
        out.push(self.into_lua(call)?);
        Ok(())
    }
}

impl<'c, T: FromLua<'c>> FromLuaMulti<'c> for T {
    const MAX_RESULTS: usize = 1;
    fn from_lua_multi(values: &[Value<'c>]) -> Result<Self> {
        T::from_lua(values.first().copied().unwrap_or(Value::Nil))
    }
}

impl<T: IntoLua> IntoLuaMulti for Variadic<T> {
    fn into_lua_multi<'a>(self, call: &Call<'_>, out: &mut Vec<Lowered<'a>>) -> Result<()>
    where
        Self: 'a,
    {
        for v in self.0 {
            out.push(v.into_lua(call)?);
        }
        Ok(())
    }
}

impl<'c, T: FromLua<'c>> FromLuaMulti<'c> for Variadic<T> {
    /// A Lua function returning more than this has the rest cut off.
    const MAX_RESULTS: usize = 64;
    fn from_lua_multi(values: &[Value<'c>]) -> Result<Self> {
        values.iter().map(|v| T::from_lua(*v)).collect()
    }
}

impl IntoLuaMulti for () {
    fn into_lua_multi<'a>(self, _: &Call<'_>, _: &mut Vec<Lowered<'a>>) -> Result<()>
    where
        Self: 'a,
    {
        Ok(())
    }
}

impl FromLuaMulti<'_> for () {
    const MAX_RESULTS: usize = 0;
    fn from_lua_multi(_: &[Value<'_>]) -> Result<Self> {
        Ok(())
    }
}

macro_rules! tuples {
    ($(($($name:ident $i:tt),+))*) => {$(
        impl<$($name: IntoLua),+> IntoLuaMulti for ($($name,)+) {
            fn into_lua_multi<'a>(self, call: &Call<'_>, out: &mut Vec<Lowered<'a>>) -> Result<()>
            where
                Self: 'a,
            {
                $(out.push(self.$i.into_lua(call)?);)+
                Ok(())
            }
        }

        impl<'c, $($name: FromLua<'c>),+> FromLuaMulti<'c> for ($($name,)+) {
            const MAX_RESULTS: usize = [$($i),+].len();
            fn from_lua_multi(values: &[Value<'c>]) -> Result<Self> {
                Ok(($(
                    $name::from_lua(values.get($i).copied().unwrap_or(Value::Nil))
                        .map_err(|e| e.context(format_args!("result {}", $i + 1)))?,
                )+))
            }
        }
    )*};
}

// 12 is what the standard library amount for tuple conversions in things like
// Debug, so we'll do the same.
tuples! {
    (A 0)
    (A 0, B 1)
    (A 0, B 1, C 2)
    (A 0, B 1, C 2, D 3)
    (A 0, B 1, C 2, D 3, E 4)
    (A 0, B 1, C 2, D 3, E 4, F 5)
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6)
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7)
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8)
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9)
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K 10)
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K 10, L 11)
}
