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
//! Scheduling is adaptive: each run arms a transient timer to wake ~1 min
//! before the earliest token crosses the threshold. The static hourly timer
//! is only the reboot-safe backstop.

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
    // Earliest expiry seen, for the adaptive re-arm.
    let mut min_expiry = 0i64;
    let mut track = |e: i64| {
        if e > 0 && (min_expiry == 0 || e < min_expiry) {
            min_expiry = e;
        }
    };
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
            track(expiry);
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
            track(expiry);
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
            track(expiry);
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
            if ok {
                track(expiry_for(&cache_dir, &session.start_url));
            } else {
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
    rearm(min_expiry, min_seconds, rc);
    rc
}

/// Adaptive re-arm: wake ~1 min before the earliest token dips under the
/// threshold. Failure retries in 10 min; no data falls back to the hourly
/// backstop timer. Clamped to [5 min, 6 h].
fn rearm(min_expiry: i64, min_seconds: i64, rc: i32) {
    let delay = if rc != 0 {
        600
    } else if min_expiry > 0 {
        (min_expiry - now() - min_seconds - 60).clamp(300, 21_600)
    } else {
        return; // nothing to schedule from: the hourly timer covers it
    };
    // Replace any previous transient timer, then arm the next wake-up.
    let _ = Command::new("systemctl")
        .args(["--user", "stop", "sso-guard-next.timer"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let armed = Command::new("systemd-run")
        .args([
            "--user",
            "--quiet",
            "--collect",
            "--unit",
            "sso-guard-next",
            &format!("--on-active={delay}s"),
            "systemctl",
            "--user",
            "start",
            "sso-guard.service",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if armed {
        println!("sso-guard: next check in {} min", delay / 60);
    }
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
