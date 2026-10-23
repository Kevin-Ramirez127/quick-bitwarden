// SPDX-License-Identifier: GPL-3.0-or-later
//! Bitwarden CLI access: unlocking, the in-memory cache and password lookup.

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};

use serde::Deserialize;

use crate::paths::Paths;
use crate::proc::now_secs;
use crate::ui;

/// Everything the daemon keeps hot between invocations.
#[derive(Default)]
pub struct State {
    /// Picker labels, one per line (`cut -f1` of `tsv`).
    pub menu: String,
    /// `(label, id)` pairs, in vault order.
    pub tsv: Vec<(String, String)>,
    /// `id -> password`, kept in memory only.
    pub pass: HashMap<String, String>,
    /// `id -> username`, kept in memory only.
    pub user: HashMap<String, String>,
    /// Unix time of the last successful cache refresh.
    pub last_refresh: i64,
    /// Current `BW_SESSION` token, if any.
    pub session: Option<String>,
    /// Cached vault lock state: `Some(true)` unlocked, `Some(false)` locked or
    /// unauthenticated, `None` unknown. Lets us skip a `bw status` round-trip
    /// whenever we already know the answer.
    pub unlocked: Option<bool>,
}

#[derive(Deserialize)]
struct Item {
    #[serde(rename = "type")]
    kind: Option<i64>,
    name: Option<String>,
    id: Option<String>,
    login: Option<Login>,
}

#[derive(Deserialize)]
struct Login {
    username: Option<String>,
    password: Option<String>,
}

/// `bw status --raw | jq -r '.status // "unauthenticated"'`.
pub fn bw_status(session: Option<&str>) -> String {
    let mut cmd = Command::new("bw");
    cmd.args(["status", "--raw"]);
    if let Some(s) = session {
        cmd.env("BW_SESSION", s);
    }
    match cmd.output() {
        Ok(out) if out.status.success() => {
            let value: serde_json::Value =
                serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null);
            value
                .get("status")
                .and_then(|s| s.as_str())
                .unwrap_or("unauthenticated")
                .to_string()
        }
        _ => "unauthenticated".to_string(),
    }
}

/// `bw status --raw | jq -r '.status'` for `status` (defaults to `"unknown"`).
pub fn bw_status_status_only(session: Option<&str>) -> String {
    let mut cmd = Command::new("bw");
    cmd.args(["status", "--raw"]);
    if let Some(s) = session {
        cmd.env("BW_SESSION", s);
    }
    match cmd.output() {
        Ok(out) if out.status.success() => {
            let value: serde_json::Value =
                serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null);
            value
                .get("status")
                .and_then(|s| s.as_str())
                .unwrap_or("unknown")
                .to_string()
        }
        _ => "unknown".to_string(),
    }
}

/// Load the session token file into `state` without contacting the vault or
/// prompting for anything.
pub fn load_session(state: &mut State, paths: &Paths) {
    if let Ok(s) = std::fs::read_to_string(&paths.session) {
        state.session = Some(s.trim_end_matches(['\n', '\r']).to_string());
    }
}

/// Outcome of a cache load that never prompts.
pub enum Load {
    /// The vault is unlocked and the in-memory cache is populated.
    Loaded,
    /// The vault is locked; the caller must run [`prompt_and_unlock`].
    NeedsPassword,
    /// Not logged in (`bw login` is required).
    Unauthenticated,
    /// `bw` failed for another reason.
    Failed,
}

/// `needs_prompt`: true when we already know a password dialog is required.
///
/// Deliberately conservative: a stored session that turns out to be stale is
/// handled by [`try_load`] returning [`Load::NeedsPassword`], not by probing
/// here (a `bw status` would add ~1.5 s to the common cold show).
pub fn needs_prompt(state: &mut State, paths: &Paths) -> bool {
    load_session(state, paths);
    state.session.is_none() || state.unlocked == Some(false)
}

