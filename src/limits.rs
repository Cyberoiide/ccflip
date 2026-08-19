//! Freshly-hit limit flags.
//!
//! cswap's polled usage lags a real limit by minutes (exhausted accounts poll
//! on backoff), so a revived session kept landing back on the limited account.
//! The definitive signal is the error Claude Code wrote into the transcript:
//! the watcher scans just-written transcripts, and an account that errored is
//! flagged unusable for a TTL. Every selection path honours the flag because
//! it rides on the account itself — see `cswap::apply_limit_flags`.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::cswap;

/// email -> unix time the flag expires.
pub type Flags = HashMap<String, i64>;

/// Text that means "this account is out of quota right now". Matched only
/// inside an API-ERROR message: these phrases also occur in ordinary
/// conversation (a session discussing limits), and flagging on prose would
/// push healthy sessions onto the paid tier for nothing.
const LIMIT_PHRASES: [&str; 2] = ["hit your session limit", "usage limit reached"];

/// Only the tail of a transcript can hold an error from the last two minutes.
const TAIL_BYTES: u64 = 16_384;

pub fn path() -> PathBuf {
    env::var_os("CCFLIP_LIMIT_FLAGS")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| cswap::home().join(".local/state/ccflip/limit-flags.json"))
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

pub fn load() -> Flags {
    fs::read_to_string(path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn flagged(flags: &Flags, email: &str) -> bool {
    flags.get(email).is_some_and(|expiry| *expiry > now())
}

/// Flag `email` unusable for `ttl` seconds, dropping expired entries on the
/// way. Returns false when it was already flagged — the caller logs only new
/// hits, so the journal shows one line per limit, not one per tick.
fn flag(email: &str, ttl: i64) -> bool {
    let mut flags = load();
    if flagged(&flags, email) {
        return false;
    }
    let now = now();
    flags.retain(|_, expiry| *expiry > now);
    flags.insert(email.to_string(), now + ttl);
    let file = path();
    if let Some(dir) = file.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string(&flags) {
        let tmp = file.with_extension("tmp");
        if fs::write(&tmp, json).is_ok() {
            let _ = fs::rename(&tmp, &file);
        }
    }
    true
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TranscriptLine {
    timestamp: Option<String>,
    /// Claude Code marks API failures with these; ordinary messages have
    /// neither, which is what keeps prose from flagging an account.
    #[serde(default)]
    is_api_error_message: bool,
    #[serde(default)]
    api_error_status: Option<serde_json::Value>,
    message: Option<TranscriptMessage>,
}

#[derive(Deserialize)]
struct TranscriptMessage {
    content: Option<Content>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Content {
    Text(String),
    Blocks(Vec<Block>),
    Other(serde::de::IgnoredAny),
}

#[derive(Deserialize)]
struct Block {
    #[serde(default)]
    text: String,
}

impl Content {
    fn mentions_limit(&self) -> bool {
        let hit = |s: &str| LIMIT_PHRASES.iter().any(|p| s.contains(p));
        match self {
            Content::Text(s) => hit(s),
            Content::Blocks(blocks) => blocks.iter().any(|b| hit(&b.text)),
            Content::Other(_) => false,
        }
    }
}

/// Scan transcripts written in the last `fresh_secs` for a limit error and
/// flag the account that hit it. Returns the emails newly flagged, for the
/// caller to log. `now` is a parameter so this is testable.
pub fn scan(list: &cswap::List, ttl: i64, fresh_secs: i64, now_epoch: i64) -> Vec<String> {
    let sessions = cswap::sessions_dir();
    let mut files = Vec::new();
    for root in [cswap::home().join(".claude/projects"), sessions.clone()] {
        recent_transcripts(&root, fresh_secs, now_epoch, &mut files);
    }

    let mut newly = Vec::new();
    for file in files {
        let Some(stamp) = last_limit_error(&file) else {
            continue;
        };
        // An old error can linger in a tail that was appended to for other
        // reasons; only a fresh one means "limited right now".
        if epoch(&stamp).is_none_or(|t| now_epoch - t >= fresh_secs) {
            continue;
        }
        let Some(email) = account_for(&file, &sessions, list) else {
            continue;
        };
        if flag(&email, ttl) {
            newly.push(email);
        }
    }
    newly
}

fn recent_transcripts(dir: &Path, fresh_secs: i64, now_epoch: i64, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            recent_transcripts(&path, fresh_secs, now_epoch, out);
            continue;
        }
        if path.extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        let fresh = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .is_some_and(|d| now_epoch - (d.as_secs() as i64) < fresh_secs);
        if fresh {
            out.push(path);
        }
    }
}

/// Timestamp of the last limit error in this transcript's tail, if any.
fn last_limit_error(path: &Path) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len > TAIL_BYTES {
        file.seek(SeekFrom::Start(len - TAIL_BYTES)).ok()?;
    }
    let mut tail = String::new();
    // Transcripts are UTF-8; a seek can land mid-character, so read bytes.
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    tail.push_str(&String::from_utf8_lossy(&bytes));

    tail.lines()
        .filter(|line| LIMIT_PHRASES.iter().any(|p| line.contains(p)))
        .filter_map(|line| serde_json::from_str::<TranscriptLine>(line).ok())
        .filter(|l| l.is_api_error_message || l.api_error_status.is_some())
        .filter(|l| {
            l.message
                .as_ref()
                .and_then(|m| m.content.as_ref())
                .is_some_and(Content::mentions_limit)
        })
        .filter_map(|l| l.timestamp)
        .next_back()
}

/// Which account wrote this transcript: a pinned session profile's live login,
/// else the default account.
fn account_for(file: &Path, sessions: &Path, list: &cswap::List) -> Option<String> {
    if let Ok(rest) = file.strip_prefix(sessions) {
        let profile = sessions.join(rest.components().next()?);
        return cswap::profile_email(&profile).or_else(|| {
            // Never used yet: fall back to the account it was seeded for.
            let name = profile.file_name()?.to_string_lossy().into_owned();
            let number = name.split('-').next()?.parse().ok()?;
            list.by_number(number).map(|a| a.email.clone())
        });
    }
    list.active_account().map(|a| a.email.clone())
}

/// ponytail: `date -d` for one RFC3339 stamp, and only for a file that
/// already contains a limit error — a datetime crate would earn its keep only
/// if timestamps needed real arithmetic.
fn epoch(stamp: &str) -> Option<i64> {
    let out = Command::new("date")
        .args(["-d", stamp, "+%s"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(kind: &str, text: &str, stamp: &str) -> String {
        let error_keys = match kind {
            "api_error" => r#""isApiErrorMessage":true,"apiErrorStatus":429,"#,
            _ => "",
        };
        format!(
            r#"{{"timestamp":"{stamp}",{error_keys}"message":{{"role":"assistant","content":[{{"type":"text","text":"{text}"}}]}}}}"#
        )
    }

    #[test]
    fn only_api_errors_flag_an_account() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let stamp = "2026-08-31T10:00:00.000Z";

        // Ordinary conversation that merely mentions a limit: this session's
        // own transcript looks like this, and flagging it would push healthy
        // sessions onto the paid tier for nothing.
        fs::write(
            &path,
            format!(
                "{}\n",
                line("prose", "we should handle: hit your session limit", stamp)
            ),
        )
        .unwrap();
        assert_eq!(last_limit_error(&path), None);

        // The real thing: an API error carrying the same phrase.
        fs::write(
            &path,
            format!(
                "{}\n{}\n",
                line("prose", "talking about usage limit reached", stamp),
                line("api_error", "API Error: 429 usage limit reached", stamp),
            ),
        )
        .unwrap();
        assert_eq!(last_limit_error(&path).as_deref(), Some(stamp));
    }

    #[test]
    fn flag_expiry_is_honoured_and_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("flags.json");
        // SAFETY: single-threaded test.
        unsafe { env::set_var("CCFLIP_LIMIT_FLAGS", &file) };

        assert!(flag("a@x.com", 900), "first hit flags");
        assert!(!flag("a@x.com", 900), "second hit is not news");
        assert!(flagged(&load(), "a@x.com"));
        assert!(!flagged(&load(), "b@x.com"));

        // An expired entry neither counts nor survives the next write.
        fs::write(&file, r#"{"old@x.com":1,"a@x.com":99999999999}"#).unwrap();
        assert!(!flagged(&load(), "old@x.com"));
        flag("c@x.com", 900);
        assert!(!load().contains_key("old@x.com"), "expired entry pruned");
        unsafe { env::remove_var("CCFLIP_LIMIT_FLAGS") };
    }
}
