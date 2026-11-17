// SPDX-License-Identifier: MIT
//! rbw access: the in-memory cache and per-pick field lookup.
//!
//! rbw keeps the unlocked vault keys in its own `rbw-agent`, so q-bw no longer
//! carries a session token around. The agent is asked for the item list when
//! the cache is refreshed, and for one field when the user actually picks an
//! item, which keeps a vault lock from another client effective immediately.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::OnceLock;

use serde::Deserialize;

use crate::paths::Paths;
use crate::proc::{now_secs, which};
use crate::ui;

/// Everything the daemon keeps hot between invocations.
///
/// Deliberately metadata-only: vault passwords and usernames are fetched from
/// `rbw` at pick time instead of being cached, so locking the vault (in `rbw`,
/// or from another process) is enforced by the agent on every copy.
#[derive(Default)]
pub struct State {
    /// Picker labels, one line per item (`cut -f1` of `tsv`).
    pub menu: String,
    /// `(label, id)` pairs, in rbw's list order (sorted by name).
    pub tsv: Vec<(String, String)>,
    /// Unix time of the last successful cache refresh.
    pub last_refresh: i64,
    /// Cached vault lock state: `Some(true)` unlocked, `Some(false)` locked,
    /// logged out or otherwise unable to read. `None` unknown.
    pub unlocked: Option<bool>,
}

#[derive(Deserialize)]
struct Item {
    #[serde(rename = "type")]
    kind: Option<String>,
    name: Option<String>,
    id: Option<String>,
    user: Option<String>,
}

/// Outcome of probing rbw's agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VaultStatus {
    /// The local database is unlocked and can be read.
    Unlocked,
    /// rbw is configured, but the database (or the agent) is locked.
    Locked,
    /// rbw has not been configured/logged in yet.
    Unauthenticated,
    /// The probe failed for another reason.
    Unknown,
}

/// Outcome of a cache load that never prompts.
pub enum Load {
    /// The vault is unlocked and the in-memory cache is populated.
    Loaded,
    /// The vault is locked; the caller must run [`unlock`].
    NeedsPassword,
    /// Not logged in (`rbw login` is required).
    Unauthenticated,
    /// rbw failed for another reason.
    Failed,
}

/// Resolve `rbw` once, using the same trusted-`PATH` rules as every other
/// helper binary (absolute entries, executable we or root own).
fn rbw_program() -> &'static PathBuf {
    static RBW: OnceLock<PathBuf> = OnceLock::new();
    RBW.get_or_init(|| which("rbw").unwrap_or_else(|| PathBuf::from("rbw")))
}

/// `PATH` with q-bw's private `pinentry` shim in front.
///
/// rbw runs the program named by its `pinentry` config setting (default
/// `pinentry`) from `PATH`, so this is how the master-password prompt becomes
/// q-bw's own r-modals dialog without editing the user's rbw config.
fn rbw_path(paths: &Paths) -> OsString {
    let shim = paths.pinentry_dir();
    match std::env::var_os("PATH") {
        Some(path) if !path.is_empty() => {
            let mut value = shim.into_os_string();
            value.push(":");
            value.push(path);
            value
        }
        _ => shim.into_os_string(),
    }
}

/// A command that talks to `rbw` (and, indirectly, `rbw-agent`).
fn rbw_command(paths: &Paths) -> Command {
    let mut cmd = Command::new(rbw_program());
    cmd.env("PATH", rbw_path(paths))
        // Keep a missing terminal from turning an unlock into a tty prompt;
        // the pinentry shim uses the graphical dialog instead.
        .stdin(Stdio::null());

    // rbw looks for `rbw-agent` on PATH. When both binaries live next to each
    // other (as they do in every package), point at the sibling so a PATH that
    // contains only the `rbw` symlink still works.
    if let Some(dir) = rbw_program().parent() {
        let agent = dir.join("rbw-agent");
        if agent.is_file() {
            cmd.env("RBW_AGENT", agent);
        }
    }
    cmd
}

