// SPDX-License-Identifier: GPL-3.0-or-later
//! Runtime paths and tunables, mirroring the shell variables of the original.

use std::path::PathBuf;

/// `quick-bitwarden --version`. Sourced from `Cargo.toml`, so the manifest is
/// the single place to bump the version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Everything the daemon and the thin client need to agree on.
#[derive(Debug, Clone)]
pub struct Paths {
    /// `QBW_BASE` (default `/tmp/qbw`).
    pub base: PathBuf,
    /// `$BASE/s` — Bitwarden session token.
    pub session: PathBuf,
    /// `$BASE/ctl` — control FIFO.
    pub ctl: PathBuf,
    /// `$BASE/pid` — daemon pid.
    pub pid: PathBuf,
    /// `$BASE/menu.pid` — picker/prompt child pid.
    pub menu_pid: PathBuf,
    /// `$BASE/sel` — the picker's selection.
    pub sel: PathBuf,
    /// `QBW_CLEAR_AFTER` (default `20`) — clipboard clear delay.
    pub clear_after: u64,
    /// `QBW_REFRESH_INTERVAL` (default `30`) — cache refresh interval.
    pub refresh_interval: i64,
}

impl Paths {
    pub fn new() -> Self {
        let base = std::env::var_os("QBW_BASE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp/qbw"));
        Self {
            session: base.join("s"),
            ctl: base.join("ctl"),
            pid: base.join("pid"),
            menu_pid: base.join("menu.pid"),
            sel: base.join("sel"),
            base,
            clear_after: env_u64("QBW_CLEAR_AFTER", 20),
            refresh_interval: env_i64("QBW_REFRESH_INTERVAL", 30),
        }
    }

    /// `$BASE/error.log` — last-resort error sink.
    pub fn error_log(&self) -> PathBuf {
        self.base.join("error.log")
    }

    /// `$BASE/prefixes.toml` — query prefixes loaded by the picker.
    pub fn prefixes(&self) -> PathBuf {
        self.base.join("prefixes.toml")
    }

    /// `$BASE/hide_items` — present while the picker hides its item list until
    /// the query is long enough. Kept outside [`Paths::remove_runtime_files`]
    /// so `stop`/`restart` do not reset the user's choice.
    pub fn hide_items_path(&self) -> PathBuf {
        self.base.join("hide_items")
    }

    /// Whether the picker should hide the default item list.
    pub fn items_hidden(&self) -> bool {
        self.hide_items_path().is_file()
    }

    /// Persist the hide-items choice. Removing an already-absent marker is not
    /// an error.
    pub fn set_items_hidden(&self, hidden: bool) -> std::io::Result<()> {
        if hidden {
            std::fs::write(self.hide_items_path(), b"1\n")
        } else {
            match std::fs::remove_file(self.hide_items_path()) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            }
        }
    }

    /// `$BASE/lock` — held by the one running daemon (`flock`) for its life.
    pub fn instance_lock(&self) -> PathBuf {
        self.base.join("lock")
    }

    /// `$BASE/start.lock` — serialises concurrent launcher startup.
    pub fn start_lock(&self) -> PathBuf {
        self.base.join("start.lock")
    }

    /// `$BASE/gen` — `"<generation> <busy>"`; `busy` is 1 while a picker is
    /// pending or open. Written atomically by the daemon.
    pub fn menu_state(&self) -> PathBuf {
        self.base.join("gen")
    }

    /// Remove the runtime files (`ctl`, `pid`, `menu.pid`, `sel`, `gen`).
    /// The lock files are intentionally kept: `flock` is tied to the inode.
    pub fn remove_runtime_files(&self) {
        let menu_state = self.menu_state();
        let menu_state_tmp = menu_state.with_extension("tmp");
        for p in [
            &self.ctl,
            &self.pid,
            &self.menu_pid,
            &self.sel,
            &menu_state,
            &menu_state_tmp,
        ] {
            let _ = std::fs::remove_file(p);
        }
    }
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_i64(key: &str, default: i64) -> i64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn hide_items_choice_is_persisted_and_cleared() {
        let base = std::env::temp_dir().join(format!("qbw-hide-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&base);
        let paths = paths_in(&base);

        assert!(!paths.items_hidden());

        paths.set_items_hidden(true).unwrap();
        assert!(paths.items_hidden());

        // Runtime cleanup (stop/restart) must not reset the choice.
        paths.remove_runtime_files();
        assert!(paths.items_hidden());

        paths.set_items_hidden(false).unwrap();
        assert!(!paths.items_hidden());
        // Idempotent: showing again is not an error.
        paths.set_items_hidden(false).unwrap();

        let _ = std::fs::remove_dir_all(&base);
    }
}
