// SPDX-License-Identifier: GPL-3.0-or-later
//! The background daemon: keeps the vault cache in memory and answers the
//! `show`/`stop` commands delivered over the control FIFO.

use std::io::{BufRead, BufReader};
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use crate::clipboard;
use crate::paths::Paths;
use crate::proc::{self, read_pid_file, send_signal};
use crate::ui;
use crate::vault::{self, State};

/// Set by the signal handler, consumed by the daemon loop.
///
/// A handler may only do async-signal-safe work; `AtomicBool::store` is, while
/// removing files or allocating is not. The loop notices the flag, breaks and
/// runs the normal cleanup path.
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_sig: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

/// `--daemon`: run until `stop` or a termination signal.
pub fn run_daemon(paths: &Paths) {
    if !proc::command_exists("bw") {
        crate::die("missing dependency: bw");
    }

    if let Err(e) = paths.ensure_private_dir() {
        crate::die(&format!(
            "unsafe runtime directory {}: {e}",
            paths.base.display()
        ));
    }

    // Single instance: only the process that owns this lock may be the daemon.
    // A losing `--daemon` exits quietly; `flock` is released by the kernel when
    // the owner exits, so a crash cannot leave a stale lock behind.
    let Some(_instance) = proc::try_lock(&paths.instance_lock()) else {
        return;
    };

    cleanup_stale(paths);
    ui::write_default_config(paths);

    // Menu state: `"<generation> <busy>"`. The daemon bumps the generation and
    // sets `busy` in one atomic write *before* doing any work, so a concurrent
    // press can neither reuse an old generation nor slip in while the picker is
    // still being prepared (which can take a `bw` round-trip).
    let mut generation: u64 = 0;
    let mut busy = false;
    proc::write_menu_state(&paths.menu_state(), generation, busy);

    if let Some(pid) = proc::Pid::current() {
        let _ = proc::write_pid_file(&paths.pid, pid);
    }

    let _ = std::fs::remove_file(&paths.ctl);
    let ctl = proc::cstr(&paths.ctl);
    if unsafe { libc::mkfifo(ctl.as_ptr(), 0o600) } != 0 {
        crate::die("could not create control fifo");
    }

    unsafe {
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::signal(sig, on_signal as *const () as usize);
        }
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
    //
    // Periodic refreshes run on a worker thread, never on this loop, so a slow
    // `bw list items` cannot delay the next `show`.
    let mut refresh: Option<vault::BackgroundRefresh> = None;
    // A `bw lock` started by `>lock` runs on a worker too, so a `show` that
    // follows immediately can paint its loading window before the loop waits
    // for the lock to finish (the `bw` CLI takes a second or two just to start).
    let mut pending_lock: Option<JoinHandle<bool>> = None;

    loop {
        if STOP.load(Ordering::SeqCst) {
            break;
        }

        // Apply a lock that finished while the loop was idle.
        if pending_lock.as_ref().is_some_and(|job| job.is_finished()) {
            finish_lock(&mut pending_lock, &mut state, paths);
        }

        // The FIFO is opened read+write so it never reports EOF; a blocking
        // `read_line` would sit here forever and a signal handler could only
        // abort the process (doing non-async-signal-safe cleanup from the
        // handler). Poll with a timeout instead and unwind normally.
        let mut pfd = libc::pollfd {
            fd: reader.get_ref().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, 200) };
        if rc < 0 {
            if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                break;
            }
            continue;
        }
        if STOP.load(Ordering::SeqCst) {
            break;
        }
        let readable = pfd.revents & libc::POLLIN != 0;
        if pfd.revents & (libc::POLLHUP | libc::POLLERR) != 0
            && !readable
            && reader.buffer().is_empty()
        {
            break;
        }
        if !readable && reader.buffer().is_empty() {
            continue; // timeout: no command yet
        }

        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                // Fold in a refresh that finished while we were idle or busy
                // with the previous command, before acting on the new one.
                let refresh_done = match &refresh {
                    Some(r) => r.try_apply(&mut state),
                    None => false,
                };
                if refresh_done {
                    refresh = None;
                }
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
                        do_show(&mut state, paths, &mut pending_lock);
                        busy = false;
                        proc::write_menu_state(&paths.menu_state(), generation, busy);
                        // Refresh behind the scenes: the picker has closed, so
                        // the user is no longer waiting, and the next `show`
                        // will fold in the result if it is ready.
                        if refresh.is_none() {
                            refresh = vault::start_refresh(&state, paths);
                        }
                    }
                    "sync" => {
                        // `do_sync` installs fresh data itself; drop any pending
                        // background refresh so it cannot resurrect older data.
                        refresh = None;
                        finish_lock(&mut pending_lock, &mut state, paths);
                        do_sync(&mut state, paths);
                    }
                    "lock" => {
                        // A pending refresh still holds the old session; drop
                        // it so it cannot repopulate the cache after the lock.
                        refresh = None;
                        // Finish a previous lock before starting a new one.
                        finish_lock(&mut pending_lock, &mut state, paths);
                        do_lock(&state, &mut pending_lock);
                    }
                    "stop" => break,
                    _ => {}
                }
            }
        }
    }

    // Let an in-flight `bw lock` finish before the process goes away, but do
    // not report anything from a shutting-down daemon. A successful lock also
    // invalidates the stored token, so it must not survive for `restart`.
    if let Some(job) = pending_lock.take() {
        if job.join().unwrap_or(false) {
            paths.remove_session();
        }
    }

    // Never leave the session token in memory on the way out. The file is kept
    // for `restart`; `stop` removes it explicitly once the daemon is gone.
    vault::forget_session(&mut state);
    paths.remove_runtime_files();
}

