// SPDX-License-Identifier: GPL-3.0-or-later
//! Runtime paths and tunables, mirroring the shell variables of the original.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// `quick-bitwarden --version`. Sourced from `Cargo.toml`, so the manifest is
/// the single place to bump the version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Historical runtime root used when `$XDG_RUNTIME_DIR` is unavailable.
const LEGACY_BASE: &str = "/tmp/qbw";

/// Everything the daemon and the thin client need to agree on.
#[derive(Debug, Clone)]
pub struct Paths {
    /// `QBW_BASE` (default `/tmp/qbw`).
    pub base: PathBuf,
    /// `$XDG_STATE_HOME/quick-bitwarden` (fallback
    /// `~/.local/state/quick-bitwarden`) — choices that must survive a logout
    /// or reboot, such as the hide-items marker.
    pub state: PathBuf,
    /// `$BASE/s` — Bitwarden session token.
    pub session: PathBuf,
    /// `$BASE/ctl` — control FIFO.
    pub ctl: PathBuf,
    /// `$BASE/pid` — daemon pid.
    pub pid: PathBuf,
    /// `$BASE/menu.pid` — picker/prompt child pid.
    pub menu_pid: PathBuf,
    /// `$BASE/sel` — legacy selection file (truncated when a picker opens;
    /// typed dialogs return the pick directly, but cleanup still removes it).
    pub sel: PathBuf,
    /// `QBW_CLEAR_AFTER` (default `20`) — clipboard clear delay.
    pub clear_after: u64,
    /// `QBW_REFRESH_INTERVAL` (default `30`) — cache refresh interval.
    pub refresh_interval: i64,
}

impl Paths {
    pub fn new() -> Self {
        let base = std::env::var_os("QBW_BASE")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(default_runtime_base)
            .unwrap_or_else(|| PathBuf::from(LEGACY_BASE));
        // Keep durable choices out of `$XDG_RUNTIME_DIR`, which is wiped on
        // logout/reboot. If neither XDG_STATE_HOME nor HOME is usable, fall
        // back to the runtime dir rather than failing: the choice then works
        // for this session, exactly as before.
        let state = default_state_base().unwrap_or_else(|| base.join("state"));
        Self {
            session: base.join("s"),
            ctl: base.join("ctl"),
            pid: base.join("pid"),
            menu_pid: base.join("menu.pid"),
            sel: base.join("sel"),
            base,
            state,
            // Clamp the timing knobs: a typo must not turn the prompt into a
            // refresh loop or keep a password on the clipboard for days.
            clear_after: env_u64("QBW_CLEAR_AFTER", 20).min(crate::clipboard::MAX_CLEAR_AFTER),
            refresh_interval: env_i64("QBW_REFRESH_INTERVAL", 30).clamp(5, 86_400),
        }
    }

    /// `$BASE/error.log` — last-resort error sink.
    pub fn error_log(&self) -> PathBuf {
        self.base.join("error.log")
    }

    /// `$BASE/config.toml` — query prefixes and keyboard shortcuts loaded by
    /// the picker.
    pub fn config(&self) -> PathBuf {
        self.base.join("config.toml")
    }

    /// `$XDG_STATE_HOME/quick-bitwarden/hide_items` (fallback
    /// `~/.local/state/quick-bitwarden/hide_items`) — present while the picker
    /// hides its item list until the query is long enough.
    ///
    /// This lives in the durable state dir, not in the runtime dir, so the
    /// choice survives a logout/reboot as well as `stop`/`restart`.
    pub fn hide_items_path(&self) -> PathBuf {
        self.state.join("hide_items")
    }

    /// Legacy location: `$QBW_BASE/hide_items`. Only read for migration.
    fn legacy_hide_items_path(&self) -> PathBuf {
        self.base.join("hide_items")
    }

    /// Whether the picker should hide the default item list.
    pub fn items_hidden(&self) -> bool {
        self.hide_items_path().is_file() || self.legacy_hide_items_path().is_file()
    }

    /// Move a marker left in the volatile runtime dir to the state dir.
    ///
    /// Best-effort: if the state dir cannot be created (for example because it
    /// is not private), the legacy marker still applies for this session.
    pub fn migrate_hide_items(&self) {
        if !self.hide_items_path().is_file() && self.legacy_hide_items_path().is_file() {
            let _ = self.set_items_hidden(true);
        }
    }

