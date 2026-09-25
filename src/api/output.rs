//! Writing the page: the raw `_out` and `_out_expr` C functions, `_out_raw`,
//! `print`, and HTML escaping.

use std::os::raw::c_int;

use mlua::{Lua, MultiValue, Value};

use super::{CURRENT_OUT, value_to_string, with_state};

/// _out, using a raw C function rather than a Rust function improves callback
/// speed drastically.
/// # Safety
/// Registered only via `create_c_function`, so Lua guarantees a valid state and
/// at least one argument slot. It must never panic, because unwinding across
/// the C boundary is UB, so it does nothing that can: no borrow that could
/// conflict, no fallible conversion, and no Lua error raised. Appending can
/// abort on allocation failure, which is a clean abort rather than an unwind.
///
/// Aliasing: the pointer is only ever dereferenced from inside a Lua call, and
/// no safe path holds a `borrow_mut` of the buffer across anything that can
/// re-enter Lua, so the two never overlap.
unsafe extern "C-unwind" fn lua_out(state: *mut mlua::lua_State) -> c_int {
    let Ok(out) = CURRENT_OUT.try_with(|c| c.get()) else {
        return 0;
    };
    if out.is_null() {
        return 0;
    }
    let mut len: usize = 0;
    let ptr = unsafe { mlua::ffi::lua_tolstring(state, 1, &mut len) };
    if ptr.is_null() {
        return 0;
    }
    unsafe {
        let bytes = std::slice::from_raw_parts(ptr as *const u8, len);
        (*out).extend_from_slice(bytes);
    }
    0
}

/// Where the output functions other than `_out` write: the same buffer the raw
/// `_out` appends to, so ordering holds without coordination. Callers derive
/// their bytes first, so nothing here can re-enter Lua and alias the buffer.
pub(super) fn emit(bytes: &[u8]) -> mlua::Result<()> {
    with_state(|st| {
        st.out.borrow_mut().extend_from_slice(bytes);
        Ok(())
    })
}

/// Escapes into the buffer rather than into a temporary, which is what makes
/// an interpolation allocation-free.
fn emit_escaped(bytes: &[u8]) -> mlua::Result<()> {
    with_state(|st| {
        escape_into(bytes, &mut st.out.borrow_mut());
        Ok(())
    })
}

/// Registry key for the Lua closure that renders what `lua_tolstring` cannot.
/// NUL-terminated, so the raw function hands it straight to `lua_getfield`.
const OUT_EXPR_SLOW: &str = "rc_out_expr_slow\0";

/// `<?lua= expr ?>`, raw for the same reason as [`lua_out`]: it runs once per
/// interpolation, and an mlua callback's dispatch costs more than the escaping.
///
/// # Safety
/// As `lua_out`, and it must not panic for the same reason. Strings, numbers,
/// nil and booleans are rendered here; anything else goes to [`out_expr_slow`],
/// the only step that can raise -- and a raise longjmps, so nothing with a
/// destructor is live across it.
unsafe extern "C-unwind" fn lua_out_expr(state: *mut mlua::lua_State) -> c_int {
    let Ok(out) = CURRENT_OUT.try_with(|c| c.get()) else {
        return 0;
    };
    if out.is_null() {
        return 0;
    }
    use std::fmt::Write as _;

    let top = unsafe { mlua::ffi::lua_gettop(state) };
    for index in 1..=top {
        // SAFETY (every arm): `out` addresses this request's buffer, which is a
        // separate allocation from anything Lua owns, so reading a Lua string
        // while appending to it cannot overlap.
        match unsafe { mlua::ffi::lua_type(state, index) } {
            mlua::ffi::LUA_TNIL => {}
            mlua::ffi::LUA_TBOOLEAN => {
                let word: &[u8] = if unsafe { mlua::ffi::lua_toboolean(state, index) } != 0 {
                    b"true"
                } else {
                    b"false"
                };
                unsafe { (*out).extend_from_slice(word) };
            }
            mlua::ffi::LUA_TSTRING => {
                let mut len: usize = 0;
                // Already a string, so this converts nothing and cannot raise.
                let ptr = unsafe { mlua::ffi::lua_tolstring(state, index, &mut len) };
                if !ptr.is_null() {
                    let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
                    escape_into(bytes, unsafe { &mut *out });
                }
            }
            // Formatted by Rust, not Lua: `tostring(5.0)` is "5.0" on 5.4 and
            // "5" on LuaJIT, and a template must not see which backend it runs
            // on. `lua_isinteger` classifies exactly as mlua's own conversion.
            mlua::ffi::LUA_TNUMBER => {
                let mut buf = StackBuf::new();
                let written = if unsafe { mlua::ffi::lua_isinteger(state, index) } != 0 {
                    write!(buf, "{}", unsafe { mlua::ffi::lua_tointeger(state, index) })
                } else {
                    write!(buf, "{}", unsafe { mlua::ffi::lua_tonumber(state, index) })
                };
                if written.is_ok() {
                    unsafe { (*out).extend_from_slice(buf.as_bytes()) };
                }
            }
            _ => unsafe { out_expr_slow(state, index) },
        }
    }
    0
}

