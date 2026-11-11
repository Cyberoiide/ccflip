//! `ccflip sso-guard` — keep cloud CLI sessions fresh so Bedrock sessions
//! (and the litellm gateway, bastion tunnels, kubectl…) never die mid-flight
//! with "Token is expired".
//!
//! This file is the provider-agnostic part: on/off switch, scheduling, and
//! asking a human. Each provider module checks its own sessions and reports
//! into a [`Run`]. Today: `aws` (IAM Identity Center). gcloud is planned as a
//! sibling module called from [`run`] the same way.
//!
//! Scheduling is adaptive: each run arms a transient timer to wake ~1 min
//! before the earliest token crosses the threshold. The static hourly
//! `sso-guard.timer` is only the reboot-safe backstop.

mod aws;

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::{cswap, now};

/// One run's shared state: the clock, the next wake-up, the exit code.
pub struct Run {
    pub now: i64,
    /// Act on a token with less life than this.
    pub min_seconds: i64,
    /// Earliest epoch the next run should happen (0 = no data).
    wake: i64,
    /// A session still needs an interactive login: exit 1, retry in 10 min.
    pub failed: bool,
}

impl Run {
    pub fn want(&mut self, at: i64) {
        if at > 0 && (self.wake == 0 || at < self.wake) {
            self.wake = at;
        }
    }

    /// Healthy token: wake 1 min before it dips under the threshold.
    pub fn track(&mut self, expiry: i64) {
        self.want(expiry - self.min_seconds - 60);
    }
}

/// A login that needs a human: one persistent notification, login on
/// click. Runs detached (its own transient unit, re-entering this binary
/// via `SSO_GUARD_ASK`) so waiting for the click never blocks the other
/// sessions. Dismissed = re-ask at most every 2 h.
pub fn ask_login(name: &str) {
    let unit = format!("sso-guard-ask-{name}");
    if systemctl_quiet(&["is-active", &unit])
        || modified_within(&dismissed(name), Duration::from_secs(120 * 60))
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
        .arg(format!("--setenv=SSO_GUARD_ASK={name}"))
        .arg(exe)
        .arg("sso-guard")
        .status()
        .is_ok_and(|s| s.success());
    if posted {
        println!("sso-guard: {name} needs you, notification posted");
    }
}

/// Exit code: 1 if any session still needs an interactive login.
pub fn run() -> i32 {
    if let Ok(name) = env::var("SSO_GUARD_ASK") {
        return ask_and_login(&name);
    }
    // sso-off disables the backstop timer; a run already in flight (or a
    // stale sso-guard-next) must then do nothing and not re-arm itself.
    if !systemctl_quiet(&["is-enabled", "sso-guard.timer"]) {
        println!("sso-guard: disabled (sso-off), skipping");
        return 0;
    }
    let _ = fs::create_dir_all(state_dir());
    let mut run = Run {
        now: now(),
        min_seconds: crate::env_parsed("SSO_GUARD_MIN_MINUTES", 15i64) * 60,
        wake: 0,
        failed: false,
    };
    aws::check(&mut run);
    rearm(&run);
    i32::from(run.failed)
}

/// The detached half of [`ask_login`]: wait for the click, then log in
/// and re-run the guard so the next check is scheduled from the new token.
fn ask_and_login(name: &str) -> i32 {
    let clicked = Command::new("notify-send")
        .args(["-u", "critical", "-t", "0", "-a", "sso-guard"])
        .args(["-A", "default=Log in"])
        .arg(format!("AWS SSO: {name} expired"))
        .arg("Click to log in (browser)")
        .output()
        .is_ok_and(|o| o.stdout.trim_ascii() == b"default");
    if !clicked {
        let _ = fs::write(dismissed(name), "");
        return 0;
    }
    if aws::login(name) {
        clear_needs_you(name);
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

fn state_dir() -> PathBuf {
    env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map_or_else(|| cswap::home().join(".local/state"), PathBuf::from)
        .join("sso-guard")
}

/// Marker: the last click-free login timed out, a human is needed.
pub fn needs_you(name: &str) -> PathBuf {
    state_dir().join(format!("{name}.needs-you"))
}

fn dismissed(name: &str) -> PathBuf {
    state_dir().join(format!("{name}.needs-you.dismissed"))
}

pub fn clear_needs_you(name: &str) {
    let _ = fs::remove_file(needs_you(name));
    let _ = fs::remove_file(dismissed(name));
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

fn on_path(program: &str) -> bool {
    env::var_os("PATH").is_some_and(|p| env::split_paths(&p).any(|d| d.join(program).is_file()))
}

fn modified_within(path: &std::path::Path, age: Duration) -> bool {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .is_ok_and(|t| t.elapsed().is_ok_and(|e| e < age))
}
