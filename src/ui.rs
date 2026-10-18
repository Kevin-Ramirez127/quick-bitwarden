// SPDX-License-Identifier: GPL-3.0-or-later
//! The view layer, built on [`r_modals`].
//!
//! The daemon never opens a window itself: every dialog runs in a short-lived
//! child process (`--menu`, `--ask`, `--error`) so it can be killed from the
//! outside, which keeps `hide` and `toggle` working.

use std::io::{Read, Write};
use std::process::{Command, Stdio};

use r_modals::{Confirm, Modal, ModalOption, Prompt};

use crate::paths::Paths;
use crate::proc::command_exists;

// ---------------------------------------------------------------------------
// Parent side: spawn the child dialogs and collect their result.
// ---------------------------------------------------------------------------

/// `launch_menu`: show the item picker with `menu` (one label per line) and,
/// on a pick, drop the chosen label into `$BASE/sel`.
///
/// Returns `false` when the window could not be shown.
pub fn launch_menu(paths: &Paths, menu: &str) -> bool {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => {
            error_dialog(paths, "Could not locate the quick-bitwarden executable");
            return false;
        }
    };

    let mut child = match Command::new(exe)
        .arg("--menu")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => {
            error_dialog(paths, "Could not open the search window");
            return false;
        }
    };

    // `> "$SEL_FILE"` in the original: truncate up front so a cancel leaves an
    // empty selection rather than a stale one.
    let _ = std::fs::write(&paths.sel, b"");

    let pid = child.id() as i32;
    let _ = std::fs::write(&paths.menu_pid, format!("{pid}\n"));

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(menu.as_bytes());
        let _ = stdin.write_all(b"\n");
    }

    let out = child.wait_with_output();
    let _ = std::fs::remove_file(&paths.menu_pid);

    match out {
        Ok(out) => {
            if out.status.code() == Some(2) {
                let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
                if msg.is_empty() {
                    error_dialog(paths, "Could not open the search window");
                } else {
                    error_dialog(paths, &msg);
                }
                return false;
            }
            if out.status.success() {
                let _ = std::fs::write(&paths.sel, &out.stdout);
            }
            true
        }
        Err(_) => true,
    }
}

/// `ask_password`: prompt for the master password (masked) and return it, or
/// `None` when cancelled / unavailable.
pub fn ask_password(paths: &Paths, label: &str) -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let child = Command::new(exe)
        .arg("--ask")
        .arg(label)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;

    let pid = child.id() as i32;
    let _ = std::fs::write(&paths.menu_pid, format!("{pid}\n"));

    let out = child.wait_with_output();
    let _ = std::fs::remove_file(&paths.menu_pid);

    let out = out.ok()?;
    if !out.status.success() {
        return None;
    }
    let pw = String::from_utf8_lossy(&out.stdout)
        .trim_end_matches(['\n', '\r'])
        .to_string();
    if pw.is_empty() {
        None
    } else {
        Some(pw)
    }
}

/// `error_dialog`: show a message in an r-modals dialog. If that fails (no
/// display) fall back to `notify-send`, then to `$BASE/error.log`.
pub fn error_dialog(paths: &Paths, msg: &str) {
    if show_error_via_child(msg) {
        return;
    }
    if command_exists("notify-send") {
        let _ = Command::new("notify-send")
            .args(["-u", "critical", "quick-bitwarden", msg])
            .status();
    } else {
        let _ = std::fs::write(paths.error_log(), format!("quick-bitwarden: {msg}\n"));
    }
}

/// `notify-send -t 3000 ...` after a successful copy.
pub fn notify_copied(label: &str, clear_after: u64) {
    if command_exists("notify-send") {
        let _ = Command::new("notify-send")
            .args([
                "-t",
                "3000",
                "quick-bitwarden",
                &format!("Copied: {label} (clears in {clear_after}s)"),
            ])
            .status();
    }
}

fn show_error_via_child(msg: &str) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    match Command::new(exe).arg("--error").arg(msg).status() {
        Ok(status) => status.success(),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Child side: run one dialog to completion and report through the exit code.
// 0 = value printed · 1 = cancelled · 2 = backend error.
// ---------------------------------------------------------------------------

/// `--menu`: read `label<TAB>id` lines from stdin, pick one, print its label.
///
/// The option `id` is set to the label so search stays label-only. Free-typed
/// input is allowed, so a name absent from the list can still be forwarded to
/// `bw get password`.
pub fn run_menu_child() -> i32 {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        eprintln!("quick-bitwarden: cannot read items");
        return 2;
    }

    let options: Vec<ModalOption> = input
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .filter(|l| !l.is_empty())
        .map(|line| {
            let label = line.split('\t').next().unwrap_or(line);
            ModalOption::new(label, label)
        })
        .collect();

    if options.is_empty() {
        eprintln!("quick-bitwarden: no items to show");
        return 2;
    }

    let modal = Modal::new("Search")
        .placeholder("Search")
        .case_sensitive(false)
        .app_id("quick-bitwarden")
        .allow_custom_input(true)
        .footer_hint("Enter copy · Esc cancel")
        .options(options);

    match modal.run() {
        Ok(Some(sel)) => {
            print!("{}", sel.label);
            let _ = std::io::stdout().flush();
            0
        }
        Ok(None) => 1,
        Err(e) => {
            eprintln!("quick-bitwarden: {e}");
            2
        }
    }
}

/// `--ask <label>`: masked text prompt; prints the password on success.
pub fn run_ask_child(label: &str) -> i32 {
    match Prompt::new(label)
        .password(true)
        .app_id("quick-bitwarden")
        .run()
    {
        Ok(Some(pw)) if !pw.is_empty() => {
            print!("{pw}");
            let _ = std::io::stdout().flush();
            0
        }
        Ok(_) => 1,
        Err(e) => {
            eprintln!("quick-bitwarden: {e}");
            2
        }
    }
}

/// `--error <message>`: one-button info dialog.
pub fn run_error_child(msg: &str) -> i32 {
    match Confirm::new("quick-bitwarden")
        .message(msg)
        .confirm_label("OK")
        .cancel_label("Close")
        .app_id("quick-bitwarden")
        .run()
    {
        Ok(_) => 0,
        Err(e) => {
            eprintln!("quick-bitwarden: {e}");
            2
        }
    }
}