    /// Persist the hide-items choice. Removing an already-absent marker is not
    /// an error. The state directory is created (or tightened) as `0700` and
    /// the marker is written as `0600` without following a symlink.
    pub fn set_items_hidden(&self, hidden: bool) -> std::io::Result<()> {
        if hidden {
            open_private_dir(&self.state, true)?;
            write_private_file(&self.hide_items_path(), b"1\n")?;
            // Migration is complete: do not leave a second copy behind in the
            // runtime dir, where it would be wiped on logout.
            let _ = std::fs::remove_file(self.legacy_hide_items_path());
            Ok(())
        } else {
            remove_file_if_exists(&self.hide_items_path())?;
            remove_file_if_exists(&self.legacy_hide_items_path())
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
    /// Durable choices such as the hide-items marker live in [`Paths::state`]
    /// and are not touched here.
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

    /// Create the runtime directory if needed and refuse to use it unless it is
    /// a real directory owned by the current user with no group/other access.
    ///
    /// The base lives in a world-writable directory (`$XDG_RUNTIME_DIR` is safe,
    /// but the `/tmp/qbw` fallback is not), so another local user could create
    /// it first and plant a `config.toml` (arbitrary command execution), a
    /// session-token destination, or PID files. Validating ownership and mode
    /// after `open(2)` closes that hole.
    pub fn ensure_private_dir(&self) -> io::Result<()> {
        open_private_dir(&self.base, true).map(|_| ())
    }

    /// Validate an already existing runtime directory. A missing directory is
    /// fine for read-only clients: there is simply nothing to read.
    pub fn validate_private_dir(&self) -> io::Result<()> {
        match open_private_dir(&self.base, false) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other.map(|_| ()),
        }
    }

    /// Whether `$BASE/config.toml` may be handed to the picker. The picker
    /// executes `shell` actions from that file, so a config writable by another
    /// user must never be loaded.
    pub fn config_is_trusted(&self) -> bool {
        std::fs::metadata(self.config())
            .map(|md| {
                md.is_file() && md.uid() == unsafe { libc::geteuid() } && md.mode() & 0o022 == 0
            })
            .unwrap_or(false)
    }

    /// Persist the session token with mode `0600`, refusing a symlinked target.
    ///
    /// The runtime directory is already private, but writing the strongest
    /// credential of the vault deserves its own check: a swapped symlink must
    /// not turn this into "write the token wherever the attacker can read it".
    pub fn write_session(&self, token: &str) -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&self.session)?;
        // `mode` only applies when the file is created: repair a legacy mode.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(token.as_bytes())
    }

    /// Forget the stored session token (best effort).
    pub fn remove_session(&self) {
        let _ = std::fs::remove_file(&self.session);
    }
}

/// `$XDG_RUNTIME_DIR/qbw` when the runtime dir is present and safe.
///
/// The spec guarantees a per-user, `0700` directory, but verify it anyway: a
/// bogus environment must not send the daemon somewhere another user controls.
fn default_runtime_base() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?);
    if !dir.is_absolute() {
        return None;
    }
    let md = std::fs::symlink_metadata(&dir).ok()?;
    if !md.is_dir() || md.uid() != unsafe { libc::geteuid() } || md.mode() & 0o077 != 0 {
        return None;
    }
    Some(dir.join("qbw"))
}

/// `$XDG_STATE_HOME/quick-bitwarden`, falling back to
/// `~/.local/state/quick-bitwarden`.
///
/// Relative paths are ignored, as the XDG base-directory spec requires. The
/// state dir is only created on demand (see [`Paths::set_items_hidden`]) and is
/// verified to be private there.
fn default_state_base() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        let dir = PathBuf::from(dir);
        if dir.is_absolute() {
            return Some(dir.join("quick-bitwarden"));
        }
    }
    let home = std::env::var_os("HOME").filter(|v| !v.is_empty())?;
    let home = PathBuf::from(home);
    if !home.is_absolute() {
        return None;
    }
    Some(home.join(".local").join("state").join("quick-bitwarden"))
}

/// Write a small state file as `0600`, refusing to follow a symlink.
fn write_private_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    // `mode` only applies when the file is created; repair a legacy mode.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(bytes)
}

fn remove_file_if_exists(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Open `path` as a directory without following a final symlink and verify that
/// it is owned by us and private to us. When `create` is set, missing
/// components are created with mode `0700`.
fn open_private_dir(path: &Path, create: bool) -> io::Result<std::fs::File> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "is not an absolute path",
        ));
    }
    if create {
        match std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
        {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }

    let dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|e| match e.raw_os_error() {
            // `O_NOFOLLOW` + `O_DIRECTORY` reports a symlink as ENOTDIR.
            Some(libc::ELOOP) | Some(libc::ENOTDIR) => insecure("is a symlink or not a directory"),
            _ => e,
        })?;

    let md = dir.metadata()?;
    if !md.is_dir() {
        return Err(insecure("is not a directory"));
    }
    if md.uid() != unsafe { libc::geteuid() } {
        return Err(insecure("is not owned by the current user"));
    }
    if md.mode() & 0o077 != 0 {
        // We own it: tighten it instead of failing on a legacy directory.
        dir.set_permissions(std::fs::Permissions::from_mode(0o700))?;
        if dir.metadata()?.mode() & 0o077 != 0 {
            return Err(insecure("is not private (mode must be 0700)"));
        }
    }
    Ok(dir)
}

