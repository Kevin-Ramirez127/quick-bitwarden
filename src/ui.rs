// SPDX-License-Identifier: GPL-3.0-or-later
//! The view layer, built on [`r_modals`].
//!
//! The daemon never opens a window itself: every dialog runs in a short-lived
//! child process (`--menu`, `--ask`, `--error`) so it can be killed from the
//! outside, which keeps `hide` and `toggle` working.

use std::process::Command;

use r_modals::{ConfigFile, Dialog, DialogOutput, DialogSession};

use crate::paths::Paths;
use crate::proc::command_exists;

/// Shown in the picker window until `bw list items` has produced a menu.
const LOADING_MESSAGE: &str = "Loading your data from bw…";

/// `${selection%%  (*}` — strip the trailing `  (username)` from a label.
fn short_label(selection: &str) -> String {
    match selection.find("  (") {
        Some(i) => selection[..i].to_string(),
        None => selection.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Parent side: spawn the child dialogs and collect their result.
// ---------------------------------------------------------------------------

/// Open the picker window without waiting for vault data.
///
/// The window starts in its loading state; [`fill_menu`] delivers the items and
/// closes stdin, then [`DialogSession::wait`] collects the typed selection.
pub fn start_menu(paths: &Paths) -> Option<DialogSession> {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => {
            error_dialog(paths, "Could not locate the quick-bitwarden executable");
            return None;
        }
    };

    let mut dialog = Dialog::menu()
        .program(exe)
        .app_id("quick-bitwarden")
        .pid_file(paths.menu_pid.clone())
        .placeholder("Search")
        .loading_message(LOADING_MESSAGE)
        .allow_custom_input(true)
        .footer_hint("Enter copy pass · Shift+Enter copy username · Tab complete · Esc cancel");

    // Privacy mode (`>hide items`): keep the whole vault out of the default
    // view. A 3+ character query filters normally; shorter queries only reveal
    // an item typed in full.
    if paths.items_hidden() {
        dialog = dialog
            .min_match_chars(3)
            .placeholder("Type 3+ characters")
            .empty_message("No matches — type 3+ characters or an exact name");
    }

    // `$BASE/config.toml` holds both the `>…` query prefixes (e.g. `>s` → sync)
    // and the keyboard shortcuts (e.g. Shift+Enter → copy the username). Since
    // those entries can run shell commands, never load a file another user
    // could have written.
    let config = paths.config();
    if config.is_file() {
        if paths.config_is_trusted() {
            dialog = dialog.config_file(config);
        } else {
            error_dialog(
                paths,
                "Refusing to load config.toml: it is not owned by you or is writable by another user.",
            );
        }
    }

    // Truncate up front so an interrupted run cannot leave a stale selection.
    let _ = std::fs::write(&paths.sel, b"");

    match dialog.start() {
        Ok(session) => Some(session),
        Err(e) => {
            error_dialog(paths, &format!("Could not open the search window: {e}"));
            None
        }
    }
}

/// Send the vault entries to the picker and close its input.
///
/// Each option id is the short label (`Name`, without `  (user)`), which keeps
/// the hidden-list exact-name behaviour. The daemon maps the pick back to the
/// real Bitwarden id through `tsv`.
pub fn fill_menu(session: &mut DialogSession, tsv: &[(String, String)]) {
    let options: Vec<(String, String)> = tsv
        .iter()
        .map(|(label, _id)| (short_label(label), label.clone()))
        .collect();
    let _ = session.send_options(options);
    session.close_input();
}

/// Show the picker for an already-loaded cache and wait for its result.
pub fn show_menu(paths: &Paths, tsv: &[(String, String)]) -> Option<DialogOutput> {
    let mut session = start_menu(paths)?;
    fill_menu(&mut session, tsv);
    match session.wait() {
        Ok(output) => Some(output),
        Err(e) => {
            error_dialog(paths, &e.to_string());
            None
        }
    }
}

/// Prompt for the master password (masked) and return it, or `None` when
/// cancelled / unavailable.
pub fn ask_password(paths: &Paths, label: &str) -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let output = Dialog::ask(label)
        .program(exe)
        .app_id("quick-bitwarden")
        .placeholder(label)
        .password(true)
        .pid_file(paths.menu_pid.clone())
        .run()
        .ok()?;
    match output {
        DialogOutput::Text(pw) if !pw.is_empty() => Some(pw),
        _ => None,
    }
}

