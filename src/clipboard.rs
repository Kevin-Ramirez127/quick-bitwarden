// SPDX-License-Identifier: GPL-3.0-or-later
//! Clipboard writing/clearing and `cliphist` hygiene.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::proc::command_exists;

/// `copy_password`: pipe the password into the first available clipboard tool.
/// Returns `false` when no tool exists.
pub fn copy_password(pw: &str) -> bool {
    let (program, args): (&str, &[&str]) = if command_exists("wl-copy") {
        ("wl-copy", &[])
    } else if command_exists("xclip") {
        ("xclip", &["-selection", "clipboard"])
    } else if command_exists("xsel") {
        ("xsel", &["--clipboard", "--input"])
    } else {
        return false;
    };

    let mut child = match Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(pw.as_bytes());
    }
    let _ = child.wait();
    true
}

/// `clear_password`: wait, then wipe the current clipboard.
pub fn clear_password(clear_after: u64) {
    std::thread::sleep(Duration::from_secs(clear_after));
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

/// `remove_from_history`: keep the password out of `cliphist`, retrying because
/// the clipboard watcher can store it a moment after we copy.
pub fn remove_from_history(pw: &str) {
    if !command_exists("cliphist") {
        return;
    }
    for _ in 0..3 {
        if let Ok(out) = Command::new("cliphist").arg("list").output() {
            let listing = String::from_utf8_lossy(&out.stdout);
            let matching: Vec<&str> = listing.lines().filter(|l| l.contains(pw)).collect();
            if !matching.is_empty() {
                if let Ok(mut child) = Command::new("cliphist")
                    .arg("delete")
                    .stdin(Stdio::piped())
                    .spawn()
                {
                    if let Some(mut stdin) = child.stdin.take() {
                        let payload = format!("{}\n", matching.join("\n"));
                        let _ = stdin.write_all(payload.as_bytes());
                    }
                    let _ = child.wait();
                }
            }
        }
        std::thread::sleep(Duration::from_millis(700));
    }
}