fn insecure(why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, why)
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
            state: base.join("state"),
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
        assert_eq!(
            std::fs::metadata(paths.hide_items_path()).unwrap().mode() & 0o777,
            0o600,
            "the marker must not be readable by other users"
        );
        assert_eq!(
            std::fs::metadata(&paths.state).unwrap().mode() & 0o777,
            0o700,
            "the state dir must be private"
        );

        // Runtime cleanup (stop/restart) must not reset the choice.
        paths.remove_runtime_files();
        assert!(paths.items_hidden());

        paths.set_items_hidden(false).unwrap();
        assert!(!paths.items_hidden());
        // Idempotent: showing again is not an error.
        paths.set_items_hidden(false).unwrap();

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn legacy_hide_items_marker_is_migrated_out_of_the_runtime_dir() {
        let base = std::env::temp_dir().join(format!("qbw-hide-legacy-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&base);
        let paths = paths_in(&base);
        let legacy = paths.base.join("hide_items");
        std::fs::write(&legacy, b"1\n").unwrap();

        // A pre-upgrade marker still hides the list before migration runs.
        assert!(paths.items_hidden());

        paths.migrate_hide_items();
        assert!(paths.hide_items_path().is_file());
        assert!(!legacy.exists(), "the volatile copy must be removed");
        assert!(paths.items_hidden());

        // `show-items` also clears a stale legacy marker.
        std::fs::write(&legacy, b"1\n").unwrap();
        paths.set_items_hidden(false).unwrap();
        assert!(!paths.hide_items_path().exists());
        assert!(!legacy.exists());

        let _ = std::fs::remove_dir_all(&base);
    }

    fn temp_base(name: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("qbw-{name}-{}-{nanos}", std::process::id()))
    }

    #[test]
    fn runtime_dir_is_created_private_and_tightened() {
        let base = temp_base("private");
        let paths = paths_in(&base);

        // A missing directory is fine for read-only clients.
        paths.validate_private_dir().unwrap();

        paths.ensure_private_dir().unwrap();
        let mode = std::fs::metadata(&base).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o700, "new runtime dirs must be 0700");

        // A legacy directory we own is tightened instead of rejected.
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
        paths.ensure_private_dir().unwrap();
        let mode = std::fs::metadata(&base).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o700, "an owned directory must be tightened");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn runtime_dir_rejects_symlinks_and_regular_files() {
        let base = temp_base("reject");
        std::fs::create_dir_all(&base).unwrap();

        let link = base.with_extension("link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&base, &link).unwrap();
        assert!(
            paths_in(&link).ensure_private_dir().is_err(),
            "a symlinked runtime dir must be refused"
        );

        let file = base.with_extension("file");
        std::fs::write(&file, b"not a directory").unwrap();
        assert!(
            paths_in(&file).ensure_private_dir().is_err(),
            "a regular file must not be used as the runtime dir"
        );

        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn runtime_dir_must_be_absolute() {
        let paths = paths_in(std::path::Path::new("relative/qbw"));
        assert!(
            paths.ensure_private_dir().is_err(),
            "a relative QBW_BASE would depend on each process's cwd"
        );
        assert!(paths.validate_private_dir().is_err());
    }

    #[test]
    fn config_is_trusted_requires_ownership_and_no_group_write() {
        let base = temp_base("config-trust");
        std::fs::create_dir_all(&base).unwrap();
        let paths = paths_in(&base);

        assert!(
            !paths.config_is_trusted(),
            "a missing config is not trusted"
        );

        std::fs::write(paths.config(), "[[prefix]]\n").unwrap();
        std::fs::set_permissions(paths.config(), std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(paths.config_is_trusted());

        // Group-writable configs could be edited by another user.
        std::fs::set_permissions(paths.config(), std::fs::Permissions::from_mode(0o664)).unwrap();
        assert!(!paths.config_is_trusted());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn session_token_is_private_and_never_follows_a_symlink() {
        let base = temp_base("session");
        let paths = paths_in(&base);
        paths.ensure_private_dir().unwrap();

        paths.write_session("token-123").unwrap();
        assert_eq!(
            std::fs::metadata(&paths.session).unwrap().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::read_to_string(&paths.session).unwrap(),
            "token-123"
        );

        paths.remove_session();
        assert!(!paths.session.exists());

        // A planted symlink must not become a token exfiltration channel.
        let target = base.join("target");
        std::fs::write(&target, b"keep").unwrap();
        std::os::unix::fs::symlink(&target, &paths.session).unwrap();
        assert!(paths.write_session("leak").is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");

        let _ = std::fs::remove_dir_all(&base);
    }
}
