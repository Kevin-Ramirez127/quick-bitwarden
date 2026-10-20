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

    // Single instance: only the process that owns this lock may be the daemon.
    // A losing `--daemon` exits quietly; `flock` is released by the kernel when
    // the owner exits, so a crash cannot leave a stale lock behind.
    let Some(_instance) = proc::try_lock(&paths.instance_lock()) else {
        return;
    };

    cleanup_stale(paths);
    ui::write_default_prefixes(paths);

    // Menu state: `"<generation> <busy>"`. The daemon bumps the generation and
    // sets `busy` in one atomic write *before* doing any work, so a concurrent
    // press can neither reuse an old generation nor slip in while the picker is
    // still being prepared (which can take a `bw` round-trip).
    let mut generation: u64 = 0;
    let mut busy = false;
    proc::write_menu_state(&paths.menu_state(), generation, busy);

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
    // No vault I/O at startup. The first `show` unlocks and loads the cache on
    // demand, so starting the daemon (including `restart`) never blocks on `bw`
    // and the control FIFO is serviced immediately.

    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let line = line.trim_end_matches(['\n', '\r']);
                let (verb, arg) = match line.split_once(' ') {
                    Some((verb, arg)) => (verb, Some(arg.trim())),
                    None => (line, None),
                };
                match verb {
                    "show" => {
                        if busy {
                            continue;
                        }
                        if let Some(stamp) = arg.and_then(|a| a.parse::<u64>().ok()) {
                            // Stamped before the current menu generation: this
                            // request was queued while a window was already
                            // being opened, so the user has been served. Drop
                            // it instead of stacking a second window.
                            if stamp != generation {
                                continue;
                            }
                        }
                        // Claim this generation *before* the (possibly slow)
                        // unlock/cache load and window spawn, so concurrent
                        // presses see `busy` and do not queue a second window.
                        generation = generation.wrapping_add(1);
                        busy = true;
                        proc::write_menu_state(&paths.menu_state(), generation, busy);
                        do_show(&mut state, paths);
                        busy = false;
                        proc::write_menu_state(&paths.menu_state(), generation, busy);
                        vault::refresh_if_stale(&mut state, paths);
                    }
                    "sync" => do_sync(&mut state, paths),
                    "stop" => break,
                    _ => {}
                }
            }
        }
    }

    paths.remove_runtime_files();
}

/// `do_show`: pick an item, copy its password, then scrub history and schedule
/// the clipboard clear.
fn do_show(state: &mut State, paths: &Paths) {
    if state.menu.is_empty() {
        // `ensure_unlocked` populates the cache as part of the unlock check.
        if !vault::ensure_unlocked(state, paths) {
            return;
        }
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
    let _ = vault::refresh_cache(state, paths);
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