/// `rbw unlocked`: does the agent currently hold the vault keys?
pub fn rbw_status(paths: &Paths) -> VaultStatus {
    match rbw_command(paths).arg("unlocked").output() {
        Ok(out) if out.status.success() => VaultStatus::Unlocked,
        Ok(out) => classify_probe(&out),
        Err(_) => VaultStatus::Unknown,
    }
}

/// Map a failed `rbw unlocked` to a state.
fn classify_probe(out: &Output) -> VaultStatus {
    let stderr = String::from_utf8_lossy(&out.stderr).to_lowercase();
    if !rbw_is_configured() || !rbw_is_logged_in() {
        return VaultStatus::Unauthenticated;
    }
    if stderr.contains("agent is locked") || stderr.contains("agent not running") {
        VaultStatus::Locked
    } else if stderr.contains("config") || stderr.contains("email") {
        VaultStatus::Unauthenticated
    } else {
        VaultStatus::Unknown
    }
}

/// Whether rbw has an email configured, honouring `XDG_CONFIG_HOME` and
/// `RBW_PROFILE` the same way rbw's own `directories` lookup does.
fn rbw_is_configured() -> bool {
    rbw_config()
        .get("email")
        .and_then(|email| email.as_str())
        .is_some_and(|email| !email.is_empty())
}

/// rbw's `config.json`, or an empty object when it cannot be read.
fn rbw_config() -> serde_json::Value {
    let Some(path) = rbw_config_file() else {
        return serde_json::Value::Object(serde_json::Map::new());
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()))
}

/// Whether rbw has a usable local database for the configured account.
///
/// `rbw unlock` runs rbw's login flow first. With no database that flow asks
/// for an API key in pinentry (and would otherwise send *that* request to
/// q-bw's master-password dialog), so it must be detected before the agent is
/// started. `rbw` stores the database under its cache directory, named after
/// the server and email, with `/`, `:` and `%` percent-encoded.
fn rbw_is_logged_in() -> bool {
    let Some(file) = rbw_db_file() else {
        return false;
    };
    let Ok(text) = std::fs::read_to_string(file) else {
        return false;
    };
    let Ok(db) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    let present = |key: &str| {
        db.get(key)
            .and_then(|value| value.as_str())
            .is_some_and(|value| !value.is_empty())
    };
    present("access_token") && present("refresh_token") && present("protected_key")
}

/// `$XDG_CACHE_HOME/rbw[-profile]`, like rbw's `directories` lookup.
fn rbw_cache_dir() -> Option<PathBuf> {
    let profile = match std::env::var("RBW_PROFILE") {
        Ok(profile) if !profile.is_empty() => format!("rbw-{profile}"),
        _ => "rbw".to_string(),
    };
    Some(xdg_base("XDG_CACHE_HOME", ".cache")?.join(profile))
}

/// An absolute XDG directory, falling back to the matching `$HOME` path.
fn xdg_base(variable: &str, home_suffix: &str) -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(variable).filter(|value| !value.is_empty()) {
        let dir = PathBuf::from(dir);
        if dir.is_absolute() {
            return Some(dir);
        }
    }
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .map(|home| home.join(home_suffix))
}

/// rbw's database path for the configured account.
fn rbw_db_file() -> Option<PathBuf> {
    let config = rbw_config();
    let email = config
        .get("email")
        .and_then(|email| email.as_str())
        .filter(|email| !email.is_empty())?;
    let server = config
        .get("base_url")
        .and_then(|base_url| base_url.as_str())
        .unwrap_or("default");
    Some(rbw_cache_dir()?.join(format!("{}:{email}.json", encode_server(server))))
}

/// Percent-encode the characters rbw escapes in its database filename.
fn encode_server(server: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = Vec::with_capacity(server.len());
    for byte in server.bytes() {
        if byte < 0x20 || byte == 0x7f || matches!(byte, b'/' | b'%' | b':') {
            out.push(b'%');
            out.push(HEX[(byte >> 4) as usize]);
            out.push(HEX[(byte & 0x0f) as usize]);
        } else {
            out.push(byte);
        }
    }
    // The input is UTF-8; only ASCII bytes are escaped, so this is still UTF-8.
    String::from_utf8(out).unwrap_or_default()
}