/// `try_load`: populate the cache without ever prompting.
///
/// The unlock check is folded into the cache load: when a session token is
/// stored, `bw list items` doubles as the probe, so an already-unlocked vault
/// costs a single `bw` call instead of `bw status` **plus** `bw list items`.
pub fn try_load(state: &mut State, paths: &Paths) -> Load {
    load_session(state, paths);

    // Already warm with data (e.g. `sync`): no need to re-list first.
    if state.unlocked == Some(true) && !state.menu.is_empty() {
        return Load::Loaded;
    }

    // A stored session lets `bw list items` double as the unlock probe. Skip it
    // when we already know the vault is locked, to avoid a doomed `bw` call.
    if state.unlocked != Some(false) && state.session.is_some() && refresh_cache(state, paths) {
        state.unlocked = Some(true);
        return Load::Loaded;
    }

    match bw_status(state.session.as_deref()).as_str() {
        "unlocked" => {
            state.unlocked = Some(true);
            if refresh_cache(state, paths) {
                Load::Loaded
            } else {
                Load::Failed
            }
        }
        "locked" => {
            state.unlocked = Some(false);
            Load::NeedsPassword
        }
        "unauthenticated" => {
            state.unlocked = Some(false);
            Load::Unauthenticated
        }
        _ => Load::Failed,
    }
}

/// `prompt_and_unlock`: ask for the master password and store the session.
///
/// Does **not** load the cache: the caller owns the loading window, so it can
/// open it before the (slow) `bw list items` that follows.
pub fn prompt_and_unlock(state: &mut State, paths: &Paths) -> bool {
    load_session(state, paths);

    // Re-probe: the vault may have been unlocked since `needs_prompt`.
    match bw_status(state.session.as_deref()).as_str() {
        "unauthenticated" => {
            ui::error_dialog(paths, "Not logged in. Run 'bw login' first.");
            state.unlocked = Some(false);
            return false;
        }
        "unlocked" => {
            state.unlocked = Some(true);
            return true;
        }
        _ => {}
    }

    let mut password = match ui::ask_password(paths, "Master password") {
        Some(p) if !p.is_empty() => p,
        // Cancelled: remember the vault is locked so the background refresh
        // does not spend a `bw` call probing it again.
        _ => {
            state.unlocked = Some(false);
            return false;
        }
    };
    let token = bw_unlock(&password);
    // Do not leave the master password in freed heap memory (or a core dump).
    wipe_string(&mut password);
    match token {
        Some(token) => {
            let _ = std::fs::write(&paths.session, token.as_bytes());
            state.session = Some(token);
            state.unlocked = Some(true);
            true
        }
        None => {
            ui::error_dialog(paths, "Could not unlock the vault: wrong master password?");
            false
        }
    }
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
        Load::NeedsPassword => prompt_and_unlock(state, paths) && refresh_cache(state, paths),
        Load::Unauthenticated => {
            ui::error_dialog(paths, "Not logged in. Run 'bw login' first.");
            false
        }
        Load::Failed => {
            ui::error_dialog(paths, "Could not read the vault");
            false
        }
    }
}

/// `bw unlock --passwordenv … --raw`: return the session token.
///
/// The password is passed through the environment, never argv: a positional
/// password is world-readable in `/proc/<pid>/cmdline` (and `ps`) for as long as
/// `bw` runs.
fn bw_unlock(password: &str) -> Option<String> {
    let out = unlock_command(password).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .trim_end_matches(['\n', '\r'])
            .to_string(),
    )
}

/// Name of the environment variable read by `bw unlock --passwordenv`.
const PASSWORD_ENV: &str = "QBW_MASTER_PASSWORD";

fn unlock_command(password: &str) -> Command {
    let mut cmd = Command::new("bw");
    cmd.args(["unlock", "--passwordenv", PASSWORD_ENV, "--raw"])
        .env(PASSWORD_ENV, password)
        .stdin(Stdio::null());
    cmd
}

/// Overwrite a secret string before dropping it.
fn wipe_string(s: &mut String) {
    // SAFETY: writing zeroes keeps the bytes valid for the buffer's capacity and
    // nothing reads the string concurrently; `clear` then drops the logical
    // contents. `write_volatile` keeps the wipe from being optimised away.
    unsafe {
        for byte in s.as_bytes_mut() {
            std::ptr::write_volatile(byte, 0);
        }
    }
    s.clear();
}

