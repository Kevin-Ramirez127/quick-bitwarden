// SPDX-License-Identifier: GPL-3.0-or-later
//! Bitwarden CLI access: unlocking, the in-memory cache and password lookup.

use std::collections::HashMap;
use std::process::Command;

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
    /// Unix time of the last successful cache refresh.
    pub last_refresh: i64,
    /// Current `BW_SESSION` token, if any.
    pub session: Option<String>,
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

/// `ensure_unlocked`: load the session, then unlock/log in as needed.
pub fn ensure_unlocked(state: &mut State, paths: &Paths) -> bool {
    if let Ok(s) = std::fs::read_to_string(&paths.session) {
        state.session = Some(s.trim_end_matches(['\n', '\r']).to_string());
    }

    match bw_status(state.session.as_deref()).as_str() {
        "unlocked" => true,
        "locked" => {
            let password = match ui::ask_password(paths, "Master password") {
                Some(p) if !p.is_empty() => p,
                _ => return false,
            };
            match bw_unlock(&password) {
                Some(token) => {
                    let _ = std::fs::write(&paths.session, token.as_bytes());
                    state.session = Some(token);
                    true
                }
                None => {
                    ui::error_dialog(paths, "Could not unlock the vault: wrong master password?");
                    false
                }
            }
        }
        "unauthenticated" => {
            ui::error_dialog(paths, "Not logged in. Run 'bw login' first.");
            false
        }
        other => {
            ui::error_dialog(paths, &format!("Unexpected bw status: {other}"));
            false
        }
    }
}

fn bw_unlock(password: &str) -> Option<String> {
    let out = Command::new("bw")
        .args(["unlock", password, "--raw"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .trim_end_matches(['\n', '\r'])
            .to_string(),
    )
}

/// `refresh_cache`: rebuild the menu/tsv/password tables from `bw list items`.
/// Returns `false` only when `bw` itself fails.
pub fn refresh_cache(state: &mut State, paths: &Paths) -> bool {
    let _ = paths;
    let mut cmd = Command::new("bw");
    cmd.args(["list", "items"]);
    if let Some(s) = state.session.as_deref() {
        cmd.env("BW_SESSION", s);
    }
    let out = match cmd.output() {
        Ok(out) if out.status.success() => out,
        _ => return false,
    };

    let (menu, tsv, pass) = parse_cache(&out.stdout);
    state.menu = menu;
    state.tsv = tsv;
    state.pass = pass;
    true
}

/// Pure `bw list items` → `(menu, tsv, id->password)` transform, matching the
/// `jq` filters of the original script. Invalid JSON yields an empty cache.
pub fn parse_cache(json: &[u8]) -> (String, Vec<(String, String)>, HashMap<String, String>) {
    let items: Vec<Item> = serde_json::from_slice(json).unwrap_or_default();

    let mut tsv: Vec<(String, String)> = Vec::new();
    let mut labels: Vec<String> = Vec::new();
    let mut pass: HashMap<String, String> = HashMap::new();

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
    }

    (labels.join("\n"), tsv, pass)
}

/// `refresh_if_stale`: refresh at most once per `QBW_REFRESH_INTERVAL`.
pub fn refresh_if_stale(state: &mut State, paths: &Paths) {
    let now = now_secs();
    if now - state.last_refresh < paths.refresh_interval {
        return;
    }
    if !ensure_unlocked(state, paths) {
        return;
    }
    if refresh_cache(state, paths) {
        state.last_refresh = now;
    }
}

/// `bw get password <id-or-name>` fallback.
pub fn bw_get_password(session: Option<&str>, id_or_name: &str) -> Option<String> {
    let mut cmd = Command::new("bw");
    cmd.args(["get", "password", id_or_name]);
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
    use super::parse_cache;

    const ITEMS: &[u8] = br#"[
        {"type":1,"id":"id-github","name":"GitHub","login":{"username":"octocat","password":"gh-secret"}},
        {"type":1,"id":"id-gitlab","name":"GitLab","login":{"username":"","password":"gl-secret"}},
        {"type":1,"id":"id-nopass","name":"NoPass","login":{"username":"u","password":""}},
        {"type":2,"id":"id-note","name":"A note","notes":"x"}
    ]"#;

    #[test]
    fn parse_cache_builds_menu_tsv_and_passwords() {
        let (menu, tsv, pass) = parse_cache(ITEMS);

        assert_eq!(menu, "GitHub  (octocat)\nGitLab\nNoPass  (u)");
        assert_eq!(
            tsv,
            vec![
                ("GitHub  (octocat)".to_string(), "id-github".to_string()),
                ("GitLab".to_string(), "id-gitlab".to_string()),
                ("NoPass  (u)".to_string(), "id-nopass".to_string()),
            ]
        );
        assert_eq!(pass.get("id-github").map(String::as_str), Some("gh-secret"));
        assert_eq!(pass.get("id-gitlab").map(String::as_str), Some("gl-secret"));
        // Empty passwords and non-login items stay out of the password table.
        assert!(!pass.contains_key("id-nopass"));
        assert!(!pass.contains_key("id-note"));
    }

    #[test]
    fn parse_cache_tolerates_invalid_json() {
        let (menu, tsv, pass) = parse_cache(b"not json");
        assert!(menu.is_empty());
        assert!(tsv.is_empty());
        assert!(pass.is_empty());
    }
}
