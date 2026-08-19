//! `ccflip watch` — everything that has to happen while sessions are live:
//! pinned-profile credential rotation, mid-session tier switching, the
//! account containment guard, and spend alerts. One systemd unit, one
//! `cswap list` snapshot per tick.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime};

use crate::{budget, cswap, launch, policy, proc, select};

pub fn run() -> ! {
    let interval = crate::env_parsed("CCFLIP_WATCH_INTERVAL", 60u64);
    // A single tick then exit: for tests and for `systemd-run` one-shots.
    let once = std::env::var_os("CCFLIP_WATCH_ONCE").is_some_and(|v| !v.is_empty());

    let mut alerted: HashSet<String> = HashSet::new();
    let mut ticks: u64 = 0;
    if !once {
        // ponytail: no timestamps in log lines — journald stamps them.
        println!("ccflip-watch started (interval={interval}s)");
    }
    loop {
        tick(ticks, &mut alerted);
        ticks += 1;
        if once {
            std::process::exit(0);
        }
        thread::sleep(Duration::from_secs(interval));
    }
}

fn tick(ticks: u64, alerted: &mut HashSet<String>) {
    let policy = policy::read();
    // Later than cswap auto's default 90: the watcher is the last-resort lever.
    let threshold = policy.knob("CCFLIP_WATCH_THRESHOLD", 95.0);
    let spend_threshold = policy.knob("CCFLIP_SPEND_THRESHOLD", 97.0);
    let alert_pct = policy.knob("CCFLIP_SPEND_ALERT_PCT", 90.0);

    if let Some(list) = cswap::list() {
        if let Some((pinned, default)) = policy.containment() {
            enforce_policy(&list, &pinned, default.as_deref());
        }
        for account in &list.accounts {
            if let Some(pct) = account.spend_pct() {
                alert(&account.email, pct, alert_pct, alerted);
            }
        }
        rotate(&list, threshold, spend_threshold);
        switch_tiers(&list, &policy, threshold, spend_threshold);
    }
    // cswap only surfaces extra_usage; the seat's included monthly pool is a
    // separate limit_dollars bucket on the oauth usage API — poll it directly
    // (every 5th tick, spare the endpoint) so "close to 100%" means the meter
    // that actually drains first.
    if ticks.is_multiple_of(5) {
        match budget::fetch_usage() {
            Ok(usage) => {
                for (name, pct) in usage.meters() {
                    alert(&name, pct, alert_pct, alerted);
                }
            }
            Err(e) => println!("spend meters unavailable: {e:#}"),
        }
    }
}

