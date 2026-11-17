// SPDX-License-Identifier: MIT
//! Thin client commands: start/steer the daemon and manage the open picker.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::daemon::cleanup_stale;
use crate::paths::Paths;
use crate::proc::{self, alive, command_exists, is_fifo, read_pid_file, send_signal};
use crate::{ui, vault};

/// Refuse to touch a runtime directory another user could have planted.
///
/// A missing directory is fine: the client will start the daemon (which creates
/// it) or simply report that it is not running.
fn require_private_dir(paths: &Paths) {
    if let Err(e) = paths.validate_private_dir() {
        crate::die(&format!(
            "unsafe runtime directory {}: {e}",
            paths.base.display()
        ));
    }
}

/// `daemon_alive`: pid file points at the live daemon (same start time).
pub fn daemon_alive(paths: &Paths) -> bool {
    read_pid_file(&paths.pid).map(alive).unwrap_or(false)
}

/// `menu_open`: a picker/prompt child is alive.
pub fn menu_open(paths: &Paths) -> bool {
    read_pid_file(&paths.menu_pid).map(alive).unwrap_or(false)
}

/// `kill_menu`: close the open picker/prompt child, if any.
pub fn kill_menu(paths: &Paths) {
    if let Some(pid) = read_pid_file(&paths.menu_pid) {
        send_signal(pid, libc::SIGTERM);
    }
}

/// `send_cmd`: write one command line to the control FIFO.
fn send_cmd(paths: &Paths, cmd: &str) {
    if !is_fifo(&paths.ctl) {
        crate::die("daemon not running");
    }
    match std::fs::OpenOptions::new().write(true).open(&paths.ctl) {
        Ok(mut f) => {
            let _ = f.write_all(format!("{cmd}\n").as_bytes());
        }
        Err(_) => crate::die("daemon not running"),
    }
}