/// A `write!` target that cannot allocate or panic, so formatting a number
/// leaves nothing with a destructor live where Lua might longjmp.
struct StackBuf {
    buf: [u8; 40],
    len: usize,
}

impl StackBuf {
    fn new() -> Self {
        StackBuf {
            buf: [0; 40],
            len: 0,
        }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

impl std::fmt::Write for StackBuf {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let end = self.len.checked_add(s.len()).ok_or(std::fmt::Error)?;
        let slot = self.buf.get_mut(self.len..end).ok_or(std::fmt::Error)?;
        slot.copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

/// Hands one value to the Lua closure under [`OUT_EXPR_SLOW`]. Skipped if that
/// is missing, which means a state whose request API was never installed.
unsafe fn out_expr_slow(state: *mut mlua::lua_State, index: c_int) {
    unsafe {
        if mlua::ffi::lua_checkstack(state, 2) == 0 {
            return;
        }
        mlua::ffi::lua_getfield(
            state,
            mlua::ffi::LUA_REGISTRYINDEX,
            OUT_EXPR_SLOW.as_ptr() as *const std::os::raw::c_char,
        );
        if mlua::ffi::lua_type(state, -1) != mlua::ffi::LUA_TFUNCTION {
            mlua::ffi::lua_pop(state, 1);
            return;
        }
        mlua::ffi::lua_pushvalue(state, index);
        mlua::ffi::lua_call(state, 1, 0);
    }
}

/// A Lua-side buffer was tried on JIT backends and removed: it wins on a page
/// of literal HTML, but every `<?lua= ?>` then costs a Rust callback *plus* a
/// call back into Lua to reach the buffer. On a 1000-row template that measured
/// 1,442 rps against 2,820 for writing straight through.
pub(super) fn install(lua: &Lua) -> mlua::Result<()> {
    let globals = lua.globals();

    // SAFETY: see `lua_out`. Its buffer comes from the thread-local that
    // `RequestScope` in `render` keeps pointed at this request's state.
    globals.set("_out", unsafe { lua.create_c_function(lua_out)? })?;

    // <?lua= expr ?>. Escapes by default; nil prints nothing. The slow path is
    // registered first, because the raw function looks it up by registry key.
    lua.set_named_registry_value(
        &OUT_EXPR_SLOW[..OUT_EXPR_SLOW.len() - 1],
        lua.create_function(|lua, value: Value| {
            emit_escaped(value_to_string(lua, value)?.as_bytes())
        })?,
    )?;
    // SAFETY: see `lua_out_expr`. Same buffer and same thread-local as `_out`.
    globals.set("_out_expr", unsafe { lua.create_c_function(lua_out_expr)? })?;

    // <?lua== expr ?>: unescaped, for templates deliberately emitting markup.
    globals.set(
        "_out_raw",
        lua.create_function(|lua, values: MultiValue| {
            for value in values {
                match value {
                    Value::Nil => {}
                    Value::String(s) => emit(&s.as_bytes())?,
                    other => emit(value_to_string(lua, other)?.as_bytes())?,
                }
            }
            Ok(())
        })?,
    )?;

    // Joins arguments with a space. Unescaped: it is how a template emits
    // markup it assembled itself.
    globals.set(
        "print",
        lua.create_function(|lua, params: MultiValue| {
            for (i, value) in params.into_iter().enumerate() {
                if i > 0 {
                    emit(b" ")?;
                }
                match value {
                    Value::String(s) => emit(&s.as_bytes())?,
                    other => emit(value_to_string(lua, other)?.as_bytes())?,
                }
            }
            Ok(())
        })?,
    )?;

    Ok(())
}

/// `html_escape`, which background threads get too.
pub(super) fn install_html_escape(lua: &Lua) -> mlua::Result<()> {
    lua.globals().set(
        "html_escape",
        lua.create_function(|lua, input: mlua::String| {
            if first_escapable(&input.as_bytes()).is_none() {
                return Ok(input);
            }
            lua.create_string(escape_bytes(&input.as_bytes()))
        })?,
    )?;
    Ok(())
}

/// Position of the first character needing an entity. Kept free of side effects
/// so it stays a pure scan the optimiser can vectorise -- the overwhelmingly
/// common answer is `None`, and that case must cost no more than the copy.
#[inline]
fn first_escapable(bytes: &[u8]) -> Option<usize> {
    bytes
        .iter()
        .position(|b| matches!(b, b'&' | b'<' | b'>' | b'"' | b'\''))
}

/// Appends the escaped form of `bytes`: one scan and one copy when there is
/// nothing to escape, otherwise run by run rather than byte by byte.
fn escape_into(bytes: &[u8], out: &mut Vec<u8>) {
    let Some(first) = first_escapable(bytes) else {
        out.extend_from_slice(bytes);
        return;
    };
    out.reserve(bytes.len() + 16);
    // From 0, not `first`: the run copied at the first replacement is the
    // unescaped prefix leading up to it.
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate().skip(first) {
        let replacement: &[u8] = match b {
            b'&' => b"&amp;",
            b'<' => b"&lt;",
            b'>' => b"&gt;",
            b'"' => b"&quot;",
            b'\'' => b"&#x27;",
            _ => continue,
        };
        out.extend_from_slice(&bytes[start..i]);
        out.extend_from_slice(replacement);
        start = i + 1;
    }
    out.extend_from_slice(&bytes[start..]);
}

fn escape_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    escape_into(bytes, &mut out);
    out
}

pub fn html_escape(input: &str) -> String {
    if first_escapable(input.as_bytes()).is_none() {
        return input.to_string();
    }
    // escape_bytes only ever replaces ASCII with ASCII, so UTF-8 is preserved.
    String::from_utf8(escape_bytes(input.as_bytes())).expect("escaping preserves UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_html_entities() {
        assert_eq!(
            html_escape(r#"<b class="x">&'</b>"#),
            "&lt;b class=&quot;x&quot;&gt;&amp;&#x27;&lt;/b&gt;"
        );
    }

    /// `escape_into` copies the runs between replacements, so where the first
    /// one falls decides which runs exist at all.
    #[test]
    fn escapes_every_run_layout() {
        for (input, want) in [
            ("", ""),
            ("plain text", "plain text"),
            ("<lead", "&lt;lead"),
            ("trail>", "trail&gt;"),
            ("mid&dle", "mid&amp;dle"),
            ("a<<b", "a&lt;&lt;b"),
            ("<>&\"'", "&lt;&gt;&amp;&quot;&#x27;"),
            ("pre<mid>post", "pre&lt;mid&gt;post"),
        ] {
            assert_eq!(html_escape(input), want, "input {input:?}");
        }
    }

    #[test]
    fn escaping_preserves_non_ascii() {
        assert_eq!(html_escape("héllo <wörld>"), "héllo &lt;wörld&gt;");
    }
}