/// `error_dialog`: show a message in an r-modals dialog. If that fails (no
/// display) fall back to `notify-send`, then to `$BASE/error.log`.
pub fn error_dialog(paths: &Paths, msg: &str) {
    let shown = std::env::current_exe().ok().is_some_and(|exe| {
        Dialog::error(msg)
            .program(exe)
            .app_id("quick-bitwarden")
            .title("quick-bitwarden")
            .confirm_label("OK")
            .cancel_label("Close")
            .run()
            .is_ok()
    });
    if shown {
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
/// `{exe}` with the running executable.
///
/// A single `config.toml` holds both the query prefixes and the keyboard
/// shortcuts. An existing file is preserved, but any built-in entries added by
/// a newer version are appended (per section), so upgrades pick up `>lock`
/// without losing local customisations.
///
/// A `prefixes.toml`/`shortcuts.toml` from before the unification is migrated
/// into the new file the first time the daemon starts.
pub fn write_default_config(paths: &Paths) {
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "quick-bitwarden".to_string());
    let exe = shell_quote(&exe);
    let path = paths.config();

    let _ = ConfigFile::at(&path)
        .template(include_str!("../config.toml"))
        .var("exe", &exe)
        .migrate_from([
            paths.base.join("prefixes.toml"),
            paths.base.join("shortcuts.toml"),
        ])
        .ensure();

    // Older versions wrote `{qbw}` into the file. Upgrade those entries in
    // place; new files use `{exe}` exclusively.
    if let Ok(text) = std::fs::read_to_string(&path) {
        if text.contains("{qbw}") {
            let _ = std::fs::write(&path, text.replace("{qbw}", &exe));
        }
    }
}

/// Single-quote a word for `sh -c`.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
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

    fn temp_paths(name: &str) -> (std::path::PathBuf, Paths) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("qbw-{name}-{}-{nanos}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let paths = paths_in(&base);
        (base, paths)
    }

    #[test]
    fn shell_quote_wraps_and_escapes() {
        assert_eq!(shell_quote("/usr/bin/qbw"), "'/usr/bin/qbw'");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn embedded_config_template_parses_both_sections() {
        let content = include_str!("../config.toml").replace("{exe}", "'/bin/quick-bitwarden'");

        // One file: the prefix parser ignores `[[shortcut]]`, and vice versa.
        let prefixes = r_modals::parse_prefixes(&content).expect("prefixes parse");
        let names: Vec<&str> = prefixes.iter().map(|s| s.prefix.as_str()).collect();
        assert_eq!(
            names,
            [
                ">sync",
                ">lock",
                ">hide items",
                ">show items",
                ">restart",
                ">stop"
            ]
        );
        assert!(prefixes.iter().all(|s| s
            .shell
            .as_deref()
            .unwrap()
            .contains("/bin/quick-bitwarden")));
        assert!(prefixes
            .iter()
            .all(|s| !s.shell.as_deref().unwrap().contains("{exe}")));

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
        let (base, paths) = temp_paths("config");

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
        assert!(prefixes.contains(&">restart".to_string()));
        assert!(prefixes.contains(&">stop".to_string()));

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
        let (base, paths) = temp_paths("fresh");

        write_default_config(&paths);

        let content = std::fs::read_to_string(paths.config()).unwrap();
        assert_eq!(r_modals::parse_prefixes(&content).unwrap().len(), 6);
        assert_eq!(r_modals::parse_shortcuts(&content).unwrap().len(), 1);
        assert!(!content.contains("{exe}"));
        assert!(!content.contains("{qbw}"));

        let exe = std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(content.contains(&shell_quote(&exe)));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn legacy_prefixes_and_shortcuts_are_migrated() {
        let (base, paths) = temp_paths("legacy");

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

    #[test]
    fn legacy_qbw_tokens_are_upgraded_to_exe() {
        let (base, paths) = temp_paths("legacy-token");
        std::fs::write(
            paths.config(),
            "[[prefix]]\nprefix = \">sync\"\nshell = \"{qbw} sync\"\n",
        )
        .unwrap();

        write_default_config(&paths);

        let content = std::fs::read_to_string(paths.config()).unwrap();
        assert!(!content.contains("{qbw}"));
        let exe = std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(content.contains(&shell_quote(&exe)));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn malformed_config_is_left_alone() {
        let (base, paths) = temp_paths("malformed");
        let broken = "this is not = valid toml\n[[prefix]\n";
        std::fs::write(paths.config(), broken).unwrap();

        write_default_config(&paths);

        assert_eq!(std::fs::read_to_string(paths.config()).unwrap(), broken);
        let _ = std::fs::remove_dir_all(&base);
    }
}
