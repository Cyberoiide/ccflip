//! The provider framework end to end: `ccflip status` and the gcloud
//! provider against a fake `gcloud`, knobs from policy.env. AWS decisions
//! themselves are covered (and parity-checked) in tests/sso_guard.rs.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn write_exec(path: &Path, content: &str) {
    fs::write(path, content).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
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

const DEAD: &str = "ERROR: (gcloud.auth.print-access-token) There was a problem refreshing your current auth tokens: ('invalid_grant: Bad Request', {'error': 'invalid_grant'})\nPlease run:\n\n  $ gcloud auth login\n\nto obtain new credentials.";
const OFFLINE: &str = "ERROR: (gcloud.auth.print-access-token) HTTPSConnectionPool(host='oauth2.googleapis.com', port=443): Max retries exceeded";
const TOKEN: &str = "ya29.SECRET-TOKEN";

/// A fake home with fake tools. `gcloud` answers per account from
/// `$HOME/gc/<account>` (ok | dead | offline); `auth list` from
/// `$HOME/gc/list.json`.
struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    fn new() -> Self {
        let h = Home {
            dir: tempfile::tempdir().unwrap(),
        };
        for d in ["bin", "gc", "state/sso-guard", ".aws/sso/cache", "gcloud"] {
            fs::create_dir_all(h.path().join(d)).unwrap();
        }
        let bin = h.path().join("bin");
        write_exec(
            &bin.join("systemctl"),
            "#!/bin/sh\necho \"systemctl $*\" >> \"$CALLS\"\ncase \"$*\" in *is-active*) exit 1;; esac\n",
        );
        for tool in ["systemd-run", "aws"] {
            write_exec(
                &bin.join(tool),
                &format!("#!/bin/sh\necho \"{tool} $*\" >> \"$CALLS\"\n"),
            );
        }
        write_exec(
            &bin.join("notify-send"),
            "#!/bin/sh\necho \"notify-send $*\" >> \"$CALLS\"\necho \"$NOTIFY\"\n",
        );
        write_exec(
            &bin.join("gcloud"),
            &format!(
                r#"#!/usr/bin/env bash
echo "gcloud $*" >> "$CALLS"
case "$*" in
  "auth list --format=json") cat "$HOME/gc/list.json" ;;
  "auth print-access-token --account="*) acct=${{!#}}; acct=${{acct#--account=}} ;;
  "auth application-default print-access-token") acct=application-default ;;
  *) exit 0 ;;
esac
[[ -n "${{acct:-}}" ]] || exit 0
case "$(cat "$HOME/gc/$acct")" in
  ok) echo "{TOKEN}" ;;
  dead) echo "{DEAD}" >&2; exit 1 ;;
  *) echo "{OFFLINE}" >&2; exit 1 ;;
esac
"#
            ),
        );
        h
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn gcloud(&self, accounts: &[(&str, bool, &str)], adc: Option<&str>) {
        let list: Vec<String> = accounts
            .iter()
            .map(|(a, active, _)| {
                format!(
                    r#"{{"account":"{a}","status":"{}"}}"#,
                    if *active { "ACTIVE" } else { "" }
                )
            })
            .collect();
        fs::write(
            self.path().join("gc/list.json"),
            format!("[{}]", list.join(",")),
        )
        .unwrap();
        for (a, _, state) in accounts {
            fs::write(self.path().join("gc").join(a), state).unwrap();
        }
        if let Some(state) = adc {
            fs::write(
                self.path()
                    .join("gcloud/application_default_credentials.json"),
                "{}",
            )
            .unwrap();
            fs::write(self.path().join("gc/application-default"), state).unwrap();
        }
    }

    fn marker(&self, key: &str) -> PathBuf {
        self.path().join(format!("state/sso-guard/{key}.needs-you"))
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.path().join("calls")).unwrap_or_default()
    }

    fn ccflip(&self, args: &[&str], env: &[(&str, &str)]) -> String {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ccflip"));
        cmd.args(args)
            .env_clear()
            .env("HOME", self.path())
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.path().join("bin").display()),
            )
            .env("XDG_STATE_HOME", self.path().join("state"))
            .env("CLOUDSDK_CONFIG", self.path().join("gcloud"))
            .env("CALLS", self.path().join("calls"))
            .env("CCFLIP_POLICY", self.path().join("policy.env"))
            .env("WAYLAND_DISPLAY", "wayland-1");
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !text.contains(TOKEN),
            "an access token leaked into output:\n{text}"
        );
        text
    }

    fn status(&self) -> Vec<serde_json::Value> {
        serde_json::from_str(&self.ccflip(&["status"], &[])).unwrap()
    }
}

