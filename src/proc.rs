// SPDX-License-Identifier: GPL-3.0-or-later
//! Shell-free helpers: executable lookup, pid handling and the control FIFO.

use std::ffi::CString;
use std::fs::File;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
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

/// A process identity: the pid *and* the kernel start time that makes it
/// unambiguous.
///
/// A pid alone is not an identity: after the process dies the number is reused,
/// so a stale `pid` file could point at an unrelated program. The start time
/// (field 22 of `/proc/<pid>/stat`) is per-boot unique and changes on reuse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pid {
    pub pid: i32,
    pub start: u64,
}

impl Pid {
    /// Identify `pid` if it currently exists.
    pub fn of(pid: i32) -> Option<Self> {
        Some(Self {
            pid,
            start: pid_starttime(pid)?,
        })
    }

    /// Identify the calling process.
    pub fn current() -> Option<Self> {
        Self::of(std::process::id() as i32)
    }
}

/// Kernel start time of `pid` (clock ticks since boot), or `None` when it is
/// gone. The `comm` field is parenthesised and may itself contain `)`; parse
/// from the last `)` to stay correct.
fn pid_starttime(pid: i32) -> Option<u64> {
    if pid <= 0 {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_starttime(&stat)
}

/// Extract field 22 from a `/proc/<pid>/stat` line.
fn parse_starttime(stat: &str) -> Option<u64> {
    let rest = stat.rsplit_once(')')?.1;
    // After `comm`, field 3 (state) comes first; starttime is field 22.
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// Whether this exact process (not a pid-reusing successor) is still running.
pub fn alive(pid: Pid) -> bool {
    pid_starttime(pid.pid) == Some(pid.start)
}

/// Signal a process only if it is still the one that was recorded.
///
/// A `pidfd` pins the process across the identity check, closing the race where
/// the target dies and its pid is reused between `read_pid_file` and `kill`.
/// Kernels without `pidfd_open` fall back to `kill` after re-checking the start
/// time (with the unavoidable small race).
pub fn send_signal(pid: Pid, sig: i32) {
    match pidfd_open(pid.pid) {
        Some(fd) => {
            if alive(pid) {
                unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        fd,
                        sig,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    );
                }
            }
            unsafe {
                libc::close(fd);
            }
        }
        None => {
            if alive(pid) {
                unsafe {
                    libc::kill(pid.pid, sig);
                }
            }
        }
    }
}

fn pidfd_open(pid: i32) -> Option<i32> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        None
    } else {
        Some(fd as i32)
    }
}

/// Write `"<pid> <starttime>"` atomically.
pub fn write_pid_file(path: &Path, pid: Pid) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, format!("{} {}\n", pid.pid, pid.start))?;
    std::fs::rename(&tmp, path)
}

/// Read a pid file and verify that the recorded process still exists.
///
/// A bare pid (legacy files, and r-modals' own pid file format) is accepted
/// only when the process belongs to us and its executable looks like q-bw,
/// which keeps a stale file from aiming a signal at an unrelated program.
pub fn read_pid_file(path: &Path) -> Option<Pid> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut fields = text.split_whitespace();
    let pid: i32 = fields.next()?.parse().ok()?;
    let start = pid_starttime(pid)?;

    match fields.next() {
        Some(expected) => {
            let expected: u64 = expected.parse().ok()?;
            (start == expected).then_some(Pid { pid, start })
        }
        None if legacy_pid_is_ours(pid) => Some(Pid { pid, start }),
        None => None,
    }
}

fn legacy_pid_is_ours(pid: i32) -> bool {
    let owned = std::fs::metadata(format!("/proc/{pid}"))
        .map(|md| md.uid() == unsafe { libc::geteuid() })
        .unwrap_or(false);
    owned && pid_exe_matches_ours(pid)
}

fn pid_exe_matches_ours(pid: i32) -> bool {
    let Ok(exe) = std::fs::read_link(format!("/proc/{pid}/exe")) else {
        return false;
    };
    let name = exe.file_name().unwrap_or_default().to_string_lossy();
    let name = name.strip_suffix(" (deleted)").unwrap_or(&name);

    if let Some(current) = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
    {
        if current == name {
            return true;
        }
    }
    // An upgrade replaces the binary while the old daemon still runs; its exe
    // link then shows "<name> (deleted)" (handled above) or the old name.
    name.starts_with("quick-bitwarden") || name.starts_with("quick_bitwarden")
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
    use super::{
        alive, parse_starttime, read_menu_state, read_pid_file, write_menu_state, write_pid_file,
        Pid,
    };

    #[test]
    fn pid_file_round_trips_and_rejects_a_reused_pid() {
        let dir = std::env::temp_dir().join(format!("qbw-pid-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("pid");
        let me = Pid::current().expect("the test process exists");

        assert!(alive(me));
        write_pid_file(&path, me).unwrap();
        assert_eq!(read_pid_file(&path), Some(me));

        // Same pid, different start time: the number was reused.
        std::fs::write(&path, format!("{} {}\n", me.pid, me.start + 1)).unwrap();
        assert_eq!(read_pid_file(&path), None);

        // A bare pid (legacy format) is accepted for our own process only.
        std::fs::write(&path, format!("{}\n", me.pid)).unwrap();
        assert_eq!(read_pid_file(&path), Some(me));

        std::fs::write(&path, "0 1\n").unwrap();
        assert_eq!(read_pid_file(&path), None);
        std::fs::write(&path, "not a pid\n").unwrap();
        assert_eq!(read_pid_file(&path), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn starttime_is_parsed_after_a_parenthesised_comm() {
        // `comm` may contain spaces and parentheses; parse from the last `)`.
        let stat = "42 (a)b) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 777";
        assert_eq!(parse_starttime(stat), Some(777));
        assert_eq!(parse_starttime("garbage"), None);
    }

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