/// `do_show`: pick an item, copy its password, then scrub history and schedule
/// the clipboard clear.
fn do_show(state: &mut State, paths: &Paths, pending_lock: &mut Option<JoinHandle<bool>>) {
    // Open the picker before any `bw` round-trip. Its loading state is the
    // feedback for the (multi-second) unlock/list, so it must be visible the
    // moment the user asks for it.
    let Some(mut session) = ui::start_menu(paths) else {
        return;
    };

    // A `>lock` may still be running on its worker. The loading window is
    // already up, so wait for it before using the session/cache it is about to
    // invalidate.
    finish_lock(pending_lock, state, paths);

    if state.menu.is_empty() {
        let mut current = Some(session);

        // The vault usually needs the master password after a lock. Ask for it
        // now; `prompt_and_reload` reopens the loading window as soon as the
        // prompt closes, before the slow `bw unlock`.
        if vault::needs_prompt(state, paths) && !prompt_and_reload(state, paths, &mut current) {
            return;
        }

        let mut load = vault::try_load(state, paths);
        if matches!(load, vault::Load::NeedsPassword) {
            // The stored session was stale. Swap the loading window for the
            // password prompt, then reopen it for the actual load.
            if !prompt_and_reload(state, paths, &mut current) {
                return;
            }
            load = vault::try_load(state, paths);
        }

        let Some(open) = current else {
            return;
        };
        session = open;

        match load {
            vault::Load::Loaded => {}
            vault::Load::NeedsPassword => {
                let _ = session.cancel();
                return;
            }
            vault::Load::Unauthenticated => {
                let _ = session.cancel();
                ui::error_dialog(paths, "Not logged in. Run 'bw login' first.");
                return;
            }
            vault::Load::Failed => {
                let _ = session.cancel();
                ui::error_dialog(paths, "Could not read the vault");
                return;
            }
        }

        if state.menu.is_empty() {
            let _ = session.cancel();
            ui::error_dialog(paths, "No login items in the vault");
            return;
        }
    }

    ui::fill_menu(&mut session, &state.tsv);
    let picked = match session.wait() {
        Ok(r_modals::DialogOutput::Picked(sel)) => sel,
        Ok(_) => return,
        Err(e) => {
            ui::error_dialog(paths, &e.to_string());
            return;
        }
    };

    let copy_username = picked.shortcut.as_deref() == Some("username");
    let label = picked.label.trim_end_matches(['\n', '\r']).to_string();
    if label.is_empty() {
        return;
    }

    // Resolve the real Bitwarden id from the row the user picked. The picker
    // reports the option index (stable across filtering) and the option id is
    // the short label, which is not unique: two items can display the same
    // `Name  (user)`. Trusting the label would copy the first match's secret
    // when the user picked a later duplicate, so use the index.
    let id = resolve_pick(&state.tsv, &picked).map(|(_, id)| id.clone());

    let field = if copy_username {
        "username"
    } else {
        "password"
    };
    let lookup = id.as_deref().unwrap_or(label.as_str());
    let value = match fetch_field(state, paths, field, lookup) {
        Ok(value) => value,
        Err(e) => {
            let why = match e {
                FetchError::Locked => "the vault is locked",
                FetchError::Unauthenticated => "not logged in, run 'bw login'",
                FetchError::Failed => "could not read the vault",
            };
            ui::error_dialog(
                paths,
                &format!("Could not fetch {field} for '{label}': {why}"),
            );
            return;
        }
    };

    if !clipboard::copy_password(&value) {
        ui::error_dialog(paths, "Clipboard tool missing (wl-copy, xclip or xsel)");
        return;
    }

    {
        let copied = value.clone();
        std::thread::spawn(move || clipboard::remove_from_history(&copied));
    }
    if !clipboard::schedule_clear(paths.clear_after) {
        // Last resort when re-exec fails (e.g. the binary was deleted):
        // a thread at least clears while this daemon stays alive.
        let clear_after = paths.clear_after;
        std::thread::spawn(move || clipboard::clear_password(clear_after));
    }

    if copy_username {
        ui::notify_copied_field("username", &short_label(&label), paths.clear_after);
    } else {
        ui::notify_copied(&short_label(&label), paths.clear_after);
    }
}