/// `ensure_daemon`: start the detached daemon and wait for it to be ready.
pub fn ensure_daemon(paths: &Paths) {
    if let Err(e) = paths.ensure_private_dir() {
        crate::die(&format!(
            "unsafe runtime directory {}: {e}",
            paths.base.display()
        ));
    }

    // Serialise startup across concurrent invocations. Without this, two
    // launchers can both see no daemon and spawn one each (two daemons, two
    // windows). The guard is held until readiness, so the next launcher sees
    // the daemon already running.
    let _start_guard = proc::lock(&paths.start_lock());

    if daemon_alive(paths) {
        return;
    }

    cleanup_stale(paths);

    let exe =
        std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("quick-bitwarden"));

    let mut cmd = if command_exists("setsid") {
        let mut c = Command::new("setsid");
        c.arg(&exe);
        c
    } else {
        let mut c = Command::new("nohup");
        c.arg(&exe);
        c
    };
    cmd.arg("--daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let _ = cmd.spawn();

    // Poll at 10 ms so a cold start does not wait up to a full 100 ms tick
    // after the daemon has created its pid file and control FIFO.
    for _ in 0..2000 {
        if daemon_alive(paths) && is_fifo(&paths.ctl) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    crate::die(&format!(
        "daemon failed to start (see {}/error.log)",
        paths.base.display()
    ));
}

/// `request_show`: ask the daemon for a picker window exactly once per press.
///
/// The daemon publishes `"<generation> <busy>"` atomically. Reading it before
/// checking `menu.pid` means a request issued while the picker is being
/// prepared (`busy`) or for an older generation is never sent; one that races
/// a concurrent press is dropped by the daemon's generation check.
fn request_show(paths: &Paths) {
    let state_file = paths.menu_state();
    let has_state = state_file.is_file();
    let (generation, busy) = proc::read_menu_state(&state_file);
    if busy || menu_open(paths) {
        return;
    }
    if has_state {
        send_cmd(paths, &format!("show {generation}"));
    } else {
        // Daemon from before generation stamping: its protocol only knows the
        // bare command. It will be upgraded on the next `restart`.
        send_cmd(paths, "show");
    }
}

/// `toggle`: close the picker if open, otherwise open it.
pub fn cmd_toggle(paths: &Paths) {
    require_private_dir(paths);
    if menu_open(paths) {
        kill_menu(paths);
        return;
    }
    ensure_daemon(paths);
    request_show(paths);
}

/// `show`: open the picker unless it is already open.
pub fn cmd_show(paths: &Paths) {
    ensure_daemon(paths);
    request_show(paths);
}

/// `hide`: close the picker, keeping the daemon alive.
pub fn cmd_hide(paths: &Paths) {
    require_private_dir(paths);
    kill_menu(paths);
}

/// `hide-items`: keep the picker's item list out of the default view until a
/// 3+ character search. Persisted in the durable state dir, so it survives
/// `stop`/`restart` and a logout/reboot.
pub fn cmd_hide_items(paths: &Paths) {
    set_items_hidden(paths, true);
}

/// `show-items`: list every item again when the picker opens.
pub fn cmd_show_items(paths: &Paths) {
    set_items_hidden(paths, false);
}

fn set_items_hidden(paths: &Paths, hidden: bool) {
    // The daemon reads the marker when it opens the picker, so a running
    // daemon picks the change up on the next `show` without a round-trip.
    if let Err(e) = paths.set_items_hidden(hidden) {
        crate::die(&format!("could not save the items setting: {e}"));
    }
    let message = if hidden {
        "Item list hidden: type 3+ characters to search"
    } else {
        "Item list shown again"
    };
    ui::notify(message);
    println!("quick-bitwarden: {message}");
}

/// `sync`: ask the daemon to run `rbw sync` and refresh its cache.
pub fn cmd_sync(paths: &Paths) {
    ensure_daemon(paths);
    send_cmd(paths, "sync");
}

/// `lock`: lock the vault and forget the cached item data.
///
/// rbw's agent owns the unlocked keys, so it does the work when the daemon is
/// running; otherwise this process can lock rbw directly.
pub fn cmd_lock(paths: &Paths) {
    require_private_dir(paths);
    if daemon_alive(paths) {
        // Close the picker first: the daemon may be blocked serving it, and
        // a stale window must not keep offering items after the lock.
        kill_menu(paths);
        send_cmd(paths, "lock");
        return;
    }

    if !command_exists("rbw") {
        crate::die("missing dependency: rbw");
    }
    if !vault::rbw_lock(paths) {
        crate::die("could not lock the vault");
    }
    paths.remove_legacy_session();
    println!("quick-bitwarden: vault locked");
}

/// `stop`: ask the daemon to exit, forcing it after a grace period.
///
/// Stopping forgets the unlocked vault: q-bw stops the daemon and locks rbw, so
/// the next `show` asks for the master password again. Use `hide` to keep the
/// daemon (and the unlocked agent) running.
pub fn cmd_stop(paths: &Paths) {
    require_private_dir(paths);
    let was_running = daemon_alive(paths);
    if was_running {
        stop_daemon(paths, false);
    } else {
        cleanup_stale(paths);
    }
    // rbw's agent is a separate process. `stop` is the explicit "forget the
    // session" command, so lock it as well; `restart` deliberately does not.
    if command_exists("rbw") {
        let _ = vault::rbw_lock(paths);
    }
    paths.remove_legacy_session();
    if !was_running {
        println!("quick-bitwarden: not running");
    }
}

/// `restart`: cycle the daemon, starting it when it is not running.
pub fn cmd_restart(paths: &Paths) {
    require_private_dir(paths);
    if daemon_alive(paths) {
        // A restart keeps the unlocked vault: rbw's agent is not touched.
        stop_daemon(paths, true);
    } else {
        cleanup_stale(paths);
    }
    ensure_daemon(paths);
}

/// `stop_daemon`: end a live daemon (FIFO `stop`, then signals) and clean up.
fn stop_daemon(paths: &Paths, keep_unlocked: bool) {
    if is_fifo(&paths.ctl) {
        if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(&paths.ctl) {
            let _ = f.write_all(b"stop\n");
        }
    }

    for _ in 0..50 {
        if !daemon_alive(paths) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    if daemon_alive(paths) {
        kill_menu(paths);
        let pid = read_pid_file(&paths.pid);
        if let Some(pid) = pid {
            send_signal(pid, libc::SIGTERM);
        }
        std::thread::sleep(Duration::from_millis(300));
        if let Some(pid) = pid {
            send_signal(pid, libc::SIGKILL);
        }
    }

    cleanup_stale(paths);
    if !keep_unlocked {
        // Drop the token a pre-rbw version may still have left behind. rbw's
        // agent is locked by `cmd_stop` itself.
        paths.remove_legacy_session();
    }
}

/// `status`: report daemon pid and vault state.
pub fn cmd_status(paths: &Paths) {
    require_private_dir(paths);
    if !daemon_alive(paths) {
        println!("quick-bitwarden: daemon not running");
        return;
    }
    if !command_exists("rbw") {
        crate::die("missing dependency: rbw");
    }

    let status = match vault::rbw_status(paths) {
        vault::VaultStatus::Unlocked => "unlocked",
        vault::VaultStatus::Locked => "locked",
        vault::VaultStatus::Unauthenticated => "unauthenticated",
        vault::VaultStatus::Unknown => "unknown",
    };
    let items = if paths.items_hidden() {
        "hidden until a 3+ character search"
    } else {
        "shown"
    };
    let pid = read_pid_file(&paths.pid).map(|p| p.pid).unwrap_or(0);
    println!("quick-bitwarden: daemon running (pid {pid}), vault: {status}, items: {items}");
}

/// `status --toggle`: report whether the picker is shown or hidden.
pub fn cmd_status_toggle(paths: &Paths) {
    require_private_dir(paths);
    println!("quick-bitwarden: {}", toggle_state(paths));
}

/// `"shown"` while a picker is pending or open, `"hidden"` otherwise.
pub fn toggle_state(paths: &Paths) -> &'static str {
    let (_, busy) = proc::read_menu_state(&paths.menu_state());
    if busy || menu_open(paths) {
        "shown"
    } else {
        "hidden"
    }
}

#[cfg(test)]
mod tests {
    use super::toggle_state;
    use crate::paths::Paths;

    fn paths_in(base: &std::path::Path) -> Paths {
        Paths {
            base: base.to_path_buf(),
            state: base.join("state"),
            ctl: base.join("ctl"),
            pid: base.join("pid"),
            menu_pid: base.join("menu.pid"),
            sel: base.join("sel"),
            clear_after: 20,
            refresh_interval: 30,
        }
    }

    #[test]
    fn toggle_state_reports_shown_and_hidden() {
        let base = std::env::temp_dir().join(format!("qbw-toggle-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&base);
        let paths = paths_in(&base);
        let _ = std::fs::remove_file(&paths.menu_pid);

        assert_eq!(toggle_state(&paths), "hidden");

        // Our own pid is alive, so a `menu.pid` pointing at it reads as shown.
        std::fs::write(&paths.menu_pid, format!("{}\n", std::process::id())).unwrap();
        assert_eq!(toggle_state(&paths), "shown");

        let _ = std::fs::remove_file(&paths.menu_pid);
        let _ = std::fs::remove_dir(&base);
    }
}
