// SPDX-License-Identifier: GPL-3.0-or-later
//! End-to-end checks for the internal helper modes.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

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