/// Whether rbw will resolve `pinentry` through `PATH`.
///
/// q-bw shadows the default `pinentry` program with its own dialog by putting
/// a private symlink first on the agent's `PATH`. An absolute or custom
/// `pinentry` path in the user's rbw config bypasses that, which would hand the
/// master password to a terminal prompt instead of q-bw's window. Treat a
/// missing setting as the default.
fn pinentry_is_shimable(value: Option<&str>) -> bool {
    value.is_none_or(|value| value.is_empty() || value == "pinentry")
}

/// `rbw_pinentry_is_shimable`: the user's config lets q-bw supply the prompt.
pub fn rbw_pinentry_is_shimable() -> bool {
    pinentry_is_shimable(
        rbw_config()
            .get("pinentry")
            .and_then(|pinentry| pinentry.as_str()),
    )
}

/// Explain how to restore q-bw's master-password dialog.
pub const PINENTRY_HELP: &str = "rbw's pinentry setting bypasses q-bw's password dialog. Run \
     'rbw config set pinentry pinentry' to use the default.";

fn rbw_config_file() -> Option<PathBuf> {
    let profile = match std::env::var("RBW_PROFILE") {
        Ok(profile) if !profile.is_empty() => format!("rbw-{profile}"),
        _ => "rbw".to_string(),
    };
    Some(
        xdg_base("XDG_CONFIG_HOME", ".config")?
            .join(profile)
            .join("config.json"),
    )
}

/// `needs_prompt`: true when the caller should unlock before reading.
///
/// A known-unlocked state is trusted, so a warm daemon does not spawn a probe
/// for every `show`. The probe itself is cheap (`rbw unlocked`), unlike the
/// old `bw status` round-trip.
pub fn needs_prompt(state: &mut State, paths: &Paths) -> bool {
    if state.unlocked.is_none() {
        state.unlocked = Some(rbw_status(paths) == VaultStatus::Unlocked);
    }
    state.unlocked != Some(true)
}

/// `try_load`: populate the cache without ever prompting.
///
/// rbw's `list`/`get` automatically unlock (and can therefore open a password
/// prompt), so the lock state is probed first and a locked vault is never
/// handed to `rbw list`.
pub fn try_load(state: &mut State, paths: &Paths) -> Load {
    // Already warm with data (e.g. `sync`): no need to re-list first.
    if state.unlocked == Some(true) && !state.menu.is_empty() {
        return Load::Loaded;
    }

    match fetch_cache(paths) {
        ListOutcome::Cache(cache) => {
            cache.apply(state);
            state.last_refresh = now_secs();
            state.unlocked = Some(true);
            Load::Loaded
        }
        ListOutcome::Locked => {
            state.unlocked = Some(false);
            Load::NeedsPassword
        }
        ListOutcome::Unauthenticated => {
            state.unlocked = Some(false);
            Load::Unauthenticated
        }
        ListOutcome::Failed => Load::Failed,
    }
}

