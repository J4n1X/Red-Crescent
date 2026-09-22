//! Request path → file inside the serve directory.
//!
//! `canonicalize` costs a `readlink` per path component, and was the hottest
//! function in a profile of a trivial page. On Linux a single `openat2` with
//! `RESOLVE_NO_SYMLINKS` makes the same guarantee in the kernel: with no `..`
//! and no symlink anywhere on it, the lexical path *is* the canonical one. A
//! path that does cross a symlink falls back to `canonicalize`, which stays
//! authoritative.

use std::fs::{File, Metadata};
use std::path::{Path, PathBuf};

pub enum Resolved {
    Found(Found),
    Missing,
    /// Canonical path, which lies outside the root.
    Escaped(PathBuf),
}

pub struct Found {
    /// Canonical.
    pub path: PathBuf,
    /// Set when resolving already produced it.
    pub meta: Option<Metadata>,
    /// The file itself, when `read` asked for it and resolving could open it
    /// on the way — the descriptor that was checked, not a second lookup.
    pub file: Option<File>,
}

/// Resolve `rel` under the canonical `root`; a directory to its `index` when
/// one is given. `read` says, by final path, whether to open it for reading.
pub fn resolve(
    root: &Path,
    rel: &str,
    index: Option<&str>,
    read: impl Fn(&Path) -> bool,
) -> Resolved {
    let target = root.join(rel);
    #[cfg(target_os = "linux")]
    if let Some(resolved) = linux::resolve(root, &target, index, read) {
        return resolved;
    }
    #[cfg(not(target_os = "linux"))]
    let _ = read;
    resolve_canonical(root, target, index)
}

