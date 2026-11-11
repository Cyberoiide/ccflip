//! End-to-end check of `ccflip sso-guard` with fakes — no AWS account, no
//! systemd, no desktop touched. A fake OIDC server answers the refresh call;
//! fake `aws`, `systemctl`, `systemd-run` and `notify-send` record what
//! reached them.
//!
//! Parity: with `SSO_GUARD_ORACLE=<path to the old bin/sso-guard>` every
//! scenario also runs the bash version and asserts identical decisions.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};

const URL: &str = "https://a.example/start";
/// Tests run in parallel and the in-use scan sees every process of this
/// user, so tests that start a profile-holding process each use their own
/// profile name.
fn config(profile: &str) -> String {
    format!(
        "\
[sso-session sg-a]
sso_start_url = {URL}
sso_region = eu-west-1

[profile {profile}]
sso_session = sg-a
sso_account_id = 111111111111
"
    )
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn stamp(epoch: i64) -> String {
    let out = Command::new("date")
        .args(["-u", "-d", &format!("@{epoch}"), "+%FT%TZ"])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn epoch(stamp: &str) -> i64 {
    let out = Command::new("date")
        .args(["-d", stamp, "+%s"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}

fn write_exec(path: &Path, content: &str) {
    fs::write(path, content).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// What the OIDC endpoint answers: (HTTP status, JSON body).
#[derive(Clone)]
struct Oidc(u16, String);

fn ok_token(expires_in: i64) -> Oidc {
    Oidc(
        200,
        format!(
            r#"{{"accessToken":"fresh","expiresIn":{expires_in},"refreshToken":"rt2","tokenType":"Bearer"}}"#
        ),
    )
}

fn denied() -> Oidc {
    Oidc(400, r#"{"error":"invalid_grant"}"#.to_string())
}

/// One-connection-at-a-time HTTP server; records every request body.
fn serve(answer: Oidc) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut len = 0;
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 && line != "\r\n" {
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
                line.clear();
            }
            let mut body = vec![0; len];
            reader.read_exact(&mut body).unwrap();
            log.lock().unwrap().push(String::from_utf8(body).unwrap());
            let reply = format!(
                "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                answer.0,
                answer.1.len(),
                answer.1
            );
            let _ = (&stream).write_all(reply.as_bytes());
        }
    });
    (addr, seen)
}

struct Scenario {
    /// Seconds of life left on the cached access token.
    left: i64,
    oidc: Oidc,
    login_ok: bool,
    graphical: bool,
    browser: Option<&'static str>,
    timer_enabled: bool,
    /// Pre-existing state markers: needs-you, needs-you.dismissed.
    markers: &'static [&'static str],
    /// A ~/.aws/cli/cache entry minted now for this account id.
    minted_for: Option<&'static str>,
    tz: &'static str,
    profile: &'static str,
}

impl Default for Scenario {
    fn default() -> Self {
        Scenario {
            left: 7200,
            oidc: denied(),
            login_ok: true,
            graphical: true,
            browser: None,
            timer_enabled: true,
            markers: &[],
            minted_for: None,
            tz: "Europe/Paris",
            profile: "sg-a-prof",
        }
    }
}

/// Every decision a run leaves behind, normalised so bash and Rust compare.
#[derive(Debug, PartialEq)]
struct Outcome {
    rc: i32,
    lines: Vec<String>,
    refresh_calls: usize,
    logins: usize,
    asks: usize,
    /// `--on-active` delay in seconds, if a re-arm was scheduled.
    rearm: Option<i64>,
    access_token: String,
    expires_at: i64,
    markers: Vec<String>,
}

fn approx(a: i64, b: i64) -> bool {
    (a - b).abs() <= 3
}

fn same(rust: &Outcome, bash: &Outcome) {
    let rearm_ok = match (rust.rearm, bash.rearm) {
        (Some(a), Some(b)) => approx(a, b),
        (a, b) => a == b,
    };
    let strip = |o: &Outcome| {
        o.lines
            .iter()
            .filter(|l| !l.contains("next check in"))
            .cloned()
            .collect::<Vec<_>>()
    };
    assert!(
        rearm_ok
            && approx(rust.expires_at, bash.expires_at)
            && strip(rust) == strip(bash)
            && (rust.rc, rust.refresh_calls, rust.logins, rust.asks)
                == (bash.rc, bash.refresh_calls, bash.logins, bash.asks)
            && (&rust.access_token, &rust.markers) == (&bash.access_token, &bash.markers),
        "rust and bash disagree:\nrust: {rust:#?}\nbash: {bash:#?}"
    );
}

/// Run `program` (the Rust binary, or the bash oracle) on a fresh fixture.
fn run_one(s: &Scenario, program: &[&str], extra_env: &[(&str, &str)]) -> Outcome {
    let t = tempfile::tempdir().unwrap();
    let t = t.path();
    let bin = t.join("bin");
    let cache = t.join(".aws/sso/cache");
    let state = t.join("state/sso-guard");
    for d in [&bin, &cache, &state, &t.join(".aws/cli/cache")] {
        fs::create_dir_all(d).unwrap();
    }
    fs::write(t.join(".aws/config"), config(s.profile)).unwrap();
    let token = cache.join("tok.json");
    fs::write(
        &token,
        format!(
            r#"{{"startUrl":"{URL}","region":"eu-west-1","accessToken":"old","expiresAt":"{}","clientId":"cid","clientSecret":"csec","refreshToken":"rt","registrationExpiresAt":"2027-02-07T00:00:00Z"}}"#,
            stamp(now() + s.left)
        ),
    )
    .unwrap();
    // The bash version named markers by session; ccflip by provider.session.
    let prefix = if program[0] == "bash" { "" } else { "aws." };
    for m in s.markers {
        fs::write(state.join(format!("{prefix}sg-a.{m}")), "").unwrap();
    }
    if let Some(acct) = s.minted_for {
        fs::write(
            t.join(".aws/cli/cache/role.json"),
            format!(r#"{{"Credentials":{{"AccountId":"{acct}"}}}}"#),
        )
        .unwrap();
    }

    let calls = t.join("calls");
    write_exec(
        &bin.join("systemctl"),
        r#"#!/usr/bin/env bash
echo "systemctl $*" >> "$CALLS"
case " $* " in
  *" is-enabled "*) exit "$TIMER_RC" ;;
  *" is-active "*) exit 1 ;;
esac
"#,
    );
    write_exec(
        &bin.join("systemd-run"),
        "#!/usr/bin/env bash\necho \"systemd-run $*\" >> \"$CALLS\"\n",
    );
    // Shadows a real gcloud (CI runners ship one): AWS scenarios only.
    write_exec(&bin.join("gcloud"), "#!/bin/sh\necho []\n");
    write_exec(
        &bin.join("notify-send"),
        "#!/usr/bin/env bash\necho \"notify-send $*\" >> \"$CALLS\"\n",
    );
    // The bash oracle refreshes through `aws sso-oidc create-token`; this
    // fake answers exactly what the fake server answers the Rust binary.
    write_exec(
        &bin.join("aws"),
        r#"#!/usr/bin/env bash
echo "aws $*" >> "$CALLS"
case "$1 $2" in
  "sso-oidc create-token") [[ "$OIDC_STATUS" == 200 ]] && printf '%s' "$OIDC_BODY" || exit 254 ;;
  "sso login")
    [[ "$LOGIN_RC" == 0 ]] || exit 1
    jq --arg e "$(date -u -d +28800sec +%FT%TZ)" '.accessToken = "login" | .expiresAt = $e' \
       "$HOME/.aws/sso/cache/tok.json" > "$HOME/t.json" && mv "$HOME/t.json" "$HOME/.aws/sso/cache/tok.json" ;;
