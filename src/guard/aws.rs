//! AWS IAM Identity Center sessions, discovered from ~/.aws/config
//! ([sso-session] blocks and the profiles bound to each).
//!
//! Silent while an access token has life. When one runs low, the cached SSO
//! refresh token is redeemed directly (OIDC `CreateToken`, no browser) and
//! written back into the CLI cache. Only when the IdC session itself has
//! ended does it try one browser login — click-free against a live IdP
//! session — and after that a human is asked via a notification.

use std::collections::HashSet;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde_json::{Value, json};

use super::{Account, Provider, Run, ask_login, clear_needs_you, needs_you};
use crate::{cswap, epoch, now, stamp};

pub struct Aws;

struct Profile {
    name: String,
    account_id: Option<String>,
}

struct Session {
    name: String,
    start_url: String,
    profiles: Vec<Profile>,
}

/// Sessions some profile actually consumes, in config order.
fn sessions(config: &str) -> Vec<Session> {
    enum Section {
        SsoSession(String),
        Profile(String),
        Other,
    }
    let mut out: Vec<Session> = Vec::new();
    let mut profiles: Vec<(String, Profile)> = Vec::new(); // (session, profile)
    let mut section = Section::Other;
    for line in config.lines().map(str::trim) {
        if let Some(header) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            let (kind, name) = header
                .split_once(char::is_whitespace)
                .unwrap_or((header, ""));
            let name = name.trim().to_string();
            section = match kind {
                "sso-session" => Section::SsoSession(name),
                "profile" => Section::Profile(name),
                // `aws configure sso` writes a single-account setup into
                // [default] — that profile consumes its session too.
                "default" => Section::Profile("default".to_string()),
                _ => Section::Other,
            };
            if let Section::Profile(p) = &section {
                profiles.push((
                    String::new(),
                    Profile {
                        name: p.clone(),
                        account_id: None,
                    },
                ));
            }
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().to_string();
        match (&section, key.trim()) {
            (Section::SsoSession(name), "sso_start_url") => out.push(Session {
                name: name.clone(),
                start_url: value,
                profiles: Vec::new(),
            }),
            (Section::Profile(_), "sso_session") => profiles.last_mut().unwrap().0 = value,
            (Section::Profile(_), "sso_account_id") => {
                profiles.last_mut().unwrap().1.account_id = Some(value);
            }
            _ => {}
        }
    }
    for (session, profile) in profiles {
        if let Some(s) = out.iter_mut().find(|s| s.name == session) {
            s.profiles.push(profile);
        }
    }
    out.retain(|s| !s.profiles.is_empty()); // nothing consumes this session
    out
}

/// No AWS config = nothing to guard (instance-role host).
fn configured_sessions() -> Vec<Session> {
    let config = env::var_os("AWS_CONFIG_FILE")
        .filter(|v| !v.is_empty())
        .map_or_else(|| cswap::home().join(".aws/config"), PathBuf::from);
    fs::read_to_string(config).map_or_else(|_| Vec::new(), |c| sessions(&c))
}

fn cache_dir() -> PathBuf {
    cswap::home().join(".aws/sso/cache")
}

impl Provider for Aws {
    fn id(&self) -> &'static str {
        "aws"
    }

    fn label(&self) -> &'static str {
        "AWS SSO"
    }

    fn check(&self, run: &mut Run) {
        check(self, run);
    }

    fn status(&self, run: &Run) -> Vec<Account> {
        let cache = cache_dir();
        configured_sessions()
            .into_iter()
            .map(|s| {
                let e = expiry_for(&cache, &s.start_url);
                Account {
                    provider: self.id(),
                    state: run.state(e),
                    expires_at: (e > 0).then(|| stamp(e)).flatten(),
                    needs_you: needs_you(self, &s.name).exists(),
                    account: s.name,
                }
            })
            .collect()
    }

    fn login(&self, session: &str) -> bool {
        Command::new("aws")
            .args(["sso", "login", "--sso-session", session])
            .status()
            .is_ok_and(|s| s.success())
    }
}

