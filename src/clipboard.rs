// SPDX-License-Identifier: GPL-3.0-or-later
//! Clipboard writing/clearing and `cliphist` hygiene.

use std::io::Write;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::proc::command_exists;

/// `copy_password`: pipe the password into the first available clipboard tool.
/// Returns `false` when no tool exists.
///
/// On Wayland, `wl-copy --sensitive` marks the selection so watchers such as
/// cliphist never store it. Older wl-clipboard versions reject the flag, so fall
/// back to a plain copy (and rely on [`remove_from_history`]).
pub fn copy_password(pw: &str) -> bool {
    if command_exists("wl-copy") {
        if write_clipboard("wl-copy", &["--sensitive"], pw) {
            return true;
        }
        return write_clipboard("wl-copy", &[], pw);
    }
    if command_exists("xclip") {
        return write_clipboard("xclip", &["-selection", "clipboard"], pw);
    }
    if command_exists("xsel") {
        return write_clipboard("xsel", &["--clipboard", "--input"], pw);
    }
    false
}

fn write_clipboard(program: &str, args: &[&str], data: &str) -> bool {
    let mut child = match Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(data.as_bytes());
    }
    child.wait().map(|status| status.success()).unwrap_or(false)
}

/// Upper bound for the clear delay: a typo in `QBW_CLEAR_AFTER` must not keep
/// the password on the clipboard for days.
pub const MAX_CLEAR_AFTER: u64 = 3600;

/// Clamp a user-supplied clear delay to [`MAX_CLEAR_AFTER`].
pub fn clamp_clear_after(secs: u64) -> u64 {
    secs.min(MAX_CLEAR_AFTER)
}

/// `clear_password`: wait, then wipe the current clipboard.
pub fn clear_password(clear_after: u64) {
    std::thread::sleep(Duration::from_secs(clamp_clear_after(clear_after)));
    if command_exists("wl-copy") {
        let _ = Command::new("wl-copy").arg("-c").status();
    } else if command_exists("xclip") {
        let _ = Command::new("xclip")
            .args(["-selection", "clipboard", "/dev/null"])
            .status();
    } else if command_exists("xsel") {
        if let Ok(f) = std::fs::File::open("/dev/null") {
            let _ = Command::new("xsel")
                .args(["--clipboard", "--input"])
                .stdin(Stdio::from(f))
                .status();
        }
    }
}

/// Schedule the clipboard clear in a detached helper process.
///
/// A thread inside the daemon dies with the daemon, so `stop`, `restart` or a
/// crash would leave the password on the clipboard forever. Re-exec this binary
/// in its own session and let that process sleep and clear. Returns `false` when
/// the helper could not be spawned (the caller can fall back to a thread).
pub fn schedule_clear(clear_after: u64) -> bool {
    if clear_after == 0 {
        clear_password(0);
        return true;
    }
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let mut cmd = Command::new(exe);
    cmd.arg("--clear-clipboard")
        .arg(clamp_clear_after(clear_after).to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // New session: a signal aimed at the daemon's process group (or its
    // terminal) does not take the pending clear down with it.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().is_ok()
}

/// How many of the newest history entries are decoded when looking for the
/// secret. A fresh copy is at the top; the window also covers a few entries
/// written by other watchers in between, without decoding the whole database.
const SCAN_LIMIT: usize = 20;

/// The clipboard watcher can store the selection a moment after we copy, so
/// retry a few times. Each attempt re-reads the newest ids (the list shifts).
const REMOVAL_ATTEMPTS: usize = 3;
const RETRY_DELAY: Duration = Duration::from_millis(600);

/// `remove_from_history`: keep the password out of `cliphist`.
///
/// The preview shown by `cliphist list` is truncated (100 characters) and has
/// its whitespace collapsed, so matching on it misses long or multi-line
/// passwords. Ask for the ids, decode each candidate and compare the *full*
/// payload byte for byte before deleting. The secret itself never enters an
/// argument vector: `decode` only receives the id.
pub fn remove_from_history(pw: &str) {
    if pw.is_empty() || !command_exists("cliphist") {
        return;
    }
    for attempt in 0..REMOVAL_ATTEMPTS {
        if let Some(ids) = matching_history_ids(pw) {
            if !ids.is_empty() {
                delete_history_entries(&ids);
                return;
            }
        }
        if attempt + 1 < REMOVAL_ATTEMPTS {
            std::thread::sleep(RETRY_DELAY);
        }
    }
}

/// IDs of the newest entries whose complete payload equals `pw`.
///
/// Returns `None` when `cliphist list` cannot be run at all.
fn matching_history_ids(pw: &str) -> Option<Vec<String>> {
    let listing = Command::new("cliphist")
        .args(["list", "-fields", "id"])
        .output()
        .ok()?;
    if !listing.status.success() {
        // cliphist before 0.5 has no `-fields`; fall back to the preview text.
        return preview_matching_ids(pw);
    }

    let mut ids = Vec::new();
    for id in String::from_utf8_lossy(&listing.stdout)
        .lines()
        .take(SCAN_LIMIT)
    {
        let id = id.trim();
        if id.is_empty() {
            continue;
        }
        let Ok(out) = Command::new("cliphist").arg("decode").arg(id).output() else {
            continue;
        };
        if out.status.success() && out.stdout == pw.as_bytes() {
            ids.push(id.to_string());
        }
    }
    Some(ids)
}

/// Best-effort fallback for cliphist versions without `-fields`: the preview is
/// lossy, so this can only delete entries that contain the secret verbatim.
fn preview_matching_ids(pw: &str) -> Option<Vec<String>> {
    let listing = Command::new("cliphist").arg("list").output().ok()?;
    if !listing.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&listing.stdout)
            .lines()
            .filter(|line| line.contains(pw))
            .filter_map(|line| line.split('\t').next().map(str::to_string))
            .take(SCAN_LIMIT)
            .collect(),
    )
}