esac
"#,
    );

    let (endpoint, seen) = serve(s.oidc.clone());
    let mut cmd = Command::new(program[0]);
    cmd.args(&program[1..])
        .env_clear()
        .env("HOME", t)
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("TZ", s.tz)
        .env("XDG_STATE_HOME", t.join("state"))
        .env("CALLS", &calls)
        .env("TIMER_RC", if s.timer_enabled { "0" } else { "1" })
        .env("LOGIN_RC", if s.login_ok { "0" } else { "1" })
        .env("OIDC_STATUS", s.oidc.0.to_string())
        .env("OIDC_BODY", &s.oidc.1)
        .env("AWS_ENDPOINT_URL_SSO_OIDC", &endpoint);
    if s.graphical {
        cmd.env("WAYLAND_DISPLAY", "wayland-1");
    }
    if let Some(b) = s.browser {
        cmd.env("SSO_GUARD_BROWSER", b);
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();

    let calls = fs::read_to_string(&calls).unwrap_or_default();
    let cached: serde_json::Value = serde_json::from_slice(&fs::read(&token).unwrap()).unwrap();
    let mut markers: Vec<String> = fs::read_dir(&state)
        .unwrap()
        .map(|e| {
            let name = e.unwrap().file_name().into_string().unwrap();
            name.strip_prefix(prefix).unwrap_or(&name).to_string()
        })
        .collect();
    markers.sort();
    let mut lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .chain(String::from_utf8_lossy(&out.stderr).lines())
        .map(String::from)
        .collect();
    lines.sort();
    Outcome {
        rc: out.status.code().unwrap(),
        lines,
        refresh_calls: seen.lock().unwrap().len()
            + calls.matches("aws sso-oidc create-token").count(),
        logins: calls.matches("aws sso login --sso-session sg-a").count(),
        asks: calls
            .lines()
            .filter(|l| l.contains("--unit sso-guard-ask-") && l.contains("sg-a"))
            .count(),
        rearm: calls.lines().find_map(|l| {
            l.split_whitespace()
                .find_map(|w| w.strip_prefix("--on-active="))
                .map(|d| d.trim_end_matches('s').parse().unwrap())
        }),
        access_token: cached["accessToken"].as_str().unwrap().to_string(),
        expires_at: epoch(cached["expiresAt"].as_str().unwrap()),
        markers,
    }
}