fn check(aws: &Aws, run: &mut Run) {
    // Sessions allowed to open a browser login when their IdC session ends
    // (space-separated; "*" = all). Others are re-logged only while in use:
    // an org that caps sessions at 1h would otherwise mean a login per hour
    // for a session nothing needs.
    let browser = run
        .policy
        .text("SSO_GUARD_BROWSER")
        .unwrap_or_else(|| "*".to_string());
    let graphical = ["WAYLAND_DISPLAY", "DISPLAY"]
        .iter()
        .any(|v| env::var_os(v).is_some_and(|x| !x.is_empty()));
    let cache = cache_dir();

    for s in configured_sessions() {
        let name = s.name.as_str();
        let e = expiry_for(&cache, &s.start_url);
        // +90s slack: the re-arm wakes 60s before the threshold, so act now
        // instead of sleeping 1 more minute and waking again.
        if e - run.now > run.min_seconds + 90 {
            clear_needs_you(aws, name);
            run.track(e);
            continue;
        }

        // Decide before spending a refresh call on a dead, unused session.
        let gated = browser != "*" && !browser.split_whitespace().any(|b| b == name);
        let why = if gated {
            in_use(&s).or_else(|| used_recently(&s))
        } else {
            None
        };
        if gated && why.is_none() && e <= run.now {
            println!(
                "sso-guard: {name} expired, unused, letting it lapse (aws sso login --sso-session {name})"
            );
            continue;
        }

        if refresh(&cache, &s.start_url) {
            let e = expiry_for(&cache, &s.start_url);
            // A full refresh buys ~60 min. Less means the IdC session itself
            // ends at `e`: refreshes or a browser login before then reuse that
            // same session and buy nothing, so sleep until it ends.
            if e - run.now > 3000 {
                println!("sso-guard: {name} access token refreshed silently");
                run.track(e);
            } else {
                println!("sso-guard: {name} session ends {}, login then", clock(e));
                run.want(e + 30);
            }
            continue;
        }

        if gated {
            let Some(why) = why else {
                println!(
                    "sso-guard: {name} session over, unused, letting it lapse (aws sso login --sso-session {name})"
                );
                continue;
            };
            println!("sso-guard: {name} in use ({why}), re-login");
        }

        if !graphical {
            eprintln!(
                "sso-guard: '{name}' needs interactive login (no display): aws sso login --sso-session {name}"
            );
            run.failed = true;
        } else if needs_you(aws, name).exists() {
            ask_login(aws, name);
        } else if aws.login(name) {
            // One click-free browser attempt worked.
            run.track(expiry_for(&cache, &s.start_url));
        } else {
            // Timed out: the browser IdP session is gone too. Ask once via a
            // notification instead of opening a new tab on every run.
            println!(
                "sso-guard: {name} browser login timed out, asking via notification instead of reopening the browser"
            );
            let _ = fs::write(needs_you(aws, name), "");
            ask_login(aws, name);
        }
    }
}

fn cache_entries(cache: &Path) -> Vec<(PathBuf, Value)> {
    let Ok(dir) = fs::read_dir(cache) else {
        return Vec::new();
    };
    dir.flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| Some((p.clone(), serde_json::from_slice(&fs::read(&p).ok()?).ok()?)))
        .collect()
}

/// Newest access-token expiry for `start_url`, as an epoch second (0 = none).
fn expiry_for(cache: &Path, start_url: &str) -> i64 {
    cache_entries(cache)
        .iter()
        .filter(|(_, v)| v["startUrl"] == start_url)
        .filter_map(|(_, v)| epoch(v["expiresAt"].as_str()?))
        .max()
        .unwrap_or(0)
}

