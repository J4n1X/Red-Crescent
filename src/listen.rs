//! Sockets handed over by systemd socket activation, per sd_listen_fds(3).

use std::net::TcpListener;
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::net::UnixListener;

/// The first descriptor systemd passes; the protocol fixes it at 3.
const SD_LISTEN_FDS_START: RawFd = 3;

pub enum Inherited {
    Tcp(TcpListener),
    Unix(UnixListener),
}

/// The listening socket systemd passed in, or `None` when this process was
/// not socket-activated.
pub fn from_systemd() -> Result<Option<Inherited>, String> {
    // LISTEN_PID guards against a child inheriting the variables by accident.
    let for_us = std::env::var("LISTEN_PID")
        .ok()
        .and_then(|p| p.parse::<u32>().ok())
        == Some(std::process::id());
    if !for_us {
        return Ok(None);
    }
    let count: usize = std::env::var("LISTEN_FDS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    if count != 1 {
        return Err(format!(
            "systemd passed {count} sockets; exactly one ListenStream= is supported"
        ));
    }
    let fd = SD_LISTEN_FDS_START;

    // systemd hands the socket over without CLOEXEC, and process.run must not
    // leak it into every child.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err(format!(
            "inherited socket is not usable: {}",
            std::io::Error::last_os_error()
        ));
    }
    if sockopt(fd, libc::SO_TYPE)? != libc::SOCK_STREAM {
        return Err("inherited socket is not a stream socket; use ListenStream=".into());
    }
    if sockopt(fd, libc::SO_ACCEPTCONN)? == 0 {
        return Err("inherited socket is not listening; the socket unit needs Accept=no".into());
    }

    let mut addr: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of_val(&addr) as libc::socklen_t;
    if unsafe { libc::getsockname(fd, (&raw mut addr).cast(), &mut len) } == -1 {
        return Err(format!(
            "inherited socket is not usable: {}",
            std::io::Error::last_os_error()
        ));
    }
    // Ownership of fd 3 passes to the listener here; nothing else closes it.
    match addr.ss_family as libc::c_int {
        libc::AF_INET | libc::AF_INET6 => Ok(Some(Inherited::Tcp(unsafe {
            TcpListener::from_raw_fd(fd)
        }))),
        libc::AF_UNIX => Ok(Some(Inherited::Unix(unsafe {
            UnixListener::from_raw_fd(fd)
        }))),
        family => Err(format!("inherited socket has unsupported family {family}")),
    }
}

fn sockopt(fd: RawFd, name: libc::c_int) -> Result<libc::c_int, String> {
    let mut value: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            name,
            (&raw mut value).cast(),
            &mut len,
        )
    };
    if rc == -1 {
        return Err(format!(
            "inherited descriptor is not a socket: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(value)
}
