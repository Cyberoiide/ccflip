//! One watcher tick against fakes: profile rotation and the containment
//! guard. Port of the old tests/policy_guard_test.sh, plus the case the shell
//! test could not express — that a profile already moved onto its target is
//! NOT switched again on the next tick.
//!
//! Tier switching (the part that ends processes) is disabled here: this box
//! runs real claude sessions.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

/// 2 = windowed and dry, 4 = spend-billed and fresh (the rotation target).
const LIST: &str = r#"{"schemaVersion":1,"activeAccountNumber":4,"accounts":[
 {"number":2,"email":"qm@x.com","active":false,"usageStatus":"ok",
  "usage":{"fiveHour":{"pct":99.0,"resetsAt":"x"},"sevenDay":{"pct":50.0,"resetsAt":"x"}}},
 {"number":4,"email":"api@x.com","active":true,"usageStatus":"ok",
  "usage":{"spend":{"used":10.0,"limit":100.0,"pct":10.0}}}
]}"#;

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let f = Fixture {
            dir: tempfile::tempdir().unwrap(),
        };
        let cswap = f.path().join("cswap");
        fs::write(
            &cswap,
            r#"#!/usr/bin/env bash
case "$1" in
  list)    cat "$LIST_FIXTURE" ;;
  switch)  echo "switch $2 CONFIG_DIR=${CLAUDE_CONFIG_DIR:-none}" >> "$CALLS" ;;
  disable) echo "disable $2" >> "$CALLS" ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(&cswap, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(f.path().join("list.json"), LIST).unwrap();
        f
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// A session profile seeded for account 2, currently logged into `email`.
    fn profile(&self, email: Option<&str>) {
        let dir = self.path().join("claude-swap/sessions/2-qm_x.com");
        fs::create_dir_all(&dir).unwrap();
        match email {
            Some(email) => fs::write(
                dir.join(".claude.json"),
                format!(r#"{{"oauthAccount":{{"emailAddress":"{email}"}}}}"#),
            )
            .unwrap(),
            None => {
                let _ = fs::remove_file(dir.join(".claude.json"));
            }
        }
    }

    fn policy(&self, content: &str) {
        fs::write(self.path().join("policy.env"), content).unwrap();
    }

    /// Run exactly one tick; returns the recorded cswap mutations.
    fn tick(&self) -> String {
        let calls = self.path().join("calls");
        let _ = fs::remove_file(&calls);
        let status = Command::new(env!("CARGO_BIN_EXE_ccflip"))
            .arg("watch")
            .env("HOME", self.path())
            .env("XDG_DATA_HOME", self.path())
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.path().display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env("LIST_FIXTURE", self.path().join("list.json"))
            .env("CALLS", &calls)
            .env("CCFLIP_POLICY", self.path().join("policy.env"))
            .env("CCFLIP_WATCH_ONCE", "1")
            .env("CCFLIP_NO_TIER_SWITCH", "1") // never touch real processes
            .status()
            .unwrap();
        assert!(status.success());
        fs::read_to_string(&calls).unwrap_or_default()
    }
}

#[test]
fn rotates_an_exhausted_profile_exactly_once() {
    let f = Fixture::new();

    // Profile still on its dry seeded account -> rotate onto account 4.
    f.profile(Some("qm@x.com"));
    let calls = f.tick();
    assert!(
        calls.contains("switch 4 CONFIG_DIR=") && calls.lines().count() == 1,
        "expected one switch to 4, got:\n{calls}"
    );

    // Same profile after that switch: the directory name still says account
    // 2, but the live login is 4 and healthy — a second switch here is the
    // every-tick rewrite loop that reading the name would cause.
    f.profile(Some("api@x.com"));
    assert_eq!(f.tick(), "", "healthy rotated profile must be left alone");

    // Never-used profile (no .claude.json yet): the seeded name is all there
    // is, so rotation still happens.
    f.profile(None);
    assert!(f.tick().contains("switch 4"), "seeded profile must rotate");
}

#[test]
fn containment_guard_enforces_policy_env() {
    let f = Fixture::new();
    f.profile(Some("api@x.com")); // healthy: keeps rotation out of the way

    // Account 4 is the default login (active) and pinned-only -> switch away
    // and re-disable it.
    f.policy("CCFLIP_PINNED_ONLY=api@x.com\nCCFLIP_DEFAULT_ACCOUNT=qm@x.com\n");
    let calls = f.tick();
    assert!(calls.contains("switch qm@x.com"), "got:\n{calls}");
    assert!(calls.contains("disable api@x.com"), "got:\n{calls}");

    // Same policy without a default to fall back to: never disable the live
    // default login (active-and-disabled is a state nothing rotates out of).
    f.policy("CCFLIP_PINNED_ONLY=api@x.com\n");
    assert_eq!(
        f.tick(),
        "",
        "no CCFLIP_DEFAULT_ACCOUNT -> leave it enabled"
    );

    // A knob set in policy.env is honoured (the README's promise): raising
    // the spend line past 10% makes account 4 un-electable, so the dry
    // profile has nowhere to go.
    f.profile(Some("qm@x.com"));
    f.policy("CCFLIP_SPEND_THRESHOLD=5\n");
    assert_eq!(f.tick(), "", "spend threshold from policy.env must apply");
}