/// Redeem the cached SSO refresh token (refresh_token grant) and write the
/// new access token back into the cache file botocore reads. An STS call
/// does NOT do this: it is served from cached role creds and never touches
/// the SSO token. Posting ourselves also keeps the refresh token and client
/// secret out of argv, where any local user could read them.
fn refresh(cache: &Path, start_url: &str) -> bool {
    cache_entries(cache)
        .into_iter()
        .filter(|(_, v)| v["startUrl"] == start_url)
        .any(|(path, mut v)| {
            let Some(r) = create_token(&v) else {
                return false;
            };
            let Some(expires_in) = r["expiresIn"].as_i64() else {
                return false;
            };
            let Some(expires_at) = stamp(now() + expires_in) else {
                return false;
            };
            v["accessToken"] = r["accessToken"].clone();
            if r["refreshToken"].is_string() {
                v["refreshToken"] = r["refreshToken"].clone();
            }
            v["expiresAt"] = expires_at.into();
            write_private(&path, v.to_string().as_bytes()).is_ok()
        })
}

fn create_token(cached: &Value) -> Option<Value> {
    let field = |k: &str| cached[k].as_str().filter(|s| !s.is_empty());
    let (id, secret, refresh) = (
        field("clientId")?,
        field("clientSecret")?,
        field("refreshToken")?,
    );
    // The region becomes part of the host the secrets are posted to.
    let region = field("region").filter(|r| {
        r.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    })?;
    // AWS_ENDPOINT_URL_SSO_OIDC: the SDK-standard override, as `aws` honours it.
    let base = env::var("AWS_ENDPOINT_URL_SSO_OIDC")
        .unwrap_or_else(|_| format!("https://oidc.{region}.amazonaws.com"));
    if !safe_endpoint(&base) {
        return None;
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .build()
        .into();
    let body = json!({
        "clientId": id,
        "clientSecret": secret,
        "grantType": "refresh_token",
        "refreshToken": refresh,
    });
    let mut resp = agent
        .post(format!("{}/token", base.trim_end_matches('/')))
        .header("Content-Type", "application/json")
        .send(body.to_string())
        .ok()?;
    let r: Value = serde_json::from_str(&resp.body_mut().read_to_string().ok()?).ok()?;
    r["accessToken"].is_string().then_some(r)
}

/// The secrets go over TLS, or in clear only to this machine (tests, local
/// emulators). Parsed, not prefix-matched: `http://localhost:1@evil/` is evil.
fn safe_endpoint(base: &str) -> bool {
    let Ok(uri) = base.parse::<ureq::http::Uri>() else {
        return false;
    };
    match uri.scheme_str() {
        Some("https") => true,
        Some("http") => matches!(uri.host(), Some("127.0.0.1" | "localhost" | "[::1]")),
        _ => false,
    }
}

/// Atomic, owner-only replace: a crash mid-write must never leave botocore a
/// truncated token file.
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("json.{}", std::process::id()));
    let result = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| f.write_all(data).and_then(|()| f.sync_all()))
        .and_then(|()| fs::rename(&tmp, path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Is anything running against this session right now? Our own processes
/// with `--profile P` / `--profile=P` in argv or AWS_PROFILE=P in the
/// start-up env, for every profile P bound to it. Returns "pid argv…".
fn in_use(s: &Session) -> Option<String> {
    let profiles: HashSet<&str> = s.profiles.iter().map(|p| p.name.as_str()).collect();
    let uid = fs::metadata("/proc/self").ok()?.uid();
    let me = std::process::id().to_string();
    fs::read_dir("/proc").ok()?.flatten().find_map(|entry| {
        let pid = entry.file_name().into_string().ok()?;
        if pid == me || !pid.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let dir = entry.path();
        if fs::metadata(&dir).ok()?.uid() != uid {
            return None;
        }
        let argv: Vec<String> = fs::read(dir.join("cmdline"))
            .ok()?
            .split(|b| *b == 0)
            .filter(|a| !a.is_empty())
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        let env_profile = || {
            let environ = fs::read(dir.join("environ")).ok()?;
            environ
                .split(|b| *b == 0)
                .find_map(|v| v.strip_prefix(b"AWS_PROFILE="))
                .map(|v| String::from_utf8_lossy(v).into_owned())
        };
        let hit = argv_profile(&argv).any(|p| profiles.contains(p))
            || env_profile().is_some_and(|p| profiles.contains(p.as_str()));
        hit.then(|| {
            format!(
                "{pid} {}",
                argv.iter().take(6).cloned().collect::<Vec<_>>().join(" ")
            )
        })
    })
}

/// Profiles named by whole argv elements, never a substring: a shell whose
/// `-c` script merely mentions a profile must not count as using it.
fn argv_profile(argv: &[String]) -> impl Iterator<Item = &str> {
    argv.iter()
        .enumerate()
        .filter_map(|(i, a)| match a.strip_prefix("--profile") {
            Some("") => argv.get(i + 1).map(String::as_str),
            Some(rest) => rest.strip_prefix('='),
            None => None,
        })
}

/// Recent use: every real use of a profile mints role creds into the CLI
/// cache, tagged with the account id, at least hourly while use continues
/// (role creds live 1h). sso-guard itself never mints any. Catches pollers
/// whose own argv/env carry no profile (only their short-lived children do).
fn used_recently(s: &Session) -> Option<String> {
    let accounts: HashSet<&str> = s
        .profiles
        .iter()
        .filter_map(|p| p.account_id.as_deref())
        .collect();
    let dir = fs::read_dir(cswap::home().join(".aws/cli/cache")).ok()?;
    dir.flatten().find_map(|entry| {
        let path = entry.path();
        let minted = fs::metadata(&path).ok()?.modified().ok()?;
        if path.extension()? != "json" || minted.elapsed().ok()? >= Duration::from_secs(65 * 60) {
            return None;
        }
        let v: Value = serde_json::from_slice(&fs::read(&path).ok()?).ok()?;
        let account = v["Credentials"]["AccountId"].as_str()?;
        let minted = jiff::Timestamp::try_from(minted).ok()?;
        accounts
            .contains(account)
            .then(|| format!("acct {account} creds minted {}", clock(minted.as_second())))
    })
}

/// Local wall-clock HH:MM for the journal.
fn clock(epoch: i64) -> String {
    jiff::Timestamp::from_second(epoch)
        .map(|t| {
            t.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_sessions_and_their_profiles() {
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

[profile second]
sso_account_id = 5678
sso_session = sia

[profile other]
sso_session = elsewhere
";
        let found = sessions(config);
        // The orphan session (no profile references it) is skipped; account
        // ids are read whatever the key order.
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "sia");
        assert_eq!(found[0].start_url, "https://sia.awsapps.com/start");
        let profiles: Vec<_> = found[0]
            .profiles
            .iter()
            .map(|p| (p.name.as_str(), p.account_id.as_deref()))
            .collect();
        assert_eq!(
            profiles,
            [
                ("code-assist-admin", Some("1234")),
                ("second", Some("5678"))
            ]
        );
    }

    #[test]
    fn profile_args_match_whole_elements_only() {
        let argv = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        let found = |s: &str| argv_profile(&argv(s)).map(String::from).collect::<Vec<_>>();
        assert_eq!(found("aws ssm start-session --profile p1"), ["p1"]);
        assert_eq!(found("kubectl --profile=p2 get pods"), ["p2"]);
        assert!(found("bash -c echo--profile p3").is_empty());
        assert!(found("aws --profiles p4").is_empty());
        assert!(found("aws --profile").is_empty()); // dangling flag
    }

    #[test]
    fn secrets_only_go_over_tls_or_to_this_machine() {
        assert!(safe_endpoint("https://oidc.eu-west-1.amazonaws.com"));
        assert!(safe_endpoint("http://127.0.0.1:4566"));
        assert!(safe_endpoint("http://localhost:4566/"));
        assert!(!safe_endpoint("http://oidc.example.com"));
        assert!(!safe_endpoint("http://localhost:1@evil.example/"));
        assert!(!safe_endpoint("ftp://x"));
        assert!(!safe_endpoint("not a url"));
    }

    #[test]
    fn write_private_is_owner_only_and_leaves_no_temp() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("tok.json");
        fs::write(&f, "old").unwrap();
        write_private(&f, b"new").unwrap();
        assert_eq!(fs::read_to_string(&f).unwrap(), "new");
        assert_eq!(fs::metadata(&f).unwrap().mode() & 0o777, 0o600);
        assert_eq!(fs::read_dir(t.path()).unwrap().count(), 1);
    }
}