#[test]
fn dead_active_gcloud_account_asks_once_others_are_only_reported() {
    let h = Home::new();
    h.gcloud(
        &[("me@x.com", true, "dead"), ("old@x.com", false, "dead")],
        None,
    );
    let out = h.ccflip(&["sso-guard"], &[]);
    assert!(
        out.contains("gcloud me@x.com credentials are dead, asking for a login"),
        "{out}"
    );
    assert!(
        out.contains("gcloud old@x.com credentials are dead (not the active account"),
        "{out}"
    );
    let calls = h.calls();
    // `@` is not allowed in a unit name.
    assert!(
        calls.contains("--unit sso-guard-ask-gcloud.me_x.com"),
        "{calls}"
    );
    assert!(
        calls.contains("--setenv=SSO_GUARD_ASK=gcloud.me@x.com"),
        "{calls}"
    );
    assert!(!calls.contains("sso-guard-ask-gcloud.old"), "{calls}");
    assert!(
        !calls.contains("gcloud auth login"),
        "never opens a browser unasked: {calls}"
    );
    assert!(h.marker("gcloud.me@x.com").exists());
}

#[test]
fn odd_account_names_stay_inside_the_state_dir() {
    let h = Home::new();
    // `gc/../x@x.com` is still inside the temp home: the fake can answer it.
    h.gcloud(&[("../x@x.com", true, "dead")], None);
    h.ccflip(&["sso-guard"], &[]);
    assert!(h.marker("gcloud..._x@x.com").exists());
    assert!(!h.path().join("state/x@x.com.needs-you").exists());
}

#[test]
fn network_trouble_never_bothers_a_human() {
    let h = Home::new();
    h.gcloud(&[("me@x.com", true, "offline")], None);
    let out = h.ccflip(&["sso-guard"], &[]);
    assert!(
        out.contains("gcloud me@x.com: could not check, retrying next run"),
        "{out}"
    );
    assert!(!h.calls().contains("sso-guard-ask"));
}

#[test]
fn healthy_gcloud_account_clears_a_stale_marker() {
    let h = Home::new();
    h.gcloud(&[("me@x.com", true, "ok")], None);
    fs::write(h.marker("gcloud.me@x.com"), "").unwrap();
    h.ccflip(&["sso-guard"], &[]);
    assert!(!h.marker("gcloud.me@x.com").exists());
}

#[test]
fn dead_application_default_credentials_ask() {
    let h = Home::new();
    h.gcloud(&[], Some("dead"));
    let out = h.ccflip(&["sso-guard"], &[]);
    assert!(
        out.contains("gcloud application-default credentials are dead"),
        "{out}"
    );
    assert!(
        h.calls()
            .contains("SSO_GUARD_ASK=gcloud.application-default")
    );
}

#[test]
fn no_gcloud_installed_means_no_gcloud_accounts() {
    let h = Home::new();
    fs::remove_file(h.path().join("bin/gcloud")).unwrap();
    // An ADC file alone (left by another tool) must not produce probes.
    fs::write(
        h.path().join("gcloud/application_default_credentials.json"),
        "{}",
    )
    .unwrap();
    assert!(h.status().is_empty());
}

#[test]
fn clicking_a_gcloud_notification_runs_its_login() {
    for (key, wanted) in [
        ("gcloud.me@x.com", "gcloud auth login me@x.com"),
        (
            "gcloud.application-default",
            "gcloud auth application-default login",
        ),
    ] {
        let h = Home::new();
        h.ccflip(
            &["sso-guard"],
            &[("SSO_GUARD_ASK", key), ("NOTIFY", "default")],
        );
        let calls = h.calls();
        assert!(calls.contains(wanted), "{key}: {calls}");
        assert!(
            calls.contains("notify-send -u critical -t 0 -a sso-guard -A default=Log in gcloud: ")
        );
    }
}

#[test]
fn unknown_ask_key_fails_loudly() {
    let h = Home::new();
    let out = h.ccflip(&["sso-guard"], &[("SSO_GUARD_ASK", "nope.x")]);
    assert!(out.contains("unknown SSO_GUARD_ASK=nope.x"), "{out}");
}