/// Unlock the vault through rbw.
///
/// The password prompt is shown by the `pinentry` shim while `rbw unlock` runs,
/// so it is still q-bw's dialog and the master password never passes through an
/// argument vector or a file.
pub fn unlock(state: &mut State, paths: &Paths) -> bool {
    // Recreate the shim in case it is missing after an upgrade.
    let _ = paths.prepare_pinentry_shim();
    let _ = paths.take_pinentry_result();

    // A custom pinentry path would bypass the shim and ask for the master
    // password in a terminal. Refuse before rbw can start the agent.
    if !rbw_pinentry_is_shimable() {
        ui::error_dialog(paths, PINENTRY_HELP);
        state.unlocked = Some(false);
        return false;
    }

    match rbw_status(paths) {
        VaultStatus::Unlocked => {
            state.unlocked = Some(true);
            return true;
        }
        VaultStatus::Unauthenticated => {
            ui::error_dialog(paths, "Not logged in. Run 'rbw login' first.");
            state.unlocked = Some(false);
            return false;
        }
        VaultStatus::Locked | VaultStatus::Unknown => {}
    }

    // A locked agent started by another program may not have q-bw's pinentry
    // shim on its PATH (the agent inherits the environment of whichever
    // command started it). Since it holds no keys, restarting it is lossless
    // and makes sure the next prompt is q-bw's dialog.
    if rbw_status(paths) == VaultStatus::Locked {
        let _ = rbw_command(paths).arg("stop-agent").output();
    }

    match rbw_command(paths).arg("unlock").output() {
        Ok(out) if out.status.success() => {
            let _ = paths.take_pinentry_result();
            state.unlocked = Some(true);
            true
        }
        Ok(out) => {
            report_unlock_failure(paths, &out);
            state.unlocked = Some(false);
            false
        }
        Err(_) => {
            ui::error_dialog(paths, "Could not unlock the vault: rbw is not runnable");
            state.unlocked = Some(false);
            false
        }
    }
}

/// Surface why `rbw unlock` failed, staying silent when the user cancelled.
fn report_unlock_failure(paths: &Paths, out: &Output) {
    match paths.take_pinentry_result().as_deref() {
        // Escape in the password dialog: the old flow returned quietly.
        Some("cancelled") => {}
        // rbw retries a rejected password up to three times; the shim stops the
        // retries on the first `SETERROR` so the user sees one prompt and one
        // error, exactly like before.
        Some("retry" | "submitted") => {
            ui::error_dialog(paths, "Could not unlock the vault: wrong master password?");
        }
        _ => {
            let stderr = String::from_utf8_lossy(&out.stderr).to_lowercase();
            if !rbw_is_configured()
                || !rbw_is_logged_in()
                || stderr.contains("config")
                || stderr.contains("email")
                || stderr.contains("load db")
                || stderr.contains("failed to log in")
                || stderr.contains("failed to register")
            {
                ui::error_dialog(paths, "Not logged in. Run 'rbw login' first.");
            } else {
                ui::error_dialog(paths, "Could not unlock the vault: wrong master password?");
            }
        }
    }
}

/// `prompt_and_unlock`: unlock (prompting through rbw) without loading the
/// cache, so the caller can keep its loading window around the slow part.
pub fn prompt_and_unlock(state: &mut State, paths: &Paths) -> bool {
    unlock(state, paths)
}

/// `ensure_unlocked`: unlock (prompting if needed) and populate the cache.
/// Returns `true` when the vault is unlocked (even if it holds no login items).
///
/// Used by commands that need a ready cache in one step (e.g. `sync`); the
/// picker uses [`needs_prompt`] + [`try_load`] so it can show its loading window
/// around the slow part.
pub fn ensure_unlocked(state: &mut State, paths: &Paths) -> bool {
    match try_load(state, paths) {
        Load::Loaded => true,
        Load::NeedsPassword => unlock(state, paths) && refresh_cache(state, paths),
        Load::Unauthenticated => {
            ui::error_dialog(paths, "Not logged in. Run 'rbw login' first.");
            false
        }
        Load::Failed => {
            ui::error_dialog(paths, "Could not read the vault");
            false
        }
    }
}