/// `bw sync`: pull the latest vault from the server. `true` on success.
pub fn bw_sync(session: Option<&str>) -> bool {
    let mut cmd = Command::new("bw");
    cmd.arg("sync");
    if let Some(s) = session {
        cmd.env("BW_SESSION", s);
    }
    cmd.output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// `bw lock`: lock the vault so the next `show` needs the master password.
pub fn bw_lock(session: Option<&str>) -> bool {
    let mut cmd = Command::new("bw");
    cmd.arg("lock");
    if let Some(s) = session {
        cmd.env("BW_SESSION", s);
    }
    cmd.output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// The parse result of one `bw list items`, before it is folded into [`State`].
struct Cache {
    menu: String,
    tsv: Vec<(String, String)>,
    pass: HashMap<String, String>,
    user: HashMap<String, String>,
}

impl Cache {
    fn apply(self, state: &mut State) {
        state.menu = self.menu;
        state.tsv = self.tsv;
        state.pass = self.pass;
        state.user = self.user;
    }
}

/// `bw list items` → cache tables. `None` when `bw` itself fails.
fn fetch_cache(session: Option<&str>) -> Option<Cache> {
    let mut cmd = Command::new("bw");
    cmd.args(["list", "items"]);
    if let Some(s) = session {
        cmd.env("BW_SESSION", s);
    }
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    // A stale session makes `bw list items` exit 0 with an empty stdout while
    // it asks for the master password on stderr (stdin is closed here, so the
    // prompt fails). Treat anything that is not a JSON array as a failed load,
    // so callers fall back to `bw status` and prompt for the password instead
    // of reporting an empty vault.
    parse_cache(&out.stdout)
}

/// `refresh_cache`: rebuild the menu/tsv/password tables from `bw list items`,
/// stamping `last_refresh` on success so callers cannot forget to.
/// Returns `false` when `bw` fails or its output is not a usable item array.
pub fn refresh_cache(state: &mut State, paths: &Paths) -> bool {
    let _ = paths;
    match fetch_cache(state.session.as_deref()) {
        Some(cache) => {
            cache.apply(state);
            state.last_refresh = now_secs();
            true
        }
        None => false,
    }
}

/// A `bw list items` running on a worker thread.
///
/// Refreshing on the daemon's FIFO loop would block the next `show` for the
/// whole (multi-second) `bw` round-trip. The worker instead posts its result
/// back, and the loop folds it in when it is ready — a `show` that arrives
/// meanwhile is served from the warm in-memory cache instead of waiting.
pub struct BackgroundRefresh {
    rx: Receiver<(Option<Cache>, i64)>,
}

impl BackgroundRefresh {
    /// Spawn the worker; it completes even if the receiver is dropped.
    pub fn start(session: Option<String>) -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let cache = fetch_cache(session.as_deref());
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
    Some(BackgroundRefresh::start(state.session.clone()))
}

/// Pure `bw list items` → `(menu, tsv, id->password)` transform, matching the
/// `jq` filters of the original script.
///
/// Returns `None` when the output is not a JSON array (e.g. `bw` exited 0 after
/// failing to read a master password), so the caller can distinguish "the vault
/// is empty" from "the vault could not be read".
fn parse_cache(json: &[u8]) -> Option<Cache> {
    let items: Vec<Item> = serde_json::from_slice(json).ok()?;

    let mut tsv: Vec<(String, String)> = Vec::new();
    let mut labels: Vec<String> = Vec::new();
    let mut pass: HashMap<String, String> = HashMap::new();
    let mut user: HashMap<String, String> = HashMap::new();

    for item in &items {
        if item.kind != Some(1) {
            continue;
        }
        let name = item.name.clone().unwrap_or_default();
        let username = item
            .login
            .as_ref()
            .and_then(|l| l.username.clone())
            .unwrap_or_default();

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

        if let Some(pw) = item.login.as_ref().and_then(|l| l.password.clone()) {
            if !pw.is_empty() {
                if let Some(id) = &item.id {
                    pass.insert(id.clone(), pw);
                }
            }
        }

        // Usernames are only needed for `Shift+Enter`; keep them out when empty.
        if let Some(name) = item.login.as_ref().and_then(|l| l.username.clone()) {
            if !name.is_empty() {
                if let Some(id) = &item.id {
                    user.insert(id.clone(), name);
                }
            }
        }
    }

    Some(Cache {
        menu: labels.join("\n"),
        tsv,
        pass,
        user,
    })
}

/// `bw get password <id-or-name>` fallback.
pub fn bw_get_password(session: Option<&str>, id_or_name: &str) -> Option<String> {
    bw_get(session, "password", id_or_name)
}

/// `bw get username <id-or-name>` fallback, used by `Shift+Enter`.
pub fn bw_get_username(session: Option<&str>, id_or_name: &str) -> Option<String> {
    bw_get(session, "username", id_or_name)
}

/// `bw get <field> <id-or-name>`.
fn bw_get(session: Option<&str>, field: &str, id_or_name: &str) -> Option<String> {
    let mut cmd = Command::new("bw");
    cmd.args(["get", field, id_or_name]);
    if let Some(s) = session {
        cmd.env("BW_SESSION", s);
    }
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .trim_end_matches(['\n', '\r'])
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::{parse_cache, unlock_command, wipe_string, PASSWORD_ENV};

    #[test]
    fn unlock_passes_the_password_via_env_not_argv() {
        let cmd = unlock_command("s3cret");
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, ["unlock", "--passwordenv", PASSWORD_ENV, "--raw"]);
        assert!(
            !args.iter().any(|a| a.contains("s3cret")),
            "the master password must never be an argument"
        );

        let envs: Vec<(String, Option<String>)> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert_eq!(
            envs,
            vec![(PASSWORD_ENV.to_string(), Some("s3cret".to_string()))]
        );
    }

    #[test]
    fn wipe_string_zeroes_the_buffer() {
        let mut s = String::from("master-password");
        let len = s.len();
        wipe_string(&mut s);
        assert!(s.is_empty());
        // The first `len` bytes were initialised, then overwritten with zeroes.
        let bytes = unsafe { std::slice::from_raw_parts(s.as_ptr(), len) };
        assert!(bytes.iter().all(|b| *b == 0));
    }

    const ITEMS: &[u8] = br#"[
        {"type":1,"id":"id-github","name":"GitHub","login":{"username":"octocat","password":"gh-secret"}},
        {"type":1,"id":"id-gitlab","name":"GitLab","login":{"username":"","password":"gl-secret"}},
        {"type":1,"id":"id-nopass","name":"NoPass","login":{"username":"u","password":""}},
        {"type":2,"id":"id-note","name":"A note","notes":"x"}
    ]"#;

    #[test]
    fn parse_cache_builds_menu_tsv_and_passwords() {
        let cache = parse_cache(ITEMS).expect("valid vault JSON");

        assert_eq!(cache.menu, "GitHub  (octocat)\nGitLab\nNoPass  (u)");
        assert_eq!(
            cache.tsv,
            vec![
                ("GitHub  (octocat)".to_string(), "id-github".to_string()),
                ("GitLab".to_string(), "id-gitlab".to_string()),
                ("NoPass  (u)".to_string(), "id-nopass".to_string()),
            ]
        );
        assert_eq!(
            cache.pass.get("id-github").map(String::as_str),
            Some("gh-secret")
        );
        assert_eq!(
            cache.pass.get("id-gitlab").map(String::as_str),
            Some("gl-secret")
        );
        // Empty passwords and non-login items stay out of the password table.
        assert!(!cache.pass.contains_key("id-nopass"));
        assert!(!cache.pass.contains_key("id-note"));

        assert_eq!(
            cache.user.get("id-github").map(String::as_str),
            Some("octocat")
        );
        assert_eq!(cache.user.get("id-nopass").map(String::as_str), Some("u"));
        // Empty usernames stay out too.
        assert!(!cache.user.contains_key("id-gitlab"));
        assert!(!cache.user.contains_key("id-note"));
    }

    #[test]
    fn parse_cache_accepts_an_empty_vault() {
        let cache = parse_cache(b"[]").expect("an empty array is valid");
        assert!(cache.menu.is_empty());
        assert!(cache.tsv.is_empty());
        assert!(cache.pass.is_empty());
        assert!(cache.user.is_empty());
    }

    #[test]
    fn parse_cache_rejects_invalid_json() {
        // `bw list items` with a stale session exits 0 with an empty stdout,
        // so this must be a failed load rather than an empty vault.
        assert!(parse_cache(b"").is_none());
        assert!(parse_cache(b"\n").is_none());
        assert!(parse_cache(b"not json").is_none());
        // A JSON value that is not an item array is equally unusable.
        assert!(parse_cache(b"{}").is_none());
    }
}
