//! Typed interface to cswap: `cswap list --json`, `mappings.json`, and the
//! session profile tree.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub enum UsageStatus {
    #[serde(rename = "ok")]
    Ok,
    /// Anything not "ok" (token_expired, …) — never usable, never a target.
    #[serde(other)]
    NotOk,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Window {
    pub pct: f64,
}

/// Spend meter on enterprise/API-billing accounts. `pct` may be absent on
/// older cswap versions — treated as "below any threshold", like jq's
/// `null < n`.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Spend {
    pub pct: Option<f64>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
    pub spend: Option<Spend>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub number: i64,
    pub email: String,
    /// The default login — what a plain `claude` launch uses.
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub disabled: bool,
    pub usage_status: UsageStatus,
    /// `Some` with no windows = spend-based (enterprise/API billing).
    /// `None` = no usage data at all (mid-poll): health unknown.
    pub usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct List {
    pub active_account_number: Option<i64>,
    pub accounts: Vec<Account>,
}

impl Account {
    /// Has 5h/7d usage windows (subscription billing).
    pub fn windowed(&self) -> bool {
        self.usage.as_ref().is_some_and(|u| u.five_hour.is_some())
    }

    pub fn spend_pct(&self) -> Option<f64> {
        self.usage
            .as_ref()
            .and_then(|u| u.spend)
            .and_then(|s| s.pct)
    }

    /// Worst window pct across 5h/7d; `None` when no window data.
    pub fn worst(&self) -> Option<f64> {
        let u = self.usage.as_ref()?;
        [u.five_hour, u.seven_day]
            .into_iter()
            .flatten()
            .map(|w| w.pct)
            .reduce(f64::max)
    }
}

impl List {
    pub fn by_number(&self, number: i64) -> Option<&Account> {
        self.accounts.iter().find(|a| a.number == number)
    }

    pub fn by_email(&self, email: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| a.email == email)
    }

    /// The default login. cswap emits both a top-level activeAccountNumber
    /// and a per-row active flag; ONE definition here for every consumer —
    /// prefer the number, fall back to the flag.
    pub fn active_account(&self) -> Option<&Account> {
        self.active_account_number
            .and_then(|n| self.by_number(n))
            .or_else(|| self.accounts.iter().find(|a| a.active))
    }
}

/// `cswap list --json`. `None` on any failure — callers fail open.
pub fn list() -> Option<List> {
    let out = Command::new("cswap")
        .args(["list", "--json"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    match serde_json::from_slice(&out.stdout) {
        Ok(list) => Some(list),
        Err(e) => {
            // Schema drift must be loud, but routing still fails open.
            eprintln!("ccflip: cannot parse `cswap list --json`: {e}");
            None
        }
    }
}

#[derive(Debug, Deserialize)]
struct MappingsFile {
    #[serde(default)]
    mappings: HashMap<String, Mapping>,
}

#[derive(Debug, Deserialize)]
struct Mapping {
    email: Option<String>,
}

/// Pinned account for `pwd`: longest mapping key equal to it or prefixing it.
pub fn mapped_email(pwd: &str) -> Option<String> {
    let path = env::var_os("CCFLIP_MAPPINGS")
        .filter(|v| !v.is_empty()) // set-but-empty falls back, like ${VAR:-}
        .map(PathBuf::from)
        .unwrap_or_else(|| data_home().join("claude-swap/mappings.json"));
    let text = fs::read_to_string(&path).ok()?; // no file = nothing pinned
    let file: MappingsFile = match serde_json::from_str(&text) {
        Ok(file) => file,
        Err(e) => {
            // A corrupt mappings file must not look like "no mapping": that
            // would silently route a pinned dir onto the default login.
            eprintln!("ccflip: cannot parse {}: {e}", path.display());
            return None;
        }
    };
    file.mappings
        .iter()
        .filter(|(dir, _)| Path::new(pwd).starts_with(dir))
        .max_by_key(|(dir, _)| dir.len())
        .and_then(|(_, m)| m.email.clone())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeJson {
    oauth_account: Option<OauthAccount>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OauthAccount {
    email_address: Option<String>,
}

/// The account a Claude Code config dir is logged into RIGHT NOW, from its
/// `.claude.json`. The session directory *name* is only the account it was
/// seeded with: the watcher rewrites credentials in place, so the name goes
/// stale the moment a profile rotates. Everything that reports or decides on
/// "which account is this profile on" reads this.
pub fn profile_email(config_dir: &Path) -> Option<String> {
    let text = fs::read_to_string(config_dir.join(".claude.json")).ok()?;
    serde_json::from_str::<ClaudeJson>(&text)
        .ok()?
        .oauth_account?
        .email_address
}

pub fn sessions_dir() -> PathBuf {
    data_home().join("claude-swap/sessions")
}

/// Session profile dirs, named `<account-number>-<email with @ as _>`.
pub fn session_dirs() -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(sessions_dir()) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir()) // follows symlinks on purpose
        .collect()
}

/// The session profile belonging to `email` — matched on its live login, or
/// on the seeded directory name when the profile has not been used yet.
pub fn profile_for_email(email: &str) -> Option<PathBuf> {
    let seeded_suffix = format!("-{}", email.replace('@', "_"));
    session_dirs().into_iter().find(|dir| {
        profile_email(dir).as_deref() == Some(email)
            || dir
                .file_name()
                .is_some_and(|n| n.to_string_lossy().ends_with(&seeded_suffix))
    })
}

pub fn home() -> PathBuf {
    env::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

pub fn data_home() -> PathBuf {
    env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/share"))
}
