// SPDX-License-Identifier: GPL-3.0-or-later
//! Thin client commands: start/steer the daemon and manage the open picker.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::daemon::cleanup_stale;
use crate::paths::Paths;
use crate::proc::{command_exists, is_fifo, pid_alive, read_pid, send_signal};
use crate::vault;

/// `daemon_alive`: pid file points at a live process.
pub fn daemon_alive(paths: &Paths) -> bool {
    read_pid(&paths.pid).map(pid_alive).unwrap_or(false)
}

/// `menu_open`: a picker/prompt child is alive.
pub fn menu_open(paths: &Paths) -> bool {
    read_pid(&paths.menu_pid).map(pid_alive).unwrap_or(false)
}

/// `kill_menu`: close the open picker/prompt child, if any.
pub fn kill_menu(paths: &Paths) {
    if menu_open(paths) {
        if let Some(pid) = read_pid(&paths.menu_pid) {
            send_signal(pid, libc::SIGTERM);
        }
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
    if daemon_alive(paths) {
        return;
    }

    let _ = std::fs::create_dir_all(&paths.base);
    let _ = std::fs::set_permissions(&paths.base, std::fs::Permissions::from_mode(0o700));
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

    for _ in 0..200 {
        if daemon_alive(paths) && is_fifo(&paths.ctl) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    crate::die(&format!(
        "daemon failed to start (see {}/error.log)",
        paths.base.display()
    ));
}

/// `toggle`: close the picker if open, otherwise open it.
pub fn cmd_toggle(paths: &Paths) {
    if menu_open(paths) {
        kill_menu(paths);
        return;
    }
    ensure_daemon(paths);
    send_cmd(paths, "show");
}

/// `show`: open the picker unless it is already open.
pub fn cmd_show(paths: &Paths) {
    ensure_daemon(paths);
    if menu_open(paths) {
        return;
    }
    send_cmd(paths, "show");
}

/// `hide`: close the picker, keeping the daemon alive.
pub fn cmd_hide(paths: &Paths) {
    kill_menu(paths);
}

/// `sync`: ask the daemon to run `bw sync` and refresh its cache.
pub fn cmd_sync(paths: &Paths) {
    ensure_daemon(paths);
    send_cmd(paths, "sync");
}

/// `stop`: ask the daemon to exit, forcing it after a grace period.
pub fn cmd_stop(paths: &Paths) {
    if !daemon_alive(paths) {
        cleanup_stale(paths);
        println!("quick-bitwarden: not running");
        return;
    }

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
        let pid = read_pid(&paths.pid);
        if let Some(pid) = pid {
            send_signal(pid, libc::SIGTERM);
        }
        std::thread::sleep(Duration::from_millis(300));
        if let Some(pid) = pid {
            send_signal(pid, libc::SIGKILL);
        }
    }

    cleanup_stale(paths);
}

/// `status`: report daemon pid and vault state.
pub fn cmd_status(paths: &Paths) {
    if !daemon_alive(paths) {
        println!("quick-bitwarden: daemon not running");
        return;
    }
    if !command_exists("bw") {
        crate::die("missing dependency: bw");
    }

    let session = std::fs::read_to_string(&paths.session)
        .ok()
        .map(|s| s.trim_end_matches(['\n', '\r']).to_string());

    let status = vault::bw_status_status_only(session.as_deref());
    let pid = read_pid(&paths.pid).unwrap_or(0);
    println!("quick-bitwarden: daemon running (pid {pid}), vault: {status}");
}