fn delete_history_entries(ids: &[String]) {
    let Ok(mut child) = Command::new("cliphist")
        .arg("delete")
        .stdin(Stdio::piped())
        .spawn()
    else {
        return;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let mut payload = String::new();
        for id in ids {
            payload.push_str(id);
            payload.push('\n');
        }
        let _ = stdin.write_all(payload.as_bytes());
    }
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::{clamp_clear_after, remove_from_history, MAX_CLEAR_AFTER};
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    /// `PATH` is process-global: serialise this test against any future test
    /// that spawns helper tools.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("qbw-{name}-{}-{nanos}", std::process::id()))
    }

    fn write_fake_cliphist(bin: &Path) {
        std::fs::create_dir_all(bin).unwrap();
        let script = bin.join("cliphist");
        std::fs::write(
            &script,
            "#!/bin/sh\n\
             case \"$1\" in\n\
             \x20 list) printf '1\\n2\\n' ;;\n\
             \x20 decode) cat \"$FAKE_DIR/$2\" ;;\n\
             \x20 delete) cat > \"$FAKE_DELETE\" ;;\n\
             esac\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn restore(name: &str, value: Option<OsString>) {
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
    }

    #[test]
    fn clear_delay_is_clamped() {
        assert_eq!(clamp_clear_after(20), 20);
        assert_eq!(clamp_clear_after(0), 0, "0 means clear immediately");
        assert_eq!(clamp_clear_after(MAX_CLEAR_AFTER + 1), MAX_CLEAR_AFTER);
        assert_eq!(clamp_clear_after(u64::MAX), MAX_CLEAR_AFTER);
    }

    #[test]
    fn history_scrub_compares_the_full_payload() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = temp_dir("cliphist");
        let bin = dir.join("bin");
        write_fake_cliphist(&bin);

        // Entry 1 merely contains the secret; entry 2 *is* a long, multi-line
        // secret whose 100-char preview would not contain the full text.
        std::fs::write(dir.join("1"), b"prefix-secret-suffix").unwrap();
        let secret = format!("{}\nsecond line", "s3cret ".repeat(30));
        std::fs::write(dir.join("2"), &secret).unwrap();
        let deleted = dir.join("deleted");

        let old_path = std::env::var_os("PATH");
        let old_dir = std::env::var_os("FAKE_DIR");
        let old_delete = std::env::var_os("FAKE_DELETE");
        std::env::set_var("PATH", format!("{}:/usr/bin:/bin", bin.display()));
        std::env::set_var("FAKE_DIR", &dir);
        std::env::set_var("FAKE_DELETE", &deleted);

        remove_from_history(&secret);

        restore("PATH", old_path);
        restore("FAKE_DIR", old_dir);
        restore("FAKE_DELETE", old_delete);

        let outcome = std::fs::read_to_string(&deleted).expect("one entry deleted");
        assert_eq!(outcome, "2\n", "only the exact full match is deleted");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