/// Credential rotation for pinned session profiles. `cswap auto` deliberately
/// skips accounts owned by live `cswap run` sessions; this covers the gap by
/// writing the best remaining account's credentials into the profile, which
/// Claude Code picks up on its next token refresh — no restart.
///
/// Rotating a profile with no live process is harmless: the next `cswap run`
/// re-seeds it. So no PID tracking here.
fn rotate(list: &cswap::List, threshold: f64, spend_threshold: f64) {
    for dir in cswap::session_dirs() {
        let name = dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        // The account this profile is on RIGHT NOW. Reading it from the
        // directory name would mean re-switching the same profile every tick
        // forever: the name keeps the account it was seeded with.
        let Some(current) = cswap::profile_email(&dir)
            .and_then(|email| list.by_email(&email).map(|a| a.number))
            .or_else(|| name.split('-').next()?.parse().ok())
        else {
            continue;
        };
        let Some(target) = select::pick_target(list, current, threshold, spend_threshold) else {
            continue;
        };
        let switched = Command::new("cswap")
            .args(["switch", &target.to_string()])
            .env("CLAUDE_CONFIG_DIR", &dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if switched {
            println!("session '{name}' exhausted -> switched profile to account {target}");
        } else {
            println!("session '{name}': switch to account {target} failed (will retry)");
        }
    }
}

/// Mid-session tier switching by process replacement.
///
/// Credential rotation covers subscription->subscription, but Bedrock needs
/// env vars a live process cannot receive. The conversation is the session id,
/// not the process: SDK harnesses (t3code, claude-mem) respawn a killed
/// process on the next message with the same session. So a harness-spawned
/// process whose account is exhausted with no rotation target is killed (its
/// requests fail anyway) and revives on Bedrock; a Bedrock process whose
/// subscription tier is usable again is killed once its transcript has been
/// idle, and revives on the account. Terminal-spawned sessions are never
/// touched.
/// What to do with one live process. Everything that decides whether a user's
/// process gets signalled lives in this one pure function, so it can be
/// tested without a process to kill.
#[derive(Debug, PartialEq)]
enum Action {
    Leave,
    /// Exhausted subscription, nothing else can serve it: end it and let the
    /// harness revive the session on Bedrock.
    Failover,
    /// On Bedrock while a subscription tier is usable again, and idle.
    Switchback,
}

struct Facts {
    /// Harness-spawned, old enough, and not opted out.
    eligible: bool,
    on_bedrock: bool,
    /// A Bedrock tier actually exists on this host.
    bedrock_available: bool,
    /// A fresh launch in this cwd would land on a subscription account
    /// (fail-CLOSED: unknown health does not count).
    subscription_ready: bool,
    /// No transcript write within the idle window — verified, not assumed.
    transcript_idle: bool,
    /// `Some(false)` = known exhausted. `None` = health unknown: never act.
    account_healthy: Option<bool>,
    /// Credential rotation or the launcher will handle this one.
    covered: bool,
}

fn decide(f: &Facts) -> Action {
    if !f.eligible {
        return Action::Leave;
    }
    if f.on_bedrock {
        if f.subscription_ready && f.transcript_idle {
            return Action::Switchback;
        }
        return Action::Leave;
    }
    // Killing a session whose only escape hatch is missing just loops: the
    // harness respawns, the launcher lands on the same exhausted account.
    if f.account_healthy == Some(false) && !f.covered && f.bedrock_available {
        return Action::Failover;
    }
    Action::Leave
}

/// Mid-session tier switching by process replacement (see `decide`).
fn switch_tiers(list: &cswap::List, policy: &policy::Policy, threshold: f64, spend_threshold: f64) {
    // This is the one thing the watcher does that ends a live process, so it
    // has an off switch (and tests use it to keep their hands off real
    // sessions). Rotation, policy and alerts keep running without it.
    if std::env::var_os("CCFLIP_NO_TIER_SWITCH").is_some_and(|v| !v.is_empty()) {
        return;
    }
    let switchback_idle = Duration::from_secs(policy.knob("CCFLIP_SWITCHBACK_IDLE", 300u64));
    let min_age = Duration::from_secs(policy.knob("CCFLIP_PROC_MIN_AGE", 120u64));
    // Tick-constant: computed once, not once per process.
    let bedrock_available = launch::bedrock_env_path().exists();
    let any_electable = select::any_enabled_electable(list, threshold, spend_threshold);

    for p in proc::claude_procs() {
        // Only harness-spawned processes auto-respawn; never kill terminals.
        // ponytail: the parent-exe heuristic cannot see whether a harness
        // really respawns, so batch callers opt out with CCFLIP_NO_TIER_SWITCH
        // in their environment; the age gate covers short-lived `claude -p`.
        let eligible = p.harness_spawned()
            && !p.opted_out
            && p.age >= min_age
            && !p.cwd.contains("claude-mem");
        let email = match &p.config_dir {
            Some(dir) => cswap::profile_email(Path::new(dir)),
            None => list.active_account().map(|a| a.email.clone()),
        };
        let account = email.as_deref().and_then(|e| list.by_email(e));
        let facts = Facts {
            eligible,
            on_bedrock: p.bedrock,
            bedrock_available,
            // Only computed for Bedrock processes: it reads mappings.json.
            subscription_ready: p.bedrock
                && eligible
                && subscription_ready(list, &p.cwd, any_electable, threshold, spend_threshold),
            transcript_idle: p.bedrock
                && eligible
                && transcript_idle(p.config_dir.as_deref(), &p.cwd, switchback_idle),
            account_healthy: account.map(|a| select::usable(a, threshold, spend_threshold)),
            covered: match (&p.config_dir, account) {
                // Rotation will move this profile's credentials.
                (Some(_), Some(a)) => {
                    select::pick_target(list, a.number, threshold, spend_threshold).is_some()
                }
                // Unknown profile: leave it alone rather than guess.
                (Some(_), None) => true,
                // Default login: cswap auto / the next launch handle it.
                (None, _) => any_electable,
            },
        };
        match decide(&facts) {
            Action::Leave => {}
            Action::Switchback if kill(p.pid) => println!(
                "switchback: ended bedrock process {} ({}) — subscription usable again, session revives on it",
                p.pid, p.cwd
            ),
            Action::Failover if kill(p.pid) => println!(
                "failover: ended process {} on exhausted {} ({}) — session revives on Bedrock",
                p.pid,
                email.as_deref().unwrap_or("?"),
                p.cwd
            ),
            _ => {}
        }
    }
}

fn kill(pid: i32) -> bool {
    // ponytail: `kill(1)` instead of a libc/nix dep for one signal.
    Command::new("kill")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Would a fresh launch in this cwd land on a subscription account? This
/// ELECTS a kill, so it fails CLOSED: unknown health must not tear down a
/// working Bedrock session (and then flap it back when real data returns).
fn subscription_ready(
    list: &cswap::List,
    cwd: &str,
    any_electable: bool,
    threshold: f64,
    spend_threshold: f64,
) -> bool {
    if let Some(mapped) = cswap::mapped_email(cwd) {
        return list
            .by_email(&mapped)
            .is_some_and(|a| select::electable(a, threshold, spend_threshold))
            || any_electable;
    }
    any_electable
}

/// No transcript write for this project in `idle` — safe to end the process
/// without cutting off a live exchange. An unverifiable state (no such
/// project dir, unreadable, a slug we computed wrong) is NOT idle: this guard
/// is the only thing standing between the switchback and live work.
fn transcript_idle(config_dir: Option<&str>, cwd: &str, idle: Duration) -> bool {
    let base = config_dir
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| cswap::home().join(".claude"));
    let dir = base.join("projects").join(cswap::project_slug(cwd));
    let Ok(entries) = fs::read_dir(&dir) else {
        return false; // cannot verify => do not touch it
    };
    let cutoff = SystemTime::now() - idle;
    !entries.flatten().any(|e| {
        e.path().extension().is_some_and(|x| x == "jsonl")
            && e.metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|m| m > cutoff)
    })
}

