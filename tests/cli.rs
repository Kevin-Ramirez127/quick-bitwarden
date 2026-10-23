// SPDX-License-Identifier: GPL-3.0-or-later
//! End-to-end checks for the internal helper modes.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

fn temp_dir(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("qbw-cli-{name}-{}-{nanos}", std::process::id()))
}

fn write_fake_tool(bin: &Path, name: &str, body: &str) {
    std::fs::create_dir_all(bin).unwrap();
    let path = bin.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Make sure a failed assertion cannot leave a daemon running.
struct KillOnDrop(Option<Child>);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn daemon_sigterm_cleans_up_and_exits_zero() {
    let dir = temp_dir("daemon-term");
    let base = dir.join("run");
    let bin = dir.join("bin");
    write_fake_tool(&bin, "bw", "exit 1");
    std::fs::create_dir_all(&base).unwrap();

    let child = Command::new(env!("CARGO_BIN_EXE_quick-bitwarden"))
        .arg("--daemon")
        .env("QBW_BASE", &base)
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .spawn()
        .expect("daemon starts");
    let mut guard = KillOnDrop(Some(child));

    let pid_file = base.join("pid");
    let ctl = base.join("ctl");
    for _ in 0..200 {
        if pid_file.exists() && ctl.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        pid_file.exists() && ctl.exists(),
        "daemon did not become ready"
    );

    let recorded = std::fs::read_to_string(&pid_file).unwrap();
    assert!(
        recorded.split_whitespace().count() >= 2,
        "pid file must record a start time: {recorded:?}"
    );

    let daemon = guard.0.as_mut().unwrap();
    assert_eq!(unsafe { libc::kill(daemon.id() as i32, libc::SIGTERM) }, 0);

    let mut status = None;
    for _ in 0..500 {
        if let Some(s) = daemon.try_wait().unwrap() {
            status = Some(s);
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let status = status.expect("daemon ignored SIGTERM");
    assert!(status.success(), "clean shutdown expected, got {status:?}");

    assert!(!pid_file.exists(), "pid file left behind");
    assert!(!ctl.exists(), "control fifo left behind");

    // Reap it so the guard does not try to kill a reused pid.
    let _ = guard.0.take();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn clear_clipboard_helper_clears_without_the_daemon() {
    let dir = temp_dir("clear");
    let bin = dir.join("bin");
    let log = dir.join("calls");
    write_fake_tool(&bin, "wl-copy", "printf '%s' \"$*\" >> \"$FAKE_LOG\"");

    let status = Command::new(env!("CARGO_BIN_EXE_quick-bitwarden"))
        .args(["--clear-clipboard", "0"])
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("FAKE_LOG", &log)
        .status()
        .expect("helper starts");

    assert!(status.success(), "helper must exit cleanly");
    assert_eq!(
        std::fs::read_to_string(&log).expect("fake wl-copy was called"),
        "-c",
        "the helper must clear the clipboard"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
