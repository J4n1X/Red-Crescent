//! The Lua-facing `process` module: run a program with an argument vector.
//!
//! No shell is involved, so quoting stops being a concept for callers and a
//! filename can never be read as syntax. Unlike `os.execute` the call is also
//! bounded: the instruction hook cannot fire while Lua waits on a child, so
//! the timeout here is the only thing that can end a hung one.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use mlua::{Lua, Table, Value};

/// Used when a call does not ask for one.
const DEFAULT_TIMEOUT_SECS: f64 = 60.0;
const MAX_TIMEOUT_SECS: f64 = 86400.0;

/// Captured stdout/stderr are held in memory, so they are bounded. Output past
/// this is dropped while the pipe keeps draining, since a child that blocks on
/// a full pipe would never exit.
const MAX_CAPTURE: usize = 8 * 1024 * 1024;

/// Polling backs off from this to [`POLL_MAX`]: a command that exits in a
/// millisecond should not be billed a fixed sleep, and a long one should not
/// spin.
const POLL_MIN: Duration = Duration::from_micros(50);
const POLL_MAX: Duration = Duration::from_millis(5);

pub fn register(lua: &Lua) -> mlua::Result<()> {
    let process = lua.create_table()?;
    process.set("run", lua.create_function(run)?)?;
    lua.globals().set("process", &process)?;
    lua.register_module("process", process)?;
    Ok(())
}

fn drain(mut source: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        let mut buf = [0u8; 8192];
        while let Ok(n) = source.read(&mut buf) {
            if n == 0 {
                break;
            }
            if kept.len() < MAX_CAPTURE {
                let room = MAX_CAPTURE - kept.len();
                kept.extend_from_slice(&buf[..n.min(room)]);
            }
        }
        kept
    })
}

fn run(lua: &Lua, spec: Table) -> mlua::Result<Table> {
    let mut argv: Vec<String> = Vec::new();
    for value in spec.clone().sequence_values::<String>() {
        argv.push(value?);
    }
    let Some((program, args)) = argv.split_first() else {
        return Err(mlua::Error::runtime("process.run: needs a program to run"));
    };

    let timeout = spec
        .get::<Option<f64>>("timeout")?
        .unwrap_or(DEFAULT_TIMEOUT_SECS);
    if !timeout.is_finite() || !(0.0..=MAX_TIMEOUT_SECS).contains(&timeout) {
        return Err(mlua::Error::runtime(format!(
            "process.run: timeout must be in 0..={MAX_TIMEOUT_SECS}"
        )));
    }
    let capture = spec.get::<Option<bool>>("capture")?.unwrap_or(true);

    let mut command = Command::new(program);
    command.args(args);
    command.stdin(Stdio::null());
    if let Some(cwd) = spec.get::<Option<String>>("cwd")? {
        command.current_dir(PathBuf::from(cwd));
    }
    command.stdout(if capture {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    command.stderr(if capture {
        Stdio::piped()
    } else {
        Stdio::null()
    });

    let result = lua.create_table()?;
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            // Never started: a missing program is a different answer from one
            // that ran and failed, and callers use it to detect availability.
            result.set("ok", false)?;
            result.set("started", false)?;
            result.set("error", format!("{program}: {e}"))?;
            return Ok(result);
        }
    };

    // Pipes are drained on their own threads: a child that fills one while the
    // parent waits on exit would deadlock.
    let out = child.stdout.take().map(drain);
    let err = child.stderr.take().map(drain);

    let deadline = Instant::now() + Duration::from_secs_f64(timeout);
    let mut timed_out = false;
    let mut wait = POLL_MIN;
    let status = loop {
        match child.try_wait()? {
            Some(status) => break Some(status),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                timed_out = true;
                break None;
            }
            None => {
                std::thread::sleep(wait);
                wait = (wait * 2).min(POLL_MAX);
            }
        }
    };

    let text = |handle: Option<std::thread::JoinHandle<Vec<u8>>>| {
        handle
            .and_then(|h| h.join().ok())
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default()
    };

    result.set("started", true)?;
    result.set("timed_out", timed_out)?;
    result.set("ok", status.is_some_and(|s| s.success()))?;
    match status.and_then(|s| s.code()) {
        Some(code) => result.set("code", code)?,
        None => result.set("code", Value::Nil)?,
    }
    result.set("stdout", text(out))?;
    result.set("stderr", text(err))?;
    Ok(result)
}