/// `rbw sync`: pull the latest vault from the server. `true` on success.
pub fn rbw_sync(paths: &Paths) -> bool {
    rbw_command(paths)
        .arg("sync")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// `rbw lock`: lock the database so the next `show` needs the master password.
pub fn rbw_lock(paths: &Paths) -> bool {
    rbw_command(paths)
        .arg("lock")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// The parse result of one `rbw list --raw`, before it is folded into [`State`].
struct Cache {
    menu: String,
    tsv: Vec<(String, String)>,
}

impl Cache {
    fn apply(self, state: &mut State) {
        state.menu = self.menu;
        state.tsv = self.tsv;
    }
}

/// Why a cache load could not produce entries.
enum ListOutcome {
    Cache(Cache),
    Locked,
    Unauthenticated,
    Failed,
}

/// `rbw list --raw` → cache tables. Never prompts.
fn fetch_cache(paths: &Paths) -> ListOutcome {
    // `rbw list` calls `unlock()` internally, which starts the agent and opens
    // a pinentry prompt when the vault is locked. Probe first so a background
    // refresh cannot pop a password dialog.
    match rbw_status(paths) {
        VaultStatus::Unlocked => {}
        VaultStatus::Locked => return ListOutcome::Locked,
        VaultStatus::Unauthenticated => return ListOutcome::Unauthenticated,
        VaultStatus::Unknown => return ListOutcome::Failed,
    }

    let out = match rbw_command(paths).args(["list", "--raw"]).output() {
        Ok(out) => out,
        Err(_) => return ListOutcome::Failed,
    };
    if !out.status.success() {
        // A lock can land between the probe and the list; report the state that
        // actually applies instead of a generic read error.
        return match rbw_status(paths) {
            VaultStatus::Locked => ListOutcome::Locked,
            VaultStatus::Unauthenticated => ListOutcome::Unauthenticated,
            _ => ListOutcome::Failed,
        };
    }
    match parse_cache(&out.stdout) {
        Some(cache) => ListOutcome::Cache(cache),
        None => ListOutcome::Failed,
    }
}

/// `refresh_cache`: rebuild the menu/tsv tables from `rbw list --raw`,
/// stamping `last_refresh` on success so callers cannot forget to.
/// Returns `false` when rbw fails or its output is not a usable item array.
pub fn refresh_cache(state: &mut State, paths: &Paths) -> bool {
    match fetch_cache(paths) {
        ListOutcome::Cache(cache) => {
            cache.apply(state);
            state.last_refresh = now_secs();
            state.unlocked = Some(true);
            true
        }
        ListOutcome::Locked | ListOutcome::Unauthenticated => {
            state.unlocked = Some(false);
            false
        }
        ListOutcome::Failed => false,
    }
}

/// A `rbw list` running on a worker thread.
///
/// Refreshing on the daemon's FIFO loop would block the next `show` for the
/// whole round-trip. The worker instead posts its result back, and the loop
/// folds it in when it is ready — a `show` that arrives meanwhile is served
/// from the warm in-memory cache instead of waiting.
pub struct BackgroundRefresh {
    rx: Receiver<(Option<Cache>, i64)>,
}

impl BackgroundRefresh {
    /// Spawn the worker; it completes even if the receiver is dropped.
    pub fn start(paths: Paths) -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let cache = match fetch_cache(&paths) {
                ListOutcome::Cache(cache) => Some(cache),
                _ => None,
            };
            let _ = tx.send((cache, now_secs()));
        });
        Self { rx }
    }

    /// Fold the worker's result in if it has arrived. `true` when it is done.
    pub fn try_apply(&self, state: &mut State) -> bool {
        match self.rx.try_recv() {
            Ok((Some(cache), at)) => {
                cache.apply(state);
                state.last_refresh = at;
                state.unlocked = Some(true);
                true
            }
            Ok((None, _)) => {
                state.unlocked = Some(false);
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => true,
        }
    }
}

/// `start_refresh`: kick off a background refresh when one is due.
///
/// Returns `None` while the cache is still fresh or while the vault is known to
/// be locked. Callers that already hold a [`BackgroundRefresh`] should not call
/// this — they keep their in-flight one instead.
pub fn start_refresh(state: &State, paths: &Paths) -> Option<BackgroundRefresh> {
    let now = now_secs();
    if now - state.last_refresh < paths.refresh_interval {
        return None;
    }
    if state.unlocked == Some(false) {
        return None;
    }
    Some(BackgroundRefresh::start(paths.clone()))
}

