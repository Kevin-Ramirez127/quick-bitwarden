// SPDX-License-Identifier: GPL-3.0-or-later
//! The view layer, built on [`r_modals`].
//!
//! The daemon never opens a window itself: every dialog runs in a short-lived
//! child process (`--menu`, `--ask`, `--error`) so it can be killed from the
//! outside, which keeps `hide` and `toggle` working.

use std::io::{BufRead, Write};
use std::process::{Child, ChildStdin, Command, Stdio};

use r_modals::{Confirm, Modal, ModalOption, Prompt};

use crate::paths::Paths;
use crate::proc::command_exists;

/// Shown in the picker window until `bw list items` has produced a menu.
const LOADING_MESSAGE: &str = "Loading your data from bw…";

// ---------------------------------------------------------------------------
// Parent side: spawn the child dialogs and collect their result.
// ---------------------------------------------------------------------------

/// A spawned `--menu` child whose items may not be known yet.
///
/// The window opens immediately in its loading state; [`MenuSession::send_items`]
/// delivers the menu (and closes stdin), then [`MenuSession::wait`] collects the
/// selection.
pub struct MenuSession {
    child: Child,
    stdin: Option<ChildStdin>,
    pid: i32,
}

/// `start_menu`: open the picker window without waiting for vault data.
pub fn start_menu(paths: &Paths) -> Option<MenuSession> {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => {
            error_dialog(paths, "Could not locate the quick-bitwarden executable");
            return None;
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
            return None;
        }
    };

    // `> "$SEL_FILE"` in the original: truncate up front so a cancel leaves an
    // empty selection rather than a stale one.
    let _ = std::fs::write(&paths.sel, b"");

    let pid = child.id() as i32;
    let _ = std::fs::write(&paths.menu_pid, format!("{pid}\n"));

    let stdin = child.stdin.take();
    Some(MenuSession { child, stdin, pid })
}

impl MenuSession {
    /// Hand the picker its labels and close stdin, which tells the child the
    /// list is complete.
    pub fn send_items(&mut self, menu: &str) {
        if let Some(mut stdin) = self.stdin.take() {
            let _ = stdin.write_all(menu.as_bytes());
            let _ = stdin.write_all(b"\n");
        }
    }

    /// Close the window early (e.g. to show the password prompt instead) and
    /// reap the child.
    pub fn cancel(mut self, paths: &Paths) {
        drop(self.stdin.take());
        crate::proc::send_signal(self.pid, libc::SIGTERM);
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&paths.menu_pid);
    }

    /// Wait for the pick (or cancel) and write the chosen label to `sel`.
    /// Returns `false` when the window could not be shown.
    pub fn wait(mut self, paths: &Paths) -> bool {
        drop(self.stdin.take());
        let out = self.child.wait_with_output();
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
}

/// `launch_menu`: show the item picker with `menu` (one label per line) and,
/// on a pick, drop the chosen label into `$BASE/sel`.
///
/// Returns `false` when the window could not be shown.
pub fn launch_menu(paths: &Paths, menu: &str) -> bool {
    let Some(mut session) = start_menu(paths) else {
        return false;
    };
    session.send_items(menu);
    session.wait(paths)
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

/// `notify-send -t 3000 "quick-bitwarden" <message>`.
pub fn notify(message: &str) {
    if command_exists("notify-send") {
        let _ = Command::new("notify-send")
            .args(["-t", "3000", "quick-bitwarden", message])
            .status();
    }
}

/// `notify-send -t 3000 ...` after a successful copy.
pub fn notify_copied(label: &str, clear_after: u64) {
    notify(&format!("Copied: {label} (clears in {clear_after}s)"));
}

/// Materialise `$BASE/prefixes.toml` from the embedded template, substituting
/// `{qbw}` with the running executable.
///
/// An existing file is preserved, but any built-in prefixes added by a newer
/// version are appended, so upgrades pick up `>lock` without losing local
/// customisations.
pub fn write_default_prefixes(paths: &Paths) {
    let path = paths.prefixes();
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "quick-bitwarden".to_string());
    let qbw = shell_quote(&exe);
    let template = include_str!("../prefixes.toml");

    if !path.exists() {
        let _ = std::fs::write(&path, template.replace("{qbw}", &qbw));
        return;
    }

    let Ok(mut content) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(specs) = r_modals::parse_prefixes(&content) else {
        // A malformed file is the user's to fix (the picker reports it too);
        // do not make it worse.
        return;
    };
    let mut names: Vec<String> = specs.into_iter().map(|s| s.prefix).collect();
    let mut changed = false;
    for (name, block) in embedded_prefix_blocks() {
        if names.iter().any(|n| n == name) {
            continue;
        }
        if !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str("\n[[prefix]]");
        content.push_str(&block.replace("{qbw}", &qbw));
        names.push(name.to_string());
        changed = true;
    }
    if changed {
        let _ = std::fs::write(&path, content);
    }
}

