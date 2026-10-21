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

/// `notify-send -t 3000 ...` after copying something other than the password
/// (e.g. the username).
pub fn notify_copied_field(kind: &str, label: &str, clear_after: u64) {
    notify(&format!(
        "Copied {kind}: {label} (clears in {clear_after}s)"
    ));
}

/// Materialise `$BASE/config.toml` from the embedded template, substituting
/// `{qbw}` with the running executable.
///
/// A single `config.toml` holds both the query prefixes and the keyboard
/// shortcuts. An existing file is preserved, but any built-in entries added by
/// a newer version are appended (per section), so upgrades pick up `>lock`
/// without losing local customisations.
///
/// A `prefixes.toml`/`shortcuts.toml` from before the unification is migrated
/// into the new file the first time the daemon starts.
pub fn write_default_config(paths: &Paths) {
    let path = paths.config();
    let template = include_str!("../config.toml");
    let qbw = shell_quote(
        &std::env::current_exe()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "quick-bitwarden".to_string()),
    );

    if !path.exists() {
        match seed_legacy_config(paths) {
            Some((content, migrated)) => {
                if std::fs::write(&path, content).is_ok() {
                    for legacy in migrated {
                        let _ = std::fs::remove_file(legacy);
                    }
                }
            }
            None => {
                let _ = std::fs::write(&path, template.replace("{qbw}", &qbw));
            }
        }
    }

    append_missing_entries(&path, template, "[[prefix]]", "prefix", &qbw, |c| {
        r_modals::parse_prefixes(c)
            .ok()
            .map(|specs| specs.into_iter().map(|s| s.prefix).collect())
    });
    append_missing_entries(&path, template, "[[shortcut]]", "key", &qbw, |c| {
        r_modals::parse_shortcuts(c)
            .ok()
            .map(|specs| specs.into_iter().map(|s| s.key).collect())
    });
}

