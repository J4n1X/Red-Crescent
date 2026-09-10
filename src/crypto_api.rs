//! The Lua-facing `crypto` module: password hashing (argon2id), secure random
//! tokens, sha256, and constant-time comparison — the primitives an
//! authentication system needs and pure Lua cannot provide.

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use mlua::Lua;
use sha2::{Digest, Sha256};

const MAX_TOKEN_BYTES: usize = 1024;

pub fn register(lua: &Lua) -> mlua::Result<()> {
    let crypto = lua.create_table()?;

    crypto.set(
        "password_hash",
        lua.create_function(|_, password: String| {
            let hash = Argon2::default()
                .hash_password(password.as_bytes())
                .map_err(|e| mlua::Error::runtime(format!("password_hash failed: {e}")))?;
            Ok(hash.to_string())
        })?,
    )?;

    crypto.set(
        "password_verify",
        lua.create_function(|_, (password, hash): (String, String)| {
            let Ok(parsed) = PasswordHash::new(&hash) else {
                return Ok(false);
            };
            Ok(Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok())
        })?,
    )?;

    crypto.set(
        "random_token",
        lua.create_function(|_, n_bytes: Option<usize>| {
            let n = n_bytes.unwrap_or(32);
            if n == 0 || n > MAX_TOKEN_BYTES {
                return Err(mlua::Error::runtime(format!(
                    "random_token: byte count must be 1..={MAX_TOKEN_BYTES}"
                )));
            }
            let mut buf = vec![0u8; n];
            getrandom::fill(&mut buf)
                .map_err(|e| mlua::Error::runtime(format!("random_token failed: {e}")))?;
            Ok(hex_encode(&buf))
        })?,
    )?;

    crypto.set(
        "sha256",
        lua.create_function(|_, input: mlua::String| {
            Ok(hex_encode(&Sha256::digest(input.as_bytes())))
        })?,
    )?;

    crypto.set(
        "constant_time_equals",
        lua.create_function(|_, (a, b): (mlua::String, mlua::String)| {
            let a = a.as_bytes();
            let b = b.as_bytes();
            if a.len() != b.len() {
                return Ok(false);
            }
            let diff = a
                .iter()
                .zip(b.iter())
                .fold(0u8, |acc, (x, y)| acc | (x ^ y));
            Ok(diff == 0)
        })?,
    )?;

    lua.globals().set("crypto", &crypto)?;
    lua.register_module("crypto", crypto)?;
    Ok(())
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}
