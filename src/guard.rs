//! `ccflip sso-guard` — keep cloud CLI credentials fresh so Bedrock sessions
//! (and the litellm gateway, bastion tunnels, kubectl, gcloud…) never die
//! mid-flight with "Token is expired". `ccflip status` reports the same
//! accounts as JSON, for humans and for a future UI.
//!
//! This file is the provider-agnostic part: the [`Provider`] contract, the
//! on/off switch, scheduling, and asking a human. Adding a provider = one
//! module implementing [`Provider`] + one entry in [`PROVIDERS`].
//!
//! Scheduling is adaptive: each run arms a transient timer to wake ~1 min
//! before the earliest token crosses the threshold. The static hourly
//! `sso-guard.timer` is the reboot-safe backstop (and the poll for
//! providers whose expiry cannot be known in advance).

mod aws;
mod gcloud;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::Serialize;

use crate::policy::{self, Policy};
use crate::{cswap, now};

/// One family of credentials (AWS Identity Center, gcloud, …).
pub trait Provider: Sync {
    /// Short id: prefixes `ccflip status` rows, state markers and units.
    fn id(&self) -> &'static str;
    /// Human name for notifications ("AWS SSO").
    fn label(&self) -> &'static str;
    /// One keep-alive pass: renew what can be renewed silently; report
    /// wake-ups (`Run::track`/`want`), failures, and humans needed
    /// (`ask_login`).
    fn check(&self, run: &mut Run);
    /// Every account and its state. Never prompts or opens anything.
    fn status(&self, run: &Run) -> Vec<Account>;
    /// Interactive login for one account (the notification's click).
    fn login(&self, account: &str) -> bool;
}

pub static PROVIDERS: [&dyn Provider; 2] = [&aws::Aws, &gcloud::Gcloud];

/// One row of `ccflip status`.
#[derive(Serialize)]
pub struct Account {
    pub provider: &'static str,
    pub account: String,
    pub state: State,
    /// RFC 3339; absent when the provider cannot know in advance.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// A login only a human can finish is pending (notification posted).
    pub needs_you: bool,
}

#[derive(Serialize, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Ok,
    /// Under the renewal threshold; the guard renews it on its next run.
    Expiring,
    Expired,
    /// Could not tell (network, tool failure); retried on the next run.
    Error,
}

/// One run's shared state: the clock, the knobs, the next wake-up, the exit
/// code.
pub struct Run {
    pub now: i64,
    /// Act on a token with less life than this.
    pub min_seconds: i64,
    pub policy: Policy,
    /// Earliest epoch the next run should happen (0 = no data).
    wake: i64,
    /// An account still needs an interactive login: exit 1, retry in 10 min.
    pub failed: bool,
}

impl Run {
    fn new() -> Self {
        let policy = policy::read();
        Run {
            now: now(),
            min_seconds: policy.knob("SSO_GUARD_MIN_MINUTES", 15i64) * 60,
            policy,
            wake: 0,
            failed: false,
        }
    }

    pub fn want(&mut self, at: i64) {
        if at > 0 && (self.wake == 0 || at < self.wake) {
            self.wake = at;
        }
    }

    /// Healthy token: wake 1 min before it dips under the threshold.
    pub fn track(&mut self, expiry: i64) {
        self.want(expiry - self.min_seconds - 60);
    }

    /// State of a token expiring at `expiry` (epoch seconds).
    pub fn state(&self, expiry: i64) -> State {
        match expiry - self.now {
            left if left > self.min_seconds => State::Ok,
            left if left > 0 => State::Expiring,
            _ => State::Expired,
        }
    }
}

/// Exit code: 1 if any account still needs an interactive login.
pub fn run() -> i32 {
    if let Ok(key) = env::var("SSO_GUARD_ASK") {
        return ask_and_login(&key);
    }
    // sso-off disables the backstop timer; a run already in flight (or a
    // stale sso-guard-next) must then do nothing and not re-arm itself.
    if !systemctl_quiet(&["is-enabled", "sso-guard.timer"]) {
        println!("sso-guard: disabled (sso-off), skipping");
        return 0;
    }
    let _ = fs::create_dir_all(state_dir());
    let mut run = Run::new();
    for p in PROVIDERS {
        p.check(&mut run);
    }
    rearm(&run);
    i32::from(run.failed)
}

/// `ccflip status`: every provider's accounts as a JSON array.
pub fn status() {
    let run = Run::new();
    let rows: Vec<Account> = PROVIDERS.iter().flat_map(|p| p.status(&run)).collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&rows).unwrap_or_default()
    );
}