#[test]
fn status_lists_every_provider_account() {
    let h = Home::new();
    fs::write(
        h.path().join(".aws/config"),
        "[sso-session ok]\nsso_start_url = https://ok/start\n[profile p1]\nsso_session = ok\n\
         [sso-session low]\nsso_start_url = https://low/start\n[profile p2]\nsso_session = low\n\
         [sso-session gone]\nsso_start_url = https://gone/start\n[profile p3]\nsso_session = gone\n",
    )
    .unwrap();
    let ok_until = now() + 7200;
    for (name, url, left) in [
        ("a", "https://ok/start", 7200),
        ("b", "https://low/start", 300),
    ] {
        fs::write(
            h.path().join(format!(".aws/sso/cache/{name}.json")),
            format!(
                r#"{{"startUrl":"{url}","expiresAt":"{}"}}"#,
                stamp(now() + left)
            ),
        )
        .unwrap();
    }
    fs::write(h.marker("aws.gone"), "").unwrap();
    h.gcloud(
        &[("me@x.com", true, "ok"), ("old@x.com", false, "dead")],
        Some("offline"),
    );

    let rows = h.status();
    let row = |p: &str, a: &str| {
        rows.iter()
            .find(|r| r["provider"] == p && r["account"] == a)
            .unwrap_or_else(|| panic!("no {p}/{a} in {rows:#?}"))
            .clone()
    };
    assert_eq!(rows.len(), 6, "{rows:#?}");
    assert_eq!(row("aws", "ok")["state"], "ok");
    assert_eq!(row("aws", "ok")["expires_at"], stamp(ok_until));
    assert_eq!(row("aws", "low")["state"], "expiring");
    assert_eq!(row("aws", "gone")["state"], "expired");
    assert!(row("aws", "gone").get("expires_at").is_none());
    assert_eq!(row("aws", "gone")["needs_you"], true);
    assert_eq!(row("gcloud", "me@x.com")["state"], "ok");
    assert_eq!(row("gcloud", "old@x.com")["state"], "expired");
    assert_eq!(row("gcloud", "application-default")["state"], "error");
    assert_eq!(row("gcloud", "me@x.com")["needs_you"], false);
    // status only looks: no login, no notification, no timer.
    let calls = h.calls();
    assert!(
        !calls.contains("login") && !calls.contains("systemd-run"),
        "{calls}"
    );
}

#[test]
fn guard_knobs_come_from_policy_env_and_the_environment_wins() {
    let h = Home::new();
    fs::write(
        h.path().join(".aws/config"),
        "[sso-session s]\nsso_start_url = https://s/start\n[profile p]\nsso_session = s\n",
    )
    .unwrap();
    fs::write(
        h.path().join(".aws/sso/cache/t.json"),
        format!(
            r#"{{"startUrl":"https://s/start","expiresAt":"{}"}}"#,
            stamp(now() - 60)
        ),
    )
    .unwrap();
    fs::write(h.path().join("policy.env"), "SSO_GUARD_BROWSER=other\n").unwrap();
    let out = h.ccflip(&["sso-guard"], &[]);
    assert!(out.contains("s expired, unused, letting it lapse"), "{out}");
    let out = h.ccflip(&["sso-guard"], &[("SSO_GUARD_BROWSER", "s")]);
    assert!(h.calls().contains("aws sso login --sso-session s"), "{out}");
}

/// The real gcloud CLI with a dead refresh token in a throwaway config:
/// its actual error text must classify as expired, a dead network as error.
#[test]
#[ignore = "needs the real gcloud CLI on PATH and network"]
fn real_gcloud_dead_and_offline_credentials() {
    let t = tempfile::tempdir().unwrap();
    let config = t.path().join("gcloud");
    fs::create_dir_all(&config).unwrap();
    // gcloud's own public installed-app client; the refresh token is bogus.
    let seeded = Command::new("python3")
        .args(["-I", "-c", r#"
import sqlite3, sys, json
c = sqlite3.connect(sys.argv[1] + "/credentials.db")
c.execute("create table credentials (account_id TEXT PRIMARY KEY, value BLOB)")
c.execute("insert into credentials values (?, ?)", ("dead@example.com", json.dumps({
    "type": "authorized_user", "refresh_token": "1//bogus",
    "client_id": "32555940559.apps.googleusercontent.com", "client_secret": "ZmssLNjJy2998hD4CTg2ejr2"})))
c.commit()
"#])
        .arg(&config)
        .status()
        .unwrap();
    assert!(seeded.success());
    let gcloud = |args: &[&str]| {
        Command::new("gcloud")
            .args(args)
            .env("CLOUDSDK_CONFIG", &config)
            .output()
            .unwrap()
    };
    gcloud(&["config", "set", "account", "dead@example.com"]);
    let status = |extra: &[(&str, &str)]| -> serde_json::Value {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ccflip"));
        cmd.arg("status")
            .env("HOME", t.path())
            .env("CLOUDSDK_CONFIG", &config)
            .env("XDG_STATE_HOME", t.path().join("state"));
        for (k, v) in extra {
            cmd.env(k, v);
        }
        let rows: Vec<serde_json::Value> =
            serde_json::from_slice(&cmd.output().unwrap().stdout).unwrap();
        rows.into_iter()
            .find(|r| r["account"] == "dead@example.com")
            .unwrap()
    };
    assert_eq!(status(&[])["state"], "expired");
    assert_eq!(
        status(&[("HTTPS_PROXY", "http://127.0.0.1:9")])["state"],
        "error"
    );
}