/// Rust outcome; also asserts bash parity when the oracle is available.
fn run(s: Scenario) -> Outcome {
    let rust = run_one(&s, &[env!("CARGO_BIN_EXE_ccflip"), "sso-guard"], &[]);
    if let Some(oracle) = std::env::var_os("SSO_GUARD_ORACLE") {
        let bash = run_one(&s, &["bash", oracle.to_str().unwrap()], &[]);
        same(&rust, &bash);
    }
    rust
}

fn line(o: &Outcome, wanted: &str) -> bool {
    o.lines.iter().any(|l| l.contains(wanted))
}

#[test]
fn healthy_token_sleeps_until_just_before_the_threshold() {
    let o = run(Scenario {
        markers: &["needs-you", "needs-you.dismissed"],
        ..Default::default()
    });
    assert_eq!((o.rc, o.refresh_calls, o.logins), (0, 0, 0));
    assert!(approx(o.rearm.unwrap(), 7200 - 900 - 60));
    assert!(o.markers.is_empty(), "a healthy token clears stale markers");
    assert_eq!(o.access_token, "old");
}

#[test]
fn low_token_refreshes_silently_and_writes_the_cache_back() {
    let o = run(Scenario {
        left: 600,
        oidc: ok_token(3600),
        ..Default::default()
    });
    assert!(line(&o, "sg-a access token refreshed silently"), "{o:#?}");
    assert_eq!((o.refresh_calls, o.logins), (1, 0));
    assert_eq!(o.access_token, "fresh");
    assert!(approx(o.expires_at, now() + 3600));
    assert!(approx(o.rearm.unwrap(), 3600 - 900 - 60));
}