/// A login that needs a human: one persistent notification, login on
/// click. Runs detached (its own transient unit, re-entering this binary
/// via `SSO_GUARD_ASK`) so waiting for the click never blocks the other
/// accounts. Dismissed = re-ask at most every 2 h.
pub fn ask_login(p: &dyn Provider, account: &str) {
    let key = key(p, account);
    // Unit names allow [A-Za-z0-9:_.-]; emails bring `@`.
    let unit: String = format!("sso-guard-ask-{key}")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || ":_.-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    if systemctl_quiet(&["is-active", &unit])
        || modified_within(&dismissed(&key), Duration::from_secs(120 * 60))
        || !on_path("notify-send")
    {
        return;
    }
    let Ok(exe) = env::current_exe() else { return };
    let posted = Command::new("systemd-run")
        .args(["--user", "--quiet", "--collect", "--unit", &unit])
        .arg(format!(
            "--setenv=PATH={}",
            env::var("PATH").unwrap_or_default()
        ))
        .arg(format!("--setenv=SSO_GUARD_ASK={key}"))
        .arg(exe)
        .arg("sso-guard")
        .status()
        .is_ok_and(|s| s.success());
    if posted {
        println!("sso-guard: {account} needs you, notification posted");
    }
}

/// The detached half of [`ask_login`]: wait for the click, then log in
/// and re-run the guard so the next check is scheduled from the new token.
fn ask_and_login(key: &str) -> i32 {
    let Some((p, account)) = key
        .split_once('.')
        .and_then(|(id, account)| Some((PROVIDERS.iter().find(|p| p.id() == id)?, account)))
    else {
        eprintln!("sso-guard: unknown SSO_GUARD_ASK={key}");
        return 1;
    };
    let clicked = Command::new("notify-send")
        .args(["-u", "critical", "-t", "0", "-a", "sso-guard"])
        .args(["-A", "default=Log in"])
        .arg(format!("{}: {account} expired", p.label()))
        .arg("Click to log in (browser)")
        .output()
        .is_ok_and(|o| o.stdout.trim_ascii() == b"default");
    if !clicked {
        let _ = fs::write(dismissed(key), "");
        return 0;
    }
    if p.login(account) {
        clear_needs_you(*p, account);
    }
    systemctl_quiet(&["start", "--no-block", "sso-guard.service"]);
    0
}

/// Adaptive re-arm. Failure retries in 10 min; no data falls back to the
/// hourly backstop timer. Clamped to [1 min, 6 h].
fn rearm(run: &Run) {
    let delay = if run.failed {
        600
    } else if run.wake > 0 {
        (run.wake - now()).clamp(60, 6 * 3600)
    } else {
        return;
    };
    systemctl_quiet(&["stop", "sso-guard-next.timer"]);
    let armed = Command::new("systemd-run")
        .args(["--user", "--quiet", "--collect", "--unit", "sso-guard-next"])
        .arg("--timer-property=AccuracySec=1s")
        .arg(format!("--on-active={delay}s"))
        .args([
            "systemctl",
            "--user",
            "start",
            "--no-block",
            "sso-guard.service",
        ])
        .status()
        .is_ok_and(|s| s.success());
    if armed {
        println!("sso-guard: next check in {} min", delay / 60);
    }
}

/// `aws.heka`, `gcloud.me@example.com`: names markers and the ask unit.
/// A `/` (from an odd session or account name) must never leave the state dir.
fn key(p: &dyn Provider, account: &str) -> String {
    format!("{}.{}", p.id(), account.replace('/', "_"))
}

fn state_dir() -> PathBuf {
    env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map_or_else(|| cswap::home().join(".local/state"), PathBuf::from)
        .join("sso-guard")
}

/// Marker: a login only a human can finish is pending.
pub fn needs_you(p: &dyn Provider, account: &str) -> PathBuf {
    state_dir().join(format!("{}.needs-you", key(p, account)))
}

fn dismissed(key: &str) -> PathBuf {
    state_dir().join(format!("{key}.needs-you.dismissed"))
}

pub fn clear_needs_you(p: &dyn Provider, account: &str) {
    let _ = fs::remove_file(needs_you(p, account));
    let _ = fs::remove_file(dismissed(&key(p, account)));
}

fn systemctl_quiet(args: &[&str]) -> bool {
    Command::new("systemctl")
        .args(["--user", "--quiet"])
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

pub fn on_path(program: &str) -> bool {
    env::var_os("PATH").is_some_and(|p| env::split_paths(&p).any(|d| d.join(program).is_file()))
}

fn modified_within(path: &Path, age: Duration) -> bool {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .is_ok_and(|t| t.elapsed().is_ok_and(|e| e < age))
}