/// Once per breach — journal always, desktop notification when available.
fn alert(name: &str, pct: f64, alert_line: f64, alerted: &mut HashSet<String>) {
    if breach(name, pct, alert_line, alerted) {
        println!("SPEND ALERT: {name} at {pct}% of its budget (alert line {alert_line}%)");
        let _ = Command::new("notify-send")
            .args([
                "-u",
                "critical",
                "ccflip",
                &format!("{name} at {pct}% of spend budget"),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Pure breach tracker: true exactly when a NEW breach begins; re-arms once
/// the meter drops back under the line.
fn breach(name: &str, pct: f64, alert_line: f64, alerted: &mut HashSet<String>) -> bool {
    if pct.round() >= alert_line {
        alerted.insert(name.to_string())
    } else {
        alerted.remove(name);
        false
    }
}

/// Account containment guard. `cswap import` re-activates a replaced slot
/// and clears its disabled flag, and nothing else re-checks intent — so a
/// token refresh can silently make a pinned-only (personal) account the
/// default login. Enforce the declared policy every tick: a pinned-only
/// account found active is switched away from, and one found enabled is
/// re-disabled — but NEVER disable the account that is still the live
/// default login (active-and-disabled is a state nothing rotates out of).
fn enforce_policy(list: &cswap::List, pinned: &[String], default: Option<&str>) {
    let pinned: Vec<&str> = pinned.iter().map(String::as_str).collect();
    let (offender, mut redisable) = policy_violations(list, &pinned);
    if let Some(offender) = offender {
        let switched = match default {
            Some(target) => {
                if cswap_cmd(&["switch", target]) {
                    println!(
                        "policy: pinned-only {offender} was the default login -> switched back to {target}"
                    );
                    true
                } else {
                    println!(
                        "policy: pinned-only {offender} is the default login and switch to {target} FAILED"
                    );
                    false
                }
            }
            None => {
                println!(
                    "policy: pinned-only {offender} is the default login and no CCFLIP_DEFAULT_ACCOUNT is set — leaving it enabled"
                );
                false
            }
        };
        if !switched {
            redisable.retain(|e| *e != offender);
        }
    }
    for email in redisable {
        if cswap_cmd(&["disable", email]) {
            println!("policy: re-disabled pinned-only account {email}");
        } else {
            println!("policy: disable of {email} failed (will retry)");
        }
    }
}

/// Pure policy decision: which pinned-only account is the default login
/// (at most one can be active), and which are enabled and must be re-disabled.
fn policy_violations<'a>(
    list: &cswap::List,
    pinned: &[&'a str],
) -> (Option<&'a str>, Vec<&'a str>) {
    let active = list.active_account().map(|a| a.email.as_str());
    let offender = pinned.iter().copied().find(|p| active == Some(p));
    let redisable = pinned
        .iter()
        .copied()
        .filter(|p| list.accounts.iter().any(|a| a.email == *p && !a.disabled))
        .collect();
    (offender, redisable)
}

fn cswap_cmd(args: &[&str]) -> bool {
    Command::new("cswap")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::policy_violations;

    // Same fixtures as the old tests/policy_guard_test.sh.
    const PINNED: &[&str] = &["qm@x.com"];

    fn list(json: &str) -> crate::cswap::List {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn violation_switches_back_and_redisables() {
        let l = list(
            r#"{"accounts":[
 {"number":1,"email":"sia@x.com","active":false,"usageStatus":"ok"},
 {"number":2,"email":"qm@x.com","active":true,"usageStatus":"ok"}]}"#,
        );
        assert_eq!(
            policy_violations(&l, PINNED),
            (Some("qm@x.com"), vec!["qm@x.com"])
        );
    }

    #[test]
    fn compliant_state_is_untouched() {
        let l = list(
            r#"{"accounts":[
 {"number":1,"email":"sia@x.com","active":true,"usageStatus":"ok"},
 {"number":2,"email":"qm@x.com","active":false,"usageStatus":"ok","disabled":true}]}"#,
        );
        assert_eq!(policy_violations(&l, PINNED), (None, vec![]));
    }

    #[test]
    fn active_account_number_beats_row_flag() {
        // Same document shape the launcher sees: top-level number wins.
        let l = list(
            r#"{"activeAccountNumber":2,"accounts":[
 {"number":1,"email":"sia@x.com","active":true,"usageStatus":"ok"},
 {"number":2,"email":"qm@x.com","active":false,"usageStatus":"ok"}]}"#,
        );
        assert_eq!(
            policy_violations(&l, PINNED),
            (Some("qm@x.com"), vec!["qm@x.com"])
        );
    }

    // Every path that can end a user's process. Base case: an exhausted
    // subscription session with nothing to rotate to and Bedrock configured.
    fn exhausted() -> super::Facts {
        super::Facts {
            eligible: true,
            on_bedrock: false,
            bedrock_available: true,
            subscription_ready: false,
            transcript_idle: false,
            account_healthy: Some(false),
            covered: false,
        }
    }

    fn on_bedrock() -> super::Facts {
        super::Facts {
            on_bedrock: true,
            subscription_ready: true,
            transcript_idle: true,
            account_healthy: Some(true),
            ..exhausted()
        }
    }

    #[test]
    fn failover_only_when_there_is_somewhere_to_land() {
        use super::{Action, decide};
        assert_eq!(decide(&exhausted()), Action::Failover);
        // No bedrock.env: killing would just respawn onto the same dry
        // account, every PROC_MIN_AGE seconds, forever.
        assert_eq!(
            decide(&super::Facts {
                bedrock_available: false,
                ..exhausted()
            }),
            Action::Leave
        );
        // Rotation or the launcher has it.
        assert_eq!(
            decide(&super::Facts {
                covered: true,
                ..exhausted()
            }),
            Action::Leave
        );
        // Health unknown (cswap poll gap) is never grounds to act.
        assert_eq!(
            decide(&super::Facts {
                account_healthy: None,
                ..exhausted()
            }),
            Action::Leave
        );
        // Terminal session, batch caller, or too young.
        assert_eq!(
            decide(&super::Facts {
                eligible: false,
                ..exhausted()
            }),
            Action::Leave
        );
    }

    #[test]
    fn switchback_needs_a_ready_tier_and_a_quiet_transcript() {
        use super::{Action, decide};
        assert_eq!(decide(&on_bedrock()), Action::Switchback);
        // Mid-conversation: never cut off live work.
        assert_eq!(
            decide(&super::Facts {
                transcript_idle: false,
                ..on_bedrock()
            }),
            Action::Leave
        );
        // No subscription account is electable yet.
        assert_eq!(
            decide(&super::Facts {
                subscription_ready: false,
                ..on_bedrock()
            }),
            Action::Leave
        );
        // The user chose this tier (`ccflip bedrock`) — eligible is false.
        assert_eq!(
            decide(&super::Facts {
                eligible: false,
                ..on_bedrock()
            }),
            Action::Leave
        );
    }

    #[test]
    fn transcript_idle_never_guesses() {
        use std::time::Duration;

        let t = tempfile::tempdir().unwrap();
        let cfg = t.path().display().to_string();
        let cwd = "/home/u/my_project/.venv";
        // Unverifiable (no project dir) must NOT read as idle.
        assert!(!super::transcript_idle(
            Some(&cfg),
            cwd,
            Duration::from_secs(300)
        ));

        // Claude Code's slug replaces EVERY non-alphanumeric character; the
        // old `/`-and-`.`-only version looked at "-home-u-my_project--venv"
        // and found nothing, then called an active session idle.
        let dir = t.path().join("projects/-home-u-my-project--venv");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("session.jsonl"), "{}").unwrap();
        assert!(!super::transcript_idle(
            Some(&cfg),
            cwd,
            Duration::from_secs(300)
        ));
        // Same transcript, zero-length idle window: nothing is newer than now.
        assert!(super::transcript_idle(
            Some(&cfg),
            cwd,
            Duration::from_secs(0)
        ));
    }

    #[test]
    fn alert_fires_once_per_breach_and_rearms() {
        let mut alerted = HashSet::new();
        assert!(super::breach("m", 95.0, 90.0, &mut alerted)); // fires
        assert!(!super::breach("m", 96.0, 90.0, &mut alerted)); // no re-fire
        assert!(!super::breach("m", 50.0, 90.0, &mut alerted)); // re-armed
        assert!(super::breach("m", 91.0, 90.0, &mut alerted)); // fires again
    }
}