#[test]
fn capped_session_sleeps_until_it_ends_instead_of_looping() {
    let o = run(Scenario {
        left: 600,
        oidc: ok_token(1200),
        ..Default::default()
    });
    assert!(line(&o, "sg-a session ends "), "{o:#?}");
    assert!(line(&o, ", login then"));
    assert!(approx(o.rearm.unwrap(), 1230));
    assert_eq!(o.logins, 0);
}

#[test]
fn dead_refresh_token_tries_one_browser_login() {
    let o = run(Scenario {
        left: -60,
        ..Default::default()
    });
    assert_eq!((o.refresh_calls, o.logins, o.asks), (1, 1, 0));
    assert_eq!(o.access_token, "login");
    assert_eq!(o.rearm, Some(6 * 3600), "8h token, clamped to the 6h cap");
}

#[test]
fn timed_out_login_asks_via_notification_once() {
    let o = run(Scenario {
        left: -60,
        login_ok: false,
        ..Default::default()
    });
    assert!(line(&o, "sg-a browser login timed out"), "{o:#?}");
    assert!(line(&o, "sg-a needs you, notification posted"));
    assert_eq!((o.logins, o.asks), (1, 1));
    assert_eq!(o.markers, ["sg-a.needs-you"]);
}

#[test]
fn pending_human_login_reasks_without_opening_the_browser() {
    let o = run(Scenario {
        left: -60,
        markers: &["needs-you"],
        ..Default::default()
    });
    assert_eq!((o.logins, o.asks), (0, 1));
}

#[test]
fn dismissed_notification_waits_two_hours() {
    let o = run(Scenario {
        left: -60,
        markers: &["needs-you", "needs-you.dismissed"],
        ..Default::default()
    });
    assert_eq!((o.logins, o.asks), (0, 0));
}

#[test]
fn no_display_fails_and_retries_in_ten_minutes() {
    let o = run(Scenario {
        left: -60,
        graphical: false,
        ..Default::default()
    });
    assert!(
        line(&o, "'sg-a' needs interactive login (no display)"),
        "{o:#?}"
    );
    assert_eq!((o.rc, o.logins, o.rearm), (1, 0, Some(600)));
}

#[test]
fn gated_expired_unused_session_lapses_without_a_refresh_call() {
    let o = run(Scenario {
        left: -60,
        browser: Some("other"),
        ..Default::default()
    });
    assert!(line(&o, "sg-a expired, unused, letting it lapse"), "{o:#?}");
    assert_eq!((o.refresh_calls, o.logins, o.rearm), (0, 0, None));
}

#[test]
fn gated_unused_session_lapses_once_its_idc_session_is_over() {
    let o = run(Scenario {
        left: 300,
        browser: Some("other"),
        ..Default::default()
    });
    assert!(
        line(&o, "sg-a session over, unused, letting it lapse"),
        "{o:#?}"
    );
    assert_eq!((o.refresh_calls, o.logins), (1, 0));
}

#[test]
fn empty_browser_allowlist_means_all_like_bash() {
    let o = run(Scenario {
        left: -60,
        browser: Some(""),
        ..Default::default()
    });
    assert_eq!(o.logins, 1, "{o:#?}");
}

#[test]
fn allowlisted_session_logs_in_whatever_uses_it() {
    let o = run(Scenario {
        left: -60,
        browser: Some("x sg-a y"),
        ..Default::default()
    });
    assert_eq!(o.logins, 1);
}

#[test]
fn gated_session_in_use_by_recent_role_creds_relogs() {
    let o = run(Scenario {
        left: -60,
        browser: Some("other"),
        minted_for: Some("111111111111"),
        ..Default::default()
    });
    assert!(
        line(&o, "sg-a in use (acct 111111111111 creds minted "),
        "{o:#?}"
    );
    assert_eq!(o.logins, 1);
}

#[test]
fn role_creds_of_another_account_do_not_count() {
    let o = run(Scenario {
        left: -60,
        browser: Some("other"),
        minted_for: Some("999999999999"),
        ..Default::default()
    });
    assert!(line(&o, "sg-a expired, unused"), "{o:#?}");
}

