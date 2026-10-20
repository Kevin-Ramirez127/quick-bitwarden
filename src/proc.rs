// SPDX-License-Identifier: GPL-3.0-or-later
//! Shell-free helpers: executable lookup, pid handling and the control FIFO.

use std::ffi::CString;
use std::fs::File;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// `command -v`: resolve `cmd` on `PATH` to an executable file.
pub fn which(cmd: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(cmd);
        if let Ok(md) = std::fs::metadata(&cand) {
            if md.is_file() && md.permissions().mode() & 0o111 != 0 {
                return Some(cand);
            }
        }
    }
    None
}

/// `command -v "$1" >/dev/null 2>&1`.
pub fn command_exists(cmd: &str) -> bool {
    which(cmd).is_some()
}

/// `kill -0 "$pid"`.
pub fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Best-effort `kill`.
pub fn send_signal(pid: i32, sig: i32) {
    if pid > 0 {
        unsafe {
            libc::kill(pid, sig);
        }
    }
}

/// Read a pid from a pid file, tolerating a trailing newline.
pub fn read_pid(path: &Path) -> Option<i32> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
}

/// `[[ -p "$path" ]]`.
pub fn is_fifo(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.file_type().is_fifo())
        .unwrap_or(false)
}

/// `date +%s`.
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// An exclusive advisory lock held for as long as the value lives.
///
/// `flock(2)` locks are released by the kernel when the process exits (even on
/// `SIGKILL`), so a crashed daemon never leaves a stale lock behind.
pub struct FileLock {
    _file: File,
}

fn open_lock_file(path: &Path) -> Option<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .ok()
}

/// `flock(LOCK_EX | LOCK_NB)`: `Some` when the lock was acquired, `None` when
/// another process already holds it.
pub fn try_lock(path: &Path) -> Option<FileLock> {
    let file = open_lock_file(path)?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        Some(FileLock { _file: file })
    } else {
        None
    }
}

/// Blocking `flock(LOCK_EX)`: waits until the lock is free.
pub fn lock(path: &Path) -> Option<FileLock> {
    let file = open_lock_file(path)?;
    loop {
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc == 0 {
            return Some(FileLock { _file: file });
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return None;
        }
    }
}

/// Read the daemon's menu state: `"<generation> <busy>"`.
///
/// A missing or malformed file reads as `(0, false)`.
pub fn read_menu_state(path: &Path) -> (u64, bool) {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut parts = text.split_whitespace();
    let generation = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let busy = parts.next().map(|s| s == "1").unwrap_or(false);
    (generation, busy)
}

/// Write `"<generation> <busy>"` atomically.
///
/// The write goes to a sibling file and is `rename(2)`d into place, so a
/// concurrent reader always sees a consistent pair — never the new generation
/// with a stale `busy` flag (which would let a duplicate `show` through).
pub fn write_menu_state(path: &Path, generation: u64, busy: bool) {
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, format!("{generation} {}\n", u8::from(busy))).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// NUL-terminated path for the `libc` calls that need one.
pub fn cstr(path: &Path) -> CString {
    CString::new(path.as_os_str().as_bytes()).unwrap_or_else(|_| CString::new("/").unwrap())
}

#[cfg(test)]
mod tests {
    use super::{read_menu_state, write_menu_state};

    #[test]
    fn menu_state_round_trips_through_the_atomic_file() {
        let dir = std::env::temp_dir().join(format!("qbw-menu-state-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("gen");
        let _ = std::fs::remove_file(&path);

        // Missing file reads as idle generation 0.
        assert_eq!(read_menu_state(&path), (0, false));

        write_menu_state(&path, 3, true);
        assert_eq!(read_menu_state(&path), (3, true));

        write_menu_state(&path, 4, false);
        assert_eq!(read_menu_state(&path), (4, false));

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(dir.join("gen.tmp"));
        let _ = std::fs::remove_dir(&dir);
    }
}