/// Content for a fresh `config.toml` built from pre-unification files, plus the
/// files to remove once it has been written. `None` when neither exists.
fn seed_legacy_config(paths: &Paths) -> Option<(String, Vec<std::path::PathBuf>)> {
    let mut content = String::new();
    let mut used = Vec::new();
    for legacy in [
        paths.base.join("prefixes.toml"),
        paths.base.join("shortcuts.toml"),
    ] {
        let Ok(text) = std::fs::read_to_string(&legacy) else {
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(&text);
        used.push(legacy);
    }
    (!content.is_empty()).then_some((content, used))
}

/// Append the built-in `[[section]]` entries that `path` lacks, keyed by
/// `name_key`. A file that cannot be parsed is left alone: it is the user's to
/// fix, and the picker reports it.
fn append_missing_entries(
    path: &std::path::Path,
    template: &str,
    section: &str,
    name_key: &str,
    qbw: &str,
    existing_names: impl Fn(&str) -> Option<Vec<String>>,
) {
    let Ok(mut content) = std::fs::read_to_string(path) else {
        return;
    };
    let Some(mut names) = existing_names(&content) else {
        return;
    };
    let mut changed = false;
    for (name, body) in embedded_blocks(template, section, name_key) {
        if names.contains(&name) {
            continue;
        }
        if !content.ends_with('\n') {
            content.push('\n');
        }
        content.push('\n');
        content.push_str(section);
        content.push('\n');
        content.push_str(&body.replace("{qbw}", qbw));
        names.push(name);
        changed = true;
    }
    if changed {
        let _ = std::fs::write(path, content);
    }
}

/// Entry bodies of `section` in `template`, paired with their `name_key` value.
///
/// Only a line that starts a table (`[[…`) ends a body, so a comment that
/// mentions a section name is never mistaken for one.
fn embedded_blocks(template: &str, section: &str, name_key: &str) -> Vec<(String, String)> {
    let needle = format!("{name_key} = \"");
    let mut out = Vec::new();
    let mut body: Option<String> = None;

    fn flush(body: &mut Option<String>, out: &mut Vec<(String, String)>, needle: &str) {
        let Some(text) = body.take() else {
            return;
        };
        if let Some(name) = text
            .lines()
            .find_map(|line| line.trim().strip_prefix(needle))
            .and_then(|value| value.strip_suffix('"'))
        {
            out.push((name.to_string(), format!("{}\n", text.trim_end())));
        }
    }

    for line in template.lines() {
        let start = line.trim_start();
        if start.starts_with("[[") {
            flush(&mut body, &mut out, &needle);
            if start == section {
                body = Some(String::new());
            }
            continue;
        }
        if let Some(text) = body.as_mut() {
            text.push_str(line);
            text.push('\n');
        }
    }
    flush(&mut body, &mut out, &needle);
    out
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
        .footer_hint("Enter copy · Shift+Enter username · Esc cancel")
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

    // `$BASE/config.toml` (materialised by the daemon) holds both the `>…`
    // query prefixes (e.g. `>s` → sync) and the keyboard shortcuts
    // (e.g. Shift+Enter → copy the username). Each parser ignores the other's
    // section, so one file feeds both.
    let config = paths.config();
    if config.is_file() {
        modal = modal.prefixes_file(config.clone()).shortcuts_file(config);
    }

    match modal.run_streaming(rx) {
        Ok(Some(sel)) => {
            // Line 1 is the shortcut tag (empty for a plain Enter/click); the
            // rest is the label, so the daemon knows what to copy.
            let tag = sel.shortcut.as_deref().unwrap_or("");
            print!("{tag}\n{}", sel.label);
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
    use super::{shell_quote, write_default_config};
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
    fn embedded_config_template_parses_both_sections() {
        let content = include_str!("../config.toml").replace("{qbw}", "'/bin/quick-bitwarden'");

        // One file: the prefix parser ignores `[[shortcut]]`, and vice versa.
        let prefixes = r_modals::parse_prefixes(&content).expect("prefixes parse");
        let names: Vec<&str> = prefixes.iter().map(|s| s.prefix.as_str()).collect();
        assert_eq!(names, [">sync", ">lock", ">hide items", ">show items"]);
        assert!(prefixes.iter().all(|s| s
            .shell
            .as_deref()
            .unwrap()
            .contains("/bin/quick-bitwarden")));
        assert!(prefixes
            .iter()
            .all(|s| !s.shell.as_deref().unwrap().contains("{qbw}")));

        let shortcuts = r_modals::parse_shortcuts(&content).expect("shortcuts parse");
        assert_eq!(shortcuts.len(), 1);
        assert_eq!(shortcuts[0].key, "Shift+Enter");
        assert_eq!(shortcuts[0].pick.as_deref(), Some("username"));
        assert!(shortcuts[0].option.is_none());
        assert!(shortcuts[0].command.is_none());
        assert!(shortcuts[0].shell.is_none());
    }

    #[test]
    fn existing_config_gains_new_builtins_once() {
        let base = std::env::temp_dir().join(format!("qbw-config-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&base);
        let paths = paths_in(&base);
        let _ = std::fs::remove_file(paths.config());

        // A file written by an older version: only one prefix, no shortcuts.
        std::fs::write(
            paths.config(),
            "[[prefix]]\nprefix = \">sync\"\nshell = \"true\"\nhint = \"Sync\"\n",
        )
        .unwrap();
        write_default_config(&paths);
        // Re-running must not duplicate a built-in.
        write_default_config(&paths);

        let content = std::fs::read_to_string(paths.config()).unwrap();
        let prefixes: Vec<String> = r_modals::parse_prefixes(&content)
            .unwrap()
            .into_iter()
            .map(|s| s.prefix)
            .collect();
        assert_eq!(prefixes.iter().filter(|n| *n == ">sync").count(), 1);
        assert!(prefixes.contains(&">lock".to_string()));
        assert!(prefixes.contains(&">hide items".to_string()));
        assert!(prefixes.contains(&">show items".to_string()));

        let shortcuts: Vec<String> = r_modals::parse_shortcuts(&content)
            .unwrap()
            .into_iter()
            .map(|s| s.key)
            .collect();
        assert_eq!(shortcuts.iter().filter(|k| *k == "Shift+Enter").count(), 1);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn missing_config_is_materialised_from_the_template() {
        let base = std::env::temp_dir().join(format!("qbw-fresh-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&base);
        let paths = paths_in(&base);
        let _ = std::fs::remove_file(paths.config());

        write_default_config(&paths);

        let content = std::fs::read_to_string(paths.config()).unwrap();
        assert!(r_modals::parse_prefixes(&content).unwrap().len() == 4);
        assert!(r_modals::parse_shortcuts(&content).unwrap().len() == 1);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn legacy_prefixes_and_shortcuts_are_migrated() {
        let base = std::env::temp_dir().join(format!("qbw-legacy-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&base);
        let paths = paths_in(&base);
        let _ = std::fs::remove_file(paths.config());

        let legacy_prefixes = paths.base.join("prefixes.toml");
        let legacy_shortcuts = paths.base.join("shortcuts.toml");
        std::fs::write(
            &legacy_prefixes,
            "[[prefix]]\nprefix = \">custom\"\nshell = \"true\"\nhint = \"Mine\"\n",
        )
        .unwrap();
        std::fs::write(
            &legacy_shortcuts,
            "[[shortcut]]\nkey = \"Ctrl+U\"\npick = \"username\"\n",
        )
        .unwrap();

        write_default_config(&paths);

        // The custom entries survive, the built-ins were added, and the old
        // files are gone.
        let content = std::fs::read_to_string(paths.config()).unwrap();
        let prefixes: Vec<String> = r_modals::parse_prefixes(&content)
            .unwrap()
            .into_iter()
            .map(|s| s.prefix)
            .collect();
        assert!(prefixes.contains(&">custom".to_string()));
        assert!(prefixes.contains(&">lock".to_string()));
        let keys: Vec<String> = r_modals::parse_shortcuts(&content)
            .unwrap()
            .into_iter()
            .map(|s| s.key)
            .collect();
        assert!(keys.contains(&"Ctrl+U".to_string()));
        assert!(keys.contains(&"Shift+Enter".to_string()));
        assert!(!legacy_prefixes.exists());
        assert!(!legacy_shortcuts.exists());

        // Idempotent: a second run leaves a migrated file alone.
        write_default_config(&paths);
        let content2 = std::fs::read_to_string(paths.config()).unwrap();
        assert_eq!(content, content2);

        let _ = std::fs::remove_dir_all(&base);
    }
}
