// SPDX-License-Identifier: GPL-3.0-or-later
//! Runtime paths and tunables, mirroring the shell variables of the original.

use std::path::PathBuf;

/// `quick-bitwarden --version`.
pub const VERSION: &str = "1.0.0";

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
