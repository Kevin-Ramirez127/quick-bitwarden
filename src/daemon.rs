// SPDX-License-Identifier: GPL-3.0-or-later
//! The background daemon: keeps the vault cache in memory and answers the
//! `show`/`stop` commands delivered over the control FIFO.

use std::io::{BufRead, BufReader};
use std::sync::OnceLock;

use crate::clipboard;
use crate::paths::Paths;
use crate::proc::{self, read_pid, send_signal};
use crate::ui;
use crate::vault::{self, State};

/// Set after the FIFO exists so the signal handler can clean up.
static CLEANUP_PATHS: OnceLock<Paths> = OnceLock::new();

extern "C" fn on_signal(_sig: libc::c_int) {
    if let Some(paths) = CLEANUP_PATHS.get() {
        paths.remove_runtime_files();
    }
    unsafe { libc::_exit(0) };
}

/// `--daemon`: run until `stop` or a termination signal.
pub fn run_daemon(paths: &Paths) {
    if !proc::command_exists("bw") {
        crate::die("missing dependency: bw");
    }

    let _ = std::fs::create_dir_all(&paths.base);
    cleanup_stale(paths);
    ui::write_default_prefixes(paths);

    let _ = std::fs::write(&paths.pid, format!("{}\n", std::process::id()));

    let _ = std::fs::remove_file(&paths.ctl);
    let ctl = proc::cstr(&paths.ctl);
    if unsafe { libc::mkfifo(ctl.as_ptr(), 0o600) } != 0 {
        crate::die("could not create control fifo");
    }

    let _ = CLEANUP_PATHS.set(paths.clone());
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as usize);
        libc::signal(libc::SIGINT, on_signal as *const () as usize);
    }

    // `exec 3<>"$CTL"`: open read+write so the FIFO never reports EOF.
    let ctl_file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&paths.ctl)
    {
        Ok(f) => f,
        Err(_) => crate::die("could not open control fifo"),
    };
    let mut reader = BufReader::new(ctl_file);

    let mut state = State::default();
    // Never prompt at startup. The master-password dialog belongs to `show`;
    // prompting here would make a cancelled prompt immediately reappear for the
    // `show` that `ensure_daemon` queued right after starting the daemon.
    // Warm the cache only when the session is already unlocked.
    vault::load_session(&mut state, paths);
    if vault::bw_status(state.session.as_deref()) == "unlocked" {
        let _ = vault::refresh_cache(&mut state, paths);
    }
    state.last_refresh = proc::now_secs();

    loop {
        let mut cmd = String::new();
        match reader.read_line(&mut cmd) {
            Ok(0) | Err(_) => break,
            Ok(_) => match cmd.trim_end_matches(['\n', '\r']) {
                "show" => {
                    do_show(&mut state, paths);
                    vault::refresh_if_stale(&mut state, paths);
                }
                "sync" => do_sync(&mut state, paths),
                "stop" => break,
                _ => {}
            },
        }
    }

    paths.remove_runtime_files();
}

/// `do_show`: pick an item, copy its password, then scrub history and schedule
/// the clipboard clear.
fn do_show(state: &mut State, paths: &Paths) {
    if state.menu.is_empty() {
        if !vault::ensure_unlocked(state, paths) {
            return;
        }
        let _ = vault::refresh_cache(state, paths);
    }

    if state.menu.is_empty() {
        ui::error_dialog(paths, "No login items in the vault");
        return;
    }

    if !ui::launch_menu(paths, &state.menu) {
        return;
    }

    let selection = std::fs::read_to_string(&paths.sel).unwrap_or_default();
    let _ = std::fs::remove_file(&paths.sel);
    let selection = selection.trim_end_matches(['\n', '\r']).to_string();
    if selection.is_empty() {
        return;
    }

    // First label match wins, like `awk '$1 == s { print $2; exit }'`.
    let id = state
        .tsv
        .iter()
        .find(|(label, _)| label == &selection)
        .map(|(_, id)| id.clone());

    let mut password = id
        .as_ref()
        .and_then(|id| state.pass.get(id).cloned())
        .unwrap_or_default();

    if password.is_empty() {
        let lookup = id.clone().unwrap_or_else(|| selection.clone());
        password = vault::bw_get_password(state.session.as_deref(), &lookup).unwrap_or_default();
        if password.is_empty() {
            ui::error_dialog(
                paths,
                &format!("Could not fetch password for '{selection}'"),
            );
            return;
        }
    }

    if !clipboard::copy_password(&password) {
        ui::error_dialog(paths, "Clipboard tool missing (wl-copy, xclip or xsel)");
        return;
    }

    {
        let pw = password.clone();
        std::thread::spawn(move || clipboard::remove_from_history(&pw));
    }
    {
        let clear_after = paths.clear_after;
        std::thread::spawn(move || clipboard::clear_password(clear_after));
    }

    ui::notify_copied(&short_label(&selection), paths.clear_after);
}

/// `sync`: run `bw sync`, then rebuild the in-memory cache from the fresh data.
fn do_sync(state: &mut State, paths: &Paths) {
    if !vault::ensure_unlocked(state, paths) {
        return;
    }
    // Tell the user a sync started; a second notification follows when it ends.
    ui::notify("Syncing vault via bw sync… you'll be notified when it ends");
    if !vault::bw_sync(state.session.as_deref()) {
        ui::error_dialog(paths, "Could not sync the vault");
        return;
    }
    if vault::refresh_cache(state, paths) {
        state.last_refresh = proc::now_secs();
    }
    ui::notify("Vault synced");
}

/// `cleanup_stale`: drop any leftover picker and runtime files.
pub fn cleanup_stale(paths: &Paths) {
    if let Some(pid) = read_pid(&paths.menu_pid) {
        send_signal(pid, libc::SIGTERM);
    }
    paths.remove_runtime_files();
}

/// `${selection%%  (*}` — strip the trailing `  (username)` from a label.
fn short_label(selection: &str) -> String {
    match selection.find("  (") {
        Some(i) => selection[..i].to_string(),
        None => selection.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::short_label;

    #[test]
    fn short_label_matches_bash_parameter_expansion() {
        assert_eq!(short_label("GitHub  (octocat)"), "GitHub");
        assert_eq!(short_label("GitLab"), "GitLab");
        // `${s%%  (*}` cuts at the first `  (`.
        assert_eq!(short_label("A  (b)  (c)"), "A");
        assert_eq!(short_label(""), "");
    }
}
