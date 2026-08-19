//! `ccflip sso-guard` — keep every AWS SSO session fresh so Bedrock sessions
//! (and the litellm gateway, bastion tunnels, kubectl…) never die mid-flight
//! with "Token is expired".
//!
//! Sessions are discovered from ~/.aws/config ([sso-session] blocks plus the
//! first profile referencing each). Silent while an access token has life.
//! When one runs low, a plain STS call lets botocore mint a fresh token from
//! the SSO refresh token — no browser. Only when the refresh token itself is
//! dead does it open the browser login (one click against a live IdP
//! session), and only on a graphical session.
//!
//! ponytail: scheduling is a plain 15-minute systemd timer. It must stay
//! shorter than SSO_GUARD_MIN_MINUTES (45) — a poll slower than the renewal
//! threshold lets a token expire between runs. A self-arming transient timer
//! was tried and cannot chain: the run is started BY unit `sso-guard-next`,
//! so that unit is still active when the run tries to re-create it, and
//! `systemd-run` fails with a name collision. A run costs one JSON read when
//! nothing is due, so polling is cheaper than the machinery to avoid it.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::{cswap, env_parsed};

struct Session {
    name: String,
    start_url: String,
    profile: String,
}

/// One entry per [sso-session] that some profile actually references.
fn sessions(config: &str) -> Vec<Session> {
    #[derive(Default)]
    enum Section {
        SsoSession(String),
        Profile(String),
        #[default]
        Other,
    }
    let mut urls: Vec<(String, String)> = Vec::new(); // session -> start_url
    let mut first_profile: Vec<(String, String)> = Vec::new(); // session -> profile
    let mut section = Section::default();
    for line in config.lines() {
        let line = line.trim();
        if let Some(header) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            let (kind, name) = header
                .split_once(char::is_whitespace)
                .unwrap_or((header, ""));
            section = match kind {
                "sso-session" => Section::SsoSession(name.trim().to_string()),
                "profile" => Section::Profile(name.trim().to_string()),
                // `aws configure sso` writes the session into [default] on a
                // single-account setup — that profile consumes it too.
                "default" => Section::Profile("default".to_string()),
                _ => Section::Other,
            };
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match (&section, key) {
            (Section::SsoSession(name), "sso_start_url") => {
                urls.push((name.clone(), value.to_string()));
            }
            (Section::Profile(profile), "sso_session")
                if !first_profile.iter().any(|(s, _)| s == value) =>
            {
                first_profile.push((value.to_string(), profile.clone()));
            }
            _ => {}
        }
    }
    urls.into_iter()
        .filter_map(|(name, start_url)| {
            let profile = first_profile
                .iter()
                .find(|(s, _)| *s == name)
                .map(|(_, p)| p.clone())?; // nothing consumes this session
            Some(Session {
                name,
                start_url,
                profile,
            })
        })
        .collect()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CacheEntry {
    start_url: Option<String>,
    expires_at: Option<String>,
}

/// Newest access-token expiry for `start_url`, as an epoch second (0 = none).
fn expiry_for(cache_dir: &PathBuf, start_url: &str) -> i64 {
    let Ok(entries) = fs::read_dir(cache_dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| {
            let text = fs::read_to_string(e.path()).ok()?;
            let entry: CacheEntry = serde_json::from_str(&text).ok()?;
            (entry.start_url.as_deref() == Some(start_url)).then_some(entry.expires_at?)
        })
        .filter_map(|stamp| epoch(&stamp))
        .max()
        .unwrap_or(0)
}

/// ponytail: `date -d` instead of a datetime crate — the bash version's own
/// parser, and this binary already shells out for the month boundary. Swap in
/// chrono/jiff if timestamps ever need arithmetic beyond "is it past yet".
fn epoch(stamp: &str) -> Option<i64> {
    let out = Command::new("date")
        .args(["-d", stamp, "+%s"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Exit code: 1 if any session still needs an interactive login.
pub fn run() -> i32 {
    let min_seconds = env_parsed("SSO_GUARD_MIN_MINUTES", 45i64) * 60;
    let config_path = env::var_os("AWS_CONFIG_FILE")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| cswap::home().join(".aws/config"));
    let Ok(config) = fs::read_to_string(&config_path) else {
        return 0; // no AWS config = nothing to guard (instance-role host)
    };
    let cache_dir = cswap::home().join(".aws/sso/cache");

    let mut rc = 0;
    for session in sessions(&config) {
        let expiry = expiry_for(&cache_dir, &session.start_url);
        if expiry - now() > min_seconds {
            continue;
        }
        // Access token low: botocore mints a fresh one from the SSO refresh
        // token if the IdC session still lives — a plain STS call triggers it.
        let _ = Command::new("aws")
            .args(["sts", "get-caller-identity", "--profile", &session.profile])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let expiry = expiry_for(&cache_dir, &session.start_url);
        if expiry - now() > min_seconds {
            println!(
                "sso-guard: {} access token refreshed silently",
                session.name
            );
            continue;
        }
        // STS can be served from cached role credentials without touching the
        // token, so "no refresh" does not mean "dead". Only interrupt a human
        // once the token really is spent — otherwise wait and re-check.
        if expiry - now() > 0 {
            println!(
                "sso-guard: {} not refreshed yet, token still valid for {} min — waiting",
                session.name,
                (expiry - now()) / 60
            );
            continue;
        }
        // Refresh token itself is dead: only a human can fix it.
        let graphical = ["WAYLAND_DISPLAY", "DISPLAY"]
            .iter()
            .any(|v| env::var_os(v).is_some_and(|x| !x.is_empty()));
        if graphical {
            let _ = Command::new("notify-send")
                .args([
                    "-u",
                    "critical",
                    "sso-guard",
                    &format!(
                        "AWS SSO session '{}' expiring — complete the login in the browser",
                        session.name
                    ),
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let ok = Command::new("aws")
                .args(["sso", "login", "--sso-session", &session.name])
                .status()
                .is_ok_and(|s| s.success());
            if !ok {
                rc = 1;
            }
        } else {
            eprintln!(
                "sso-guard: '{}' needs interactive login (no display): aws sso login --sso-session {}",
                session.name, session.name
            );
            rc = 1;
        }
    }
    rc
}

#[cfg(test)]
mod tests {
    use super::sessions;

    #[test]
    fn discovers_sessions_with_a_consumer() {
        let config = "\
[default]
region = eu-west-1

[sso-session sia]
sso_start_url = https://sia.awsapps.com/start
sso_region = eu-west-1

[sso-session orphan]
sso_start_url = https://nobody.awsapps.com/start

[profile code-assist-admin]
sso_session = sia
sso_account_id = 1234

[profile second-on-same-session]
sso_session = sia
";
        let found = sessions(config);
        // The orphan session (no profile references it) is skipped, and the
        // FIRST profile wins for a session with several.
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "sia");
        assert_eq!(found[0].start_url, "https://sia.awsapps.com/start");
        assert_eq!(found[0].profile, "code-assist-admin");
    }
}