/// A live process naming the profile, kept running for the whole test.
fn holder(args: &[&str], env: Option<(&str, &str)>) -> Child {
    // `; :` keeps bash itself alive with this argv instead of exec'ing sleep.
    let mut c = Command::new("bash");
    c.args(["-c", "sleep 30; :", "holder"]).args(args);
    if let Some((k, v)) = env {
        c.env(k, v);
    }
    c.spawn().unwrap()
}

#[test]
fn gated_session_in_use_by_a_process_relogs() {
    for (profile, args, env) in [
        ("sg-hold-1", &["--profile", "sg-hold-1"][..], None),
        ("sg-hold-2", &["--profile=sg-hold-2"][..], None),
        ("sg-hold-3", &[][..], Some(("AWS_PROFILE", "sg-hold-3"))),
    ] {
        let mut p = holder(args, env);
        let o = run(Scenario {
            left: -60,
            browser: Some("other"),
            profile,
            ..Default::default()
        });
        p.kill().unwrap();
        p.wait().unwrap();
        assert!(
            line(&o, &format!("sg-a in use ({} ", p.id())),
            "{args:?} {env:?}: {o:#?}"
        );
        assert_eq!(o.logins, 1);
    }
}

#[test]
fn a_script_merely_mentioning_the_profile_is_not_use() {
    let mut p = holder(&["echo --profile sg-mention"], None);
    let o = run(Scenario {
        left: -60,
        browser: Some("other"),
        profile: "sg-mention",
        ..Default::default()
    });
    p.kill().unwrap();
    p.wait().unwrap();
    assert!(line(&o, "sg-a expired, unused"), "{o:#?}");
}

#[test]
fn disabled_timer_does_nothing_and_does_not_rearm() {
    let o = run(Scenario {
        left: -60,
        timer_enabled: false,
        ..Default::default()
    });
    assert_eq!(o.lines, ["sso-guard: disabled (sso-off), skipping"]);
    assert_eq!((o.refresh_calls, o.logins, o.rearm), (0, 0, None));
}

#[test]
fn journal_times_are_local() {
    for tz in ["UTC", "Asia/Tokyo"] {
        let o = run(Scenario {
            left: 600,
            oidc: ok_token(1200),
            tz,
            ..Default::default()
        });
        let hhmm = Command::new("date")
            .env("TZ", tz)
            .args(["-d", &format!("@{}", o.expires_at), "+%H:%M"])
            .output()
            .unwrap();
        let hhmm = String::from_utf8(hhmm.stdout).unwrap();
        assert!(
            line(&o, &format!("session ends {}, login then", hhmm.trim())),
            "{tz}: {o:#?}"
        );
    }
}