/// Pure `rbw list --raw` → `(menu, tsv)` transform.
///
/// `--raw` is used instead of `--fields id,name,user` because item names come
/// from the vault (and can be shared in from another account): a tab or newline
/// in a name must not break the cache the way a TSV parse would. Passwords are
/// never part of the raw listing.
///
/// Returns `None` when the output is not a JSON array (e.g. rbw printed an
/// error), so the caller can distinguish "the vault is empty" from "the vault
/// could not be read".
fn parse_cache(json: &[u8]) -> Option<Cache> {
    let items: Vec<Item> = serde_json::from_slice(json).ok()?;

    let mut tsv: Vec<(String, String)> = Vec::new();
    let mut labels: Vec<String> = Vec::new();

    for item in &items {
        // rbw lists every entry type; q-bw has always offered logins only.
        if item.kind.as_deref() != Some("Login") {
            continue;
        }
        let name = sanitize_label(&item.name.clone().unwrap_or_default());
        let username = sanitize_label(&item.user.clone().unwrap_or_default());

        if !name.is_empty() {
            let label = if username.is_empty() {
                name
            } else {
                format!("{name}  ({username})")
            };
            if let Some(id) = &item.id {
                tsv.push((label.clone(), id.clone()));
            }
            labels.push(label);
        }
    }

    Some(Cache {
        menu: labels.join("\n"),
        tsv,
    })
}

/// Replace control characters (newlines, tabs, escapes, …) with spaces.
///
/// Labels come from vault items, which can be created by another Bitwarden
/// user and shared into the collection. A newline would otherwise split one
/// item into two picker rows (and `menu` lines), and terminal control bytes
/// have no business in a label.
fn sanitize_label(label: &str) -> String {
    label
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

/// `rbw get --field <field> -- <id-or-name>`.
///
/// Used at pick time so the agent is asked for the secret with the current
/// unlock state, which makes a lock from another process (or from rbw itself)
/// take effect immediately.
pub fn rbw_get_field(paths: &Paths, field: &str, id_or_name: &str) -> Option<String> {
    let out = get_command(paths, field, id_or_name).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .trim_end_matches(['\n', '\r'])
            .to_string(),
    )
}

