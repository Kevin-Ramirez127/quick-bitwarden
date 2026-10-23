// SPDX-License-Identifier: GPL-3.0-or-later
//! quick-bitwarden — copy Bitwarden passwords from a daemon-backed picker.

mod clipboard;
mod commands;
mod daemon;
mod paths;
mod proc;
mod ui;
mod vault;

use paths::{Paths, VERSION};

/// `die`: print `quick-bitwarden: <msg>` and exit non-zero.
pub fn die(msg: &str) -> ! {
    eprintln!("quick-bitwarden: {msg}");
    std::process::exit(1);
}

fn usage() {
    print!(
        "Usage: quick-bitwarden [COMMAND] [OPTION]\n\
         \n\
         A quick Bitwarden vault CLI backed by a menu-driven daemon.\n\
         \n\
         Commands:\n\
         \x20 show           Open the item picker (copies the selected password)\n\
         \x20 hide           Close the picker menu if open\n\
         \x20 toggle         Open the picker if closed, close it if open (default)\n\
         \x20 sync           Sync the vault and refresh the cache\n\
         \x20 lock           Lock the vault and forget the cached session\n\
         \x20 hide-items     Hide the item list until a 3+ character search\n\
         \x20 show-items     Show the full item list again\n\
         \x20 stop           Stop the background daemon\n\
         \x20 restart        Restart the daemon (starts it if not running)\n\
         \x20 status         Show daemon and vault status\n\
         \x20 status --toggle  Show whether the picker is shown or hidden\n\
         \x20 --daemon       Run in the background as the daemon (internal)\n\
         \n\
         Options:\n\
         \x20 -h, --help     Show this help and exit\n\
         \x20 -v, --version  Print version and exit\n\
         \n\
         Environment:\n\
         \x20 QBW_BASE              Runtime directory (default: $XDG_RUNTIME_DIR/qbw, fallback /tmp/qbw)\n\
         \x20 QBW_CLEAR_AFTER       Seconds before the clipboard is cleared (default: 20)\n\
         \x20 QBW_REFRESH_INTERVAL  Cache refresh interval in seconds (default: 30)\n"
    );
}

fn main() {
    // `umask 077`: session/IPC files are private to the user.
    unsafe { libc::umask(0o077) };

    // Dialog children spawned by `r_modals::Dialog` (the picker, the password
    // prompt, and error dialogs) re-enter this binary with a mode flag. Handle
    // them before any quick-bitwarden command parsing.
    r_modals::dialog_main!();

    let args: Vec<String> = std::env::args().skip(1).collect();

    // Internal mode: the detached clipboard-clear helper spawned by
    // `clipboard::schedule_clear`. It must not need the runtime directory or
    // trigger the picker.
    if args.first().map(String::as_str) == Some("--clear-clipboard") {
        let secs = args
            .get(1)
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(20);
        clipboard::clear_password(clipboard::clamp_clear_after(secs));
        return;
    }

    let paths = Paths::new();

    match args.first().map(String::as_str) {
        Some("--daemon") => daemon::run_daemon(&paths),
        Some("-h") | Some("--help") => usage(),
        Some("-v") | Some("--version") => println!("quick-bitwarden {VERSION}"),
        Some("show") => commands::cmd_show(&paths),
        Some("hide") => commands::cmd_hide(&paths),
        Some("toggle") | None => commands::cmd_toggle(&paths),
        Some("sync") => commands::cmd_sync(&paths),
        Some("lock") => commands::cmd_lock(&paths),
        Some("hide-items") => commands::cmd_hide_items(&paths),
        Some("show-items") => commands::cmd_show_items(&paths),
        Some("stop") => commands::cmd_stop(&paths),
        Some("restart") => commands::cmd_restart(&paths),
        Some("status") => {
            if args.get(1).map(String::as_str) == Some("--toggle") {
                commands::cmd_status_toggle(&paths);
            } else {
                commands::cmd_status(&paths);
            }
        }
        Some(_) => die(
            "usage: quick-bitwarden [show|hide|toggle|sync|lock|hide-items|show-items|stop|restart|status] (see --help)",
        ),
    }
}