#[test]
fn refresh_keeps_every_other_cache_field_and_stays_private() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path();
    let cache = home.join(".aws/sso/cache");
    fs::create_dir_all(&cache).unwrap();
    fs::write(home.join(".aws/config"), config("sg-a-prof")).unwrap();
    let token = cache.join("tok.json");
    fs::write(
        &token,
        format!(
            r#"{{"startUrl":"{URL}","region":"eu-west-1","accessToken":"old","expiresAt":"{}","clientId":"cid","clientSecret":"csec","refreshToken":"rt","extra":{{"keep":1}}}}"#,
            stamp(now() + 60)
        ),
    )
    .unwrap();
    // Another session's entry, and an entry for ours whose region would
    // redirect the secrets elsewhere: neither is posted, and the bad one does
    // not stop the good one from being refreshed.
    fs::write(
        cache.join("other.json"),
        r#"{"startUrl":"https://b.example/start","region":"eu-west-1","clientId":"x","clientSecret":"y","refreshToken":"z"}"#,
    )
    .unwrap();
    let bad = format!(
        r#"{{"startUrl":"{URL}","region":"evil.example/x?","clientId":"x","clientSecret":"y","refreshToken":"z"}}"#
    );
    fs::write(cache.join("bad.json"), &bad).unwrap();
    let (endpoint, seen) = serve(ok_token(3600));
    let bin = home.join("bin");
    fs::create_dir_all(&bin).unwrap();
    write_exec(&bin.join("systemctl"), "#!/bin/sh\nexit 0\n");
    write_exec(&bin.join("systemd-run"), "#!/bin/sh\nexit 0\n");
    let status = Command::new(env!("CARGO_BIN_EXE_ccflip"))
        .arg("sso-guard")
        .env_clear()
        .env("HOME", home)
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("AWS_ENDPOINT_URL_SSO_OIDC", &endpoint)
        .status()
        .unwrap();
    assert!(status.success());
    let v: serde_json::Value = serde_json::from_slice(&fs::read(&token).unwrap()).unwrap();
    assert_eq!(v["accessToken"], "fresh");
    assert_eq!(v["refreshToken"], "rt2", "a rotated refresh token is kept");
    assert_eq!(v["extra"]["keep"], 1);
    assert_eq!(v["clientSecret"], "csec");
    assert_eq!(
        fs::metadata(&token).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let body: serde_json::Value = serde_json::from_str(&seen[0]).unwrap();
    assert_eq!(
        body,
        serde_json::json!({"clientId":"cid","clientSecret":"csec","grantType":"refresh_token","refreshToken":"rt"})
    );
    assert_eq!(fs::read_to_string(cache.join("bad.json")).unwrap(), bad);
    assert_eq!(
        fs::read_dir(&cache).unwrap().count(),
        3,
        "no temp file left"
    );
}

#[test]
fn hostile_region_is_never_contacted() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path();
    let cache = home.join(".aws/sso/cache");
    fs::create_dir_all(&cache).unwrap();
    fs::write(home.join(".aws/config"), config("sg-a-prof")).unwrap();
    fs::write(
        cache.join("tok.json"),
        format!(
            r#"{{"startUrl":"{URL}","region":"evil.example/x?","accessToken":"old","expiresAt":"{}","clientId":"cid","clientSecret":"csec","refreshToken":"rt"}}"#,
            stamp(now() + 60)
        ),
    )
    .unwrap();
    let bin = home.join("bin");
    fs::create_dir_all(&bin).unwrap();
    write_exec(&bin.join("systemctl"), "#!/bin/sh\nexit 0\n");
    write_exec(&bin.join("systemd-run"), "#!/bin/sh\nexit 0\n");
    // No endpoint override: a refresh attempt would have to build the host
    // from the region. Not graphical, so the run must just report and fail.
    let out = Command::new(env!("CARGO_BIN_EXE_ccflip"))
        .arg("sso-guard")
        .env_clear()
        .env("HOME", home)
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let v: serde_json::Value =
        serde_json::from_slice(&fs::read(cache.join("tok.json")).unwrap()).unwrap();
    assert_eq!(v["accessToken"], "old");
}

/// The detached notification half: click = login + rerun, dismiss = mark.
#[test]
fn ask_flow_click_logs_in_and_dismiss_marks() {
    for (action, logged_in) in [("default", true), ("", false)] {
        let t = tempfile::tempdir().unwrap();
        let home = t.path();
        let state = home.join("state/sso-guard");
        fs::create_dir_all(&state).unwrap();
        fs::write(state.join("aws.sg-a.needs-you"), "").unwrap();
        let bin = home.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let calls = home.join("calls");
        for tool in ["systemctl", "aws"] {
            write_exec(
                &bin.join(tool),
                &format!("#!/bin/sh\necho \"{tool} $*\" >> \"$CALLS\"\n"),
            );
        }
        write_exec(
            &bin.join("notify-send"),
            &format!("#!/bin/sh\necho '{action}'\n"),
        );
        let status = Command::new(env!("CARGO_BIN_EXE_ccflip"))
            .arg("sso-guard")
            .env_clear()
            .env("HOME", home)
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("XDG_STATE_HOME", home.join("state"))
            .env("CALLS", &calls)
            .env("SSO_GUARD_ASK", "aws.sg-a")
            .status()
            .unwrap();
        assert!(status.success());
        let calls = fs::read_to_string(&calls).unwrap_or_default();
        let markers: BTreeMap<_, _> = fs::read_dir(&state)
            .unwrap()
            .map(|e| (e.unwrap().file_name().into_string().unwrap(), ()))
            .collect();
        if logged_in {
            assert!(
                calls.contains("aws sso login --sso-session sg-a"),
                "{calls}"
            );
            assert!(calls.contains("systemctl --user --quiet start --no-block sso-guard.service"));
            assert!(markers.is_empty(), "{markers:?}");
        } else {
            assert!(!calls.contains("aws sso login"), "{calls}");
            assert!(markers.contains_key("aws.sg-a.needs-you.dismissed"));
        }
    }
}