/// `rbw get --field <field> -- <id-or-name>`.
///
/// A free-typed name can start with `-`; without the `--` terminator, rbw
/// would parse it as an option (e.g. `--help`) instead of looking it up.
fn get_command(paths: &Paths, field: &str, id_or_name: &str) -> Command {
    let mut cmd = rbw_command(paths);
    cmd.args(["get", "--field", field, "--"]).arg(id_or_name);
    cmd
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::process::Command;

    use super::{encode_server, get_command, parse_cache, pinentry_is_shimable, rbw_path};
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

    fn args(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn get_uses_the_field_option_and_ends_option_parsing() {
        let paths = paths_in(std::path::Path::new("/tmp/qbw-test"));
        let cmd = get_command(&paths, "password", "--help");
        assert_eq!(
            args(&cmd),
            ["get", "--field", "password", "--", "--help"],
            "an item name that looks like a flag must still be looked up"
        );

        let cmd = get_command(&paths, "username", "58b1120e-48fa-4b5c-b530-b39500f1d0e7");
        assert_eq!(
            args(&cmd),
            [
                "get",
                "--field",
                "username",
                "--",
                "58b1120e-48fa-4b5c-b530-b39500f1d0e7"
            ]
        );
    }

    #[test]
    fn rbw_database_server_names_are_percent_encoded() {
        assert_eq!(encode_server("default"), "default");
        assert_eq!(
            encode_server("https://vault.example.com"),
            "https%3A%2F%2Fvault.example.com"
        );
        assert_eq!(
            encode_server("https://vault.example.com/"),
            "https%3A%2F%2Fvault.example.com%2F"
        );
    }

    #[test]
    fn only_the_default_pinentry_setting_uses_the_shim() {
        assert!(pinentry_is_shimable(None), "missing setting means default");
        assert!(
            pinentry_is_shimable(Some("")),
            "empty setting means default"
        );
        assert!(pinentry_is_shimable(Some("pinentry")));
        // An absolute or custom program ignores PATH and would bypass q-bw.
        assert!(!pinentry_is_shimable(Some("/usr/bin/pinentry")));
        assert!(!pinentry_is_shimable(Some("pinentry-gtk-2")));
    }

    #[test]
    fn rbw_path_puts_the_pinentry_shim_first() {
        let paths = paths_in(std::path::PathBuf::from("/tmp/qbw-test").as_path());
        let value = rbw_path(&paths);
        let value = PathBuf::from(value);
        let mut parts = std::env::split_paths(&value);
        assert_eq!(parts.next().unwrap(), paths.pinentry_dir());
        // The rest of the daemon's PATH is preserved for rbw-agent itself.
        if let Some(old) = std::env::var_os("PATH") {
            let old: Vec<PathBuf> = std::env::split_paths(&old).collect();
            let rest: Vec<PathBuf> = parts.collect();
            assert_eq!(rest, old);
        }
    }

    const ITEMS: &[u8] = br#"[
        {"id":"id-github","name":"GitHub","user":"octocat","folder":null,"uris":["https://github.com"],"type":"Login"},
        {"id":"id-gitlab","name":"GitLab","user":"","folder":null,"uris":null,"type":"Login"},
        {"id":"id-nouser","name":"NoUser","user":null,"folder":null,"uris":null,"type":"Login"},
        {"id":"id-nopass","name":"NoPass","user":"u","folder":null,"uris":null,"type":"Login"},
        {"id":"id-note","name":"A note","user":null,"folder":null,"uris":null,"type":"Note"},
        {"id":"id-card","name":"A card","user":null,"folder":null,"uris":null,"type":"Card"}
    ]"#;

    #[test]
    fn parse_cache_builds_menu_and_tsv_without_secrets() {
        let cache = parse_cache(ITEMS).expect("valid vault JSON");

        assert_eq!(cache.menu, "GitHub  (octocat)\nGitLab\nNoUser\nNoPass  (u)");
        assert_eq!(
            cache.tsv,
            vec![
                ("GitHub  (octocat)".to_string(), "id-github".to_string()),
                ("GitLab".to_string(), "id-gitlab".to_string()),
                ("NoUser".to_string(), "id-nouser".to_string()),
                ("NoPass  (u)".to_string(), "id-nopass".to_string()),
            ]
        );
        // Non-login entries are excluded, exactly like the old `type == 1`
        // filter, and the cache never contains a password.
        assert!(!cache.menu.contains("A note"));
        assert!(!cache.menu.contains("A card"));
    }

    #[test]
    fn parse_cache_accepts_an_empty_vault() {
        let cache = parse_cache(b"[]").expect("an empty array is valid");
        assert!(cache.menu.is_empty());
        assert!(cache.tsv.is_empty());
    }

    #[test]
    fn parse_cache_strips_control_characters_from_labels() {
        // A name shared by another user must not inject extra picker rows.
        let json = br#"[{"type":"Login","id":"id-x",
            "name":"Evil\nName\t(X)\u001b[31m",
            "user":"u\nser"}]"#;
        let cache = parse_cache(json).expect("valid vault JSON");

        assert!(
            cache.menu.chars().all(|c| !c.is_control()),
            "no control characters may reach the picker: {:?}",
            cache.menu
        );
        assert_eq!(cache.menu, "Evil Name (X) [31m  (u ser)");
        assert_eq!(cache.tsv.len(), 1);
    }

    #[test]
    fn parse_cache_rejects_invalid_json() {
        assert!(parse_cache(b"").is_none());
        assert!(parse_cache(b"\n").is_none());
        assert!(parse_cache(b"not json").is_none());
        // A JSON value that is not an item array is equally unusable.
        assert!(parse_cache(b"{}").is_none());
    }
}