/// Ask for the master password and reopen the loading window before the slow
/// `bw unlock`, so feedback returns the moment the prompt closes.
///
/// `session` is the loading window that is already open; it stays visible
/// during the status probe and is replaced only by the prompt itself.
fn prompt_and_reload(
    state: &mut State,
    paths: &Paths,
    session: &mut Option<ui::MenuSession>,
) -> bool {
    let request = vault::request_master_password(state, paths, || {
        if let Some(open) = session.take() {
            let _ = open.cancel();
        }
    });

    match request {
        vault::PasswordRequest::AlreadyUnlocked => true,
        vault::PasswordRequest::Cancelled => {
            if let Some(open) = session.take() {
                let _ = open.cancel();
            }
            false
        }
        vault::PasswordRequest::Password(password) => {
            // Reopen the loading window *before* `bw unlock`, not after it.
            *session = ui::start_menu(paths);
            if session.is_none() {
                return false;
            }
            let unlocked = vault::unlock_with_password(state, paths, password);
            if !unlocked {
                if let Some(open) = session.take() {
                    let _ = open.cancel();
                }
            }
            unlocked
        }
    }
}

/// Wait for a `bw lock` started by `>lock` to finish and apply its result.
fn finish_lock(job: &mut Option<JoinHandle<bool>>, state: &mut State, paths: &Paths) {
    let Some(job) = job.take() else {
        return;
    };
    if !job.join().unwrap_or(false) {
        ui::error_dialog(paths, "Could not lock the vault");
        return;
    }
    paths.remove_session();
    vault::forget_session(state);
    state.menu.clear();
    state.tsv.clear();
    state.last_refresh = 0;
    state.unlocked = Some(false);
    ui::notify("Vault locked");
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

/// `lock`: start `bw lock` on a worker; [`finish_lock`] forgets the session and
/// cached items once it finishes, so a later `show` must ask for the master
/// password again.
///
/// The CLI takes a second or two to start, so running it on the FIFO loop would
/// keep a `show` issued right after `>lock` from even painting its loading
/// window.
fn do_lock(state: &State, job: &mut Option<JoinHandle<bool>>) {
    let session = state.session.clone();
    *job = Some(std::thread::spawn(move || {
        vault::bw_lock(session.as_deref())
    }));
}

/// Why a per-pick `bw get` failed, so the caller can show a useful message.
enum FetchError {
    Locked,
    Unauthenticated,
    Failed,
}

/// Read one field straight from the vault.
///
/// The daemon deliberately keeps no decrypted secrets between picks, so this is
/// the point where a vault lock (from `bw` or any other client) takes effect. A
/// stale session is retried once after the master-password dialog.
fn fetch_field(
    state: &mut State,
    paths: &Paths,
    field: &str,
    lookup: &str,
) -> Result<String, FetchError> {
    if let Some(value) = vault::bw_get_field(state.session.as_deref(), field, lookup) {
        if !value.is_empty() {
            return Ok(value);
        }
    }

    match vault::bw_status(state.session.as_deref()).as_str() {
        "locked" => {
            if !vault::prompt_and_unlock(state, paths) {
                return Err(FetchError::Locked);
            }
            match vault::bw_get_field(state.session.as_deref(), field, lookup) {
                Some(value) if !value.is_empty() => Ok(value),
                _ => Err(FetchError::Failed),
            }
        }
        "unauthenticated" => Err(FetchError::Unauthenticated),
        _ => Err(FetchError::Failed),
    }
}

/// `cleanup_stale`: drop any leftover picker and runtime files.
pub fn cleanup_stale(paths: &Paths) {
    if let Some(pid) = read_pid_file(&paths.menu_pid) {
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

/// Map a picker selection back to the `(label, id)` row the user chose.
///
/// `ModalSelection::index` is the index into the option list as sent, even when
/// the visible list is filtered, so it identifies the row unambiguously. The
/// option id is the short label (needed by the hidden-list exact-name search)
/// and therefore not unique. Custom input (a name that matched no row) has no
/// row: the caller falls back to `bw get <typed text>`.
fn resolve_pick<'a>(
    tsv: &'a [(String, String)],
    sel: &r_modals::ModalSelection,
) -> Option<&'a (String, String)> {
    if sel.custom_input {
        return None;
    }
    let row = tsv.get(sel.index)?;
    (short_label(&row.0) == sel.id).then_some(row)
}

#[cfg(test)]
mod tests {
    use super::{resolve_pick, short_label};
    use r_modals::ModalSelection;

    fn selection(index: usize, id: &str, label: &str, custom_input: bool) -> ModalSelection {
        ModalSelection {
            id: id.to_string(),
            label: label.to_string(),
            index,
            path: vec![index],
            query: label.to_string(),
            custom_input,
            shortcut: None,
        }
    }

    #[test]
    fn short_label_matches_bash_parameter_expansion() {
        assert_eq!(short_label("GitHub  (octocat)"), "GitHub");
        assert_eq!(short_label("GitLab"), "GitLab");
        // `${s%%  (*}` cuts at the first `  (`.
        assert_eq!(short_label("A  (b)  (c)"), "A");
        assert_eq!(short_label(""), "");
    }

    #[test]
    fn resolve_pick_uses_the_row_index_not_the_shared_label() {
        let tsv = vec![
            ("GitHub  (octocat)".to_string(), "id-a".to_string()),
            ("GitHub  (octocat)".to_string(), "id-b".to_string()),
        ];

        // Two rows share the same label/id; the index picks the exact one.
        let first = selection(0, "GitHub", "GitHub  (octocat)", false);
        assert_eq!(
            resolve_pick(&tsv, &first).map(|(_, id)| id.as_str()),
            Some("id-a")
        );
        let second = selection(1, "GitHub", "GitHub  (octocat)", false);
        assert_eq!(
            resolve_pick(&tsv, &second).map(|(_, id)| id.as_str()),
            Some("id-b")
        );

        // A mismatched pair is not trusted: the caller falls back to `bw get`.
        let tampered = selection(1, "Other", "Other", false);
        assert!(resolve_pick(&tsv, &tampered).is_none());

        // Free-typed input has no row at all.
        let typed = selection(0, "GitHub", "GitHub", true);
        assert!(resolve_pick(&tsv, &typed).is_none());
    }
}