/// `[[prefix]]` blocks of the embedded template, paired with their prefix.
fn embedded_prefix_blocks() -> Vec<(&'static str, &'static str)> {
    include_str!("../prefixes.toml")
        .split("[[prefix]]")
        .skip(1)
        .filter_map(|block| {
            let name = block
                .lines()
                .find_map(|line| line.trim().strip_prefix("prefix = \""))?
                .strip_suffix('"')?;
            Some((name, block))
        })
        .collect()
}

/// Single-quote a word for `sh -c`.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
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
/// The option `id` is the short label (the name without the trailing
/// `  (username)`), so a hidden list can still reveal `Name  (user)` when the
/// user types `Name` exactly. The full label remains the visible text and the
/// value printed on selection. Free-typed input is allowed, so a name absent
/// from the list can still be forwarded to `bw get password`.
pub fn run_menu_child(paths: &Paths) -> i32 {
    // Read labels off the daemon in the background so the window can open and
    // show its loading message before `bw list items` has finished.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut options = Vec::new();
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            let line = line.strip_suffix('\r').unwrap_or(&line);
            if line.is_empty() {
                continue;
            }
            let label = line.split('\t').next().unwrap_or(line);
            let id = label.split("  (").next().unwrap_or(label);
            options.push(ModalOption::new(id, label));
        }
        let _ = tx.send(options);
    });

    let mut modal = Modal::new("Search")
        .placeholder("Search")
        .case_sensitive(false)
        .app_id("quick-bitwarden")
        .allow_custom_input(true)
        .footer_hint("Enter copy · Esc cancel")
        .loading_message(LOADING_MESSAGE);

    // Privacy mode (`>hide items`): keep the whole vault out of the default
    // view. A 3+ character query filters normally; shorter queries only reveal
    // an item typed in full.
    if paths.items_hidden() {
        modal = modal
            .min_match_chars(3)
            .placeholder("Type 3+ characters")
            .empty_message("No matches — type 3+ characters or an exact name");
    }

    // `>…` query prefixes (e.g. `>s` → sync), materialised by the daemon.
    let prefixes = paths.prefixes();
    if prefixes.is_file() {
        modal = modal.prefixes_file(prefixes);
    }

    match modal.run_streaming(rx) {
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
///
/// The `Prompt` window is borderless, so the label must be rendered in the body
/// (`.message`) and as the field hint (`.placeholder`) — the window title alone
/// is never shown.
pub fn run_ask_child(label: &str) -> i32 {
    match Prompt::new(label)
        .message(label)
        .placeholder(label)
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

#[cfg(test)]
mod tests {
    use super::{shell_quote, write_default_prefixes};
    use crate::paths::Paths;

    fn paths_in(base: &std::path::Path) -> Paths {
        Paths {
            base: base.to_path_buf(),
            session: base.join("s"),
            ctl: base.join("ctl"),
            pid: base.join("pid"),
            menu_pid: base.join("menu.pid"),
            sel: base.join("sel"),
            clear_after: 20,
            refresh_interval: 30,
        }
    }

    #[test]
    fn shell_quote_wraps_and_escapes() {
        assert_eq!(shell_quote("/usr/bin/qbw"), "'/usr/bin/qbw'");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn embedded_prefixes_template_parses() {
        let content = include_str!("../prefixes.toml").replace("{qbw}", "'/bin/quick-bitwarden'");
        let specs = r_modals::parse_prefixes(&content).expect("template parses");
        let prefixes: Vec<&str> = specs.iter().map(|s| s.prefix.as_str()).collect();
        assert_eq!(prefixes, [">sync", ">lock", ">hide items", ">show items"]);
        assert!(specs
            .iter()
            .all(|s| s.shell.as_deref().unwrap().contains("/bin/quick-bitwarden")));
        assert!(specs
            .iter()
            .all(|s| !s.shell.as_deref().unwrap().contains("{qbw}")));
    }

    #[test]
    fn existing_prefix_files_gain_new_builtins() {
        let base = std::env::temp_dir().join(format!("qbw-prefixes-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&base);
        let paths = paths_in(&base);
        let _ = std::fs::remove_file(paths.prefixes());

        write_default_prefixes(&paths);
        let content = std::fs::read_to_string(paths.prefixes()).unwrap();
        let names: Vec<String> = r_modals::parse_prefixes(&content)
            .unwrap()
            .into_iter()
            .map(|s| s.prefix)
            .collect();
        assert!(names.contains(&">lock".to_string()));
        assert!(names.contains(&">hide items".to_string()));
        assert!(names.contains(&">show items".to_string()));

        // Simulate a file written by an older version: only sync. Re-running
        // must append the missing built-ins exactly once.
        std::fs::write(
            paths.prefixes(),
            "[[prefix]]\nprefix = \">sync\"\nshell = \"true\"\nhint = \"Sync\"\n",
        )
        .unwrap();
        write_default_prefixes(&paths);
        let content = std::fs::read_to_string(paths.prefixes()).unwrap();
        let specs = r_modals::parse_prefixes(&content).unwrap();
        let names: Vec<&str> = specs.iter().map(|s| s.prefix.as_str()).collect();
        assert_eq!(names.iter().filter(|n| **n == ">sync").count(), 1);
        assert!(names.contains(&">lock"));
        assert!(names.contains(&">hide items"));

        let _ = std::fs::remove_dir_all(&base);
    }
}