fn resolve_canonical(root: &Path, mut target: PathBuf, index: Option<&str>) -> Resolved {
    if let Some(index) = index
        && target.is_dir()
    {
        target.push(index);
    }
    match target.canonicalize() {
        Ok(path) if path.starts_with(root) => Resolved::Found(Found {
            path,
            meta: None,
            file: None,
        }),
        Ok(path) => Resolved::Escaped(path),
        Err(_) => Resolved::Missing,
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::CString;
    use std::fs::File;
    use std::io;
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Component, Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::{Found, Resolved};

    /// Cleared once the kernel (< 5.6) or a seccomp filter refuses `openat2`.
    static AVAILABLE: AtomicBool = AtomicBool::new(true);

    /// `None` hands the path to `canonicalize`: a symlink was crossed, or
    /// the answer is not a plain one.
    pub(super) fn resolve(
        root: &Path,
        target: &Path,
        index: Option<&str>,
        read: impl Fn(&Path) -> bool,
    ) -> Option<Resolved> {
        if !AVAILABLE.load(Ordering::Relaxed) {
            return None;
        }
        // The kernel gets the path as written: a trailing `/` must still
        // demand a directory.
        let mut path = lexical(target)?;
        let mut file = match open(target, read(&path)) {
            Ok(file) => file,
            Err(e) => return missing(e),
        };
        let mut meta = file.metadata().ok()?;
        if let Some(index) = index
            && meta.is_dir()
        {
            let target = target.join(index);
            path = lexical(&target)?;
            file = match open(&target, read(&path)) {
                Ok(file) => file,
                Err(e) => return missing(e),
            };
            meta = file.metadata().ok()?;
        }
        // `target` may be absolute, or `index`; let the slow path refuse them.
        if !meta.is_file() || !beneath(&path, root) {
            return None;
        }
        let file = read(&path).then_some(file);
        Some(Resolved::Found(Found {
            path,
            meta: Some(meta),
            file,
        }))
    }

    /// `path` without `.` and repeated separators, or `None` if it has a `..`.
    /// Checked on bytes first: `components()` showed up in the profile.
    fn lexical(path: &Path) -> Option<PathBuf> {
        let plain = path
            .as_os_str()
            .as_bytes()
            .strip_prefix(b"/")
            .is_some_and(|rel| {
                rel.split(|&b| b == b'/')
                    .all(|seg| !matches!(seg, b"" | b"." | b".."))
            });
        if plain {
            return Some(path.to_path_buf());
        }
        path.components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
            .then(|| path.components().collect())
    }

    /// `Path::starts_with` for normalized paths, without splitting both.
    fn beneath(path: &Path, root: &Path) -> bool {
        let root = root.as_os_str().as_bytes();
        path.as_os_str()
            .as_bytes()
            .strip_prefix(root)
            .is_some_and(|rest| rest.first() == Some(&b'/') || root.ends_with(b"/"))
    }

    fn missing(e: io::Error) -> Option<Resolved> {
        match e.raw_os_error() {
            Some(libc::ENOENT | libc::ENOTDIR) => Some(Resolved::Missing),
            Some(libc::ENOSYS | libc::EPERM) => {
                AVAILABLE.store(false, Ordering::Relaxed);
                None
            }
            _ => None,
        }
    }

    /// Open `path`, refusing to follow a symlink in any component. Without
    /// `read`, `O_PATH`: nothing is really opened, so no device side effects.
    /// `O_NONBLOCK` so a FIFO cannot hang the open.
    fn open(path: &Path, read: bool) -> io::Result<File> {
        let c_path = CString::new(path.as_os_str().as_bytes())?;
        // `openat2` rejects flags it would ignore, so `O_PATH` comes alone.
        let flags = if read {
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOCTTY
        } else {
            libc::O_PATH
        };
        // SAFETY: plain integers; all-zero is the valid default.
        let mut how: libc::open_how = unsafe { std::mem::zeroed() };
        how.flags = (flags | libc::O_CLOEXEC) as u64;
        how.resolve = libc::RESOLVE_NO_SYMLINKS;
        // SAFETY: both pointers outlive the call, and `size` is `how`'s.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                libc::AT_FDCWD,
                c_path.as_ptr(),
                &how as *const libc::open_how,
                std::mem::size_of::<libc::open_how>(),
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh descriptor nothing else owns.
        Ok(unsafe { File::from_raw_fd(fd as libc::c_int) })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn site() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/index.lhtml"), "i").unwrap();
        std::fs::write(root.join("a.txt"), "a").unwrap();
        std::os::unix::fs::symlink("a.txt", root.join("link.txt")).unwrap();
        std::os::unix::fs::symlink("/etc", root.join("etc")).unwrap();
        (dir, root)
    }

    /// (canonical path, whether the fast path produced it)
    fn found(r: Resolved) -> (PathBuf, bool) {
        match r {
            Resolved::Found(f) => (f.path, f.meta.is_some()),
            Resolved::Missing => panic!("missing"),
            Resolved::Escaped(abs) => panic!("escaped to {}", abs.display()),
        }
    }

    /// The slow path gives the same answers, so it would hide a fast path that
    /// silently stopped working.
    #[test]
    fn plain_paths_take_the_fast_path_on_linux() {
        let (_dir, root) = site();
        let fast = cfg!(target_os = "linux");
        let index = Some("index.lhtml");
        assert_eq!(
            found(resolve(&root, "a.txt", index, |_| false)),
            (root.join("a.txt"), fast)
        );
        assert_eq!(
            found(resolve(&root, "./a.txt", index, |_| false)),
            (root.join("a.txt"), fast)
        );
        assert_eq!(
            found(resolve(&root, "sub//", index, |_| false)),
            (root.join("sub/index.lhtml"), fast)
        );
    }

    #[test]
    fn read_asks_for_the_file_by_its_final_path() {
        use std::io::Read;
        let (_dir, root) = site();
        let read = |p: &Path| p.ends_with("a.txt");
        let Resolved::Found(found) = resolve(&root, "a.txt", None, read) else {
            panic!("a.txt not found");
        };
        if cfg!(target_os = "linux") {
            let mut text = String::new();
            found.file.unwrap().read_to_string(&mut text).unwrap();
            assert_eq!(text, "a");
        }
        let Resolved::Found(found) = resolve(&root, "sub", Some("index.lhtml"), read) else {
            panic!("sub not found");
        };
        assert!(found.file.is_none());
    }

    #[test]
    fn symlinks_resolve_to_their_canonical_target() {
        let (_dir, root) = site();
        assert_eq!(
            found(resolve(&root, "link.txt", None, |_| false)),
            (root.join("a.txt"), false)
        );
        assert!(matches!(
            resolve(&root, "etc/passwd", None, |_| false),
            Resolved::Escaped(_)
        ));
    }

    #[test]
    fn an_absolute_index_cannot_leave_the_root() {
        let (_dir, root) = site();
        assert!(matches!(
            resolve(&root, "sub", Some("/etc/passwd"), |_| false),
            Resolved::Escaped(_)
        ));
    }

    #[test]
    fn missing_paths_and_files_used_as_directories_are_missing() {
        let (_dir, root) = site();
        for rel in ["nope", "sub/nope", "a.txt/", "a.txt/x", ""] {
            assert!(
                matches!(
                    resolve(&root, rel, Some("index.lhtml"), |_| true),
                    Resolved::Missing
                ),
                "rel: {rel}"
            );
        }
    }

    #[test]
    fn without_an_index_a_directory_is_found_as_itself() {
        let (_dir, root) = site();
        assert_eq!(
            found(resolve(&root, "sub", None, |_| false)).0,
            root.join("sub")
        );
    }
}