/// Real botocore reads what we wrote: `aws` loads the refreshed token from
/// the cache and presents it to (our fake) SSO. Needs the AWS CLI v2.
#[test]
#[ignore = "needs the real AWS CLI v2 on PATH"]
fn botocore_accepts_the_cache_we_write() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path();
    let cache = home.join(".aws/sso/cache");
    fs::create_dir_all(&cache).unwrap();
    let config = format!(
        "{}sso_role_name = R\nregion = eu-west-1\n",
        config("sg-a-prof")
    );
    fs::write(home.join(".aws/config"), &config).unwrap();
    // botocore keys the token file by sha1(session name).
    let key = Command::new("sh")
        .args(["-c", "printf %s sg-a | sha1sum | cut -d' ' -f1"])
        .output()
        .unwrap();
    let token = cache.join(format!(
        "{}.json",
        String::from_utf8(key.stdout).unwrap().trim()
    ));
    fs::write(
        &token,
        format!(
            r#"{{"startUrl":"{URL}","region":"eu-west-1","accessToken":"old","expiresAt":"{}","clientId":"cid","clientSecret":"csec","refreshToken":"rt","registrationExpiresAt":"2099-01-01T00:00:00Z"}}"#,
            stamp(now() + 60)
        ),
    )
    .unwrap();
    let (endpoint, _) = serve(ok_token(3600));
    let bin = home.join("bin");
    fs::create_dir_all(&bin).unwrap();
    write_exec(&bin.join("systemctl"), "#!/bin/sh\nexit 0\n");
    write_exec(&bin.join("systemd-run"), "#!/bin/sh\nexit 0\n");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let status = Command::new(env!("CARGO_BIN_EXE_ccflip"))
        .arg("sso-guard")
        .env("HOME", home)
        .env("PATH", &path)
        .env("AWS_ENDPOINT_URL_SSO_OIDC", &endpoint)
        .status()
        .unwrap();
    assert!(status.success());

    // GetRoleCredentials goes to a server that records the bearer token.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let sso = format!("http://{}", listener.local_addr().unwrap());
    let bearer = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut bearer = String::new();
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap() > 0 && line != "\r\n" {
            if let Some(v) = line
                .to_ascii_lowercase()
                .strip_prefix("x-amz-sso_bearer_token:")
            {
                bearer = v.trim().to_string();
            }
            line.clear();
        }
        let body = r#"{"roleCredentials":{"accessKeyId":"AK","secretAccessKey":"SK","sessionToken":"ST","expiration":4102444800000}}"#;
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        (&stream).write_all(reply.as_bytes()).unwrap();
        bearer
    });
    let out = Command::new("aws")
        .args(["configure", "export-credentials", "--profile", "sg-a-prof"])
        .env("HOME", home)
        .env("AWS_ENDPOINT_URL_SSO", &sso)
        .env("AWS_ENDPOINT_URL_SSO_OIDC", "http://127.0.0.1:9") // must not refresh again
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(bearer.join().unwrap(), "fresh");
}
