// SPDX-License-Identifier: GPL-3.0-or-later
//! Shell-free helpers: executable lookup, pid handling and the control FIFO.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
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

/// NUL-terminated path for the `libc` calls that need one.
pub fn cstr(path: &Path) -> CString {
    CString::new(path.as_os_str().as_bytes()).unwrap_or_else(|_| CString::new("/").unwrap())
}
