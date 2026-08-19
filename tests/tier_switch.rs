//! The destructive path, end to end: the watcher scanning real /proc and
//! actually signalling a process. The victim is our own `sleep` copy.
//!
//! `#[ignore]` on purpose — run it with `cargo test -- --ignored`. It scans
//! the WHOLE process table, so on a machine with live claude sessions a
//! fixture account could match one of them and end it. The decision logic
//! itself is covered by the pure `decide` tests in src/watch.rs, which are
//! safe to run anywhere.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::Duration;

/// Only account is out of quota: nothing to rotate to.
const DRY: &str = r#"{"activeAccountNumber":1,"accounts":[
 {"number":1,"email":"dry@x.com","active":true,"usageStatus":"ok",
  "usage":{"fiveHour":{"pct":100.0},"sevenDay":{"pct":100.0}}}]}"#;
/// Only account is fresh: the subscription tier is ready again.
const HEALTHY: &str = r#"{"activeAccountNumber":1,"accounts":[
 {"number":1,"email":"ok@x.com","active":true,"usageStatus":"ok",
  "usage":{"fiveHour":{"pct":5.0},"sevenDay":{"pct":5.0}}}]}"#;

struct Case {
    dir: tempfile::TempDir,
    parent: Child,
    victim: u32,
}

impl Case {
    /// A process the watcher will recognise: a real binary named `claude`
    /// (a shell script's /proc/exe would be the interpreter) spawned by a
    /// node parent, as an SDK harness does.
    fn spawn(fixture: &str, victim_env: &[(&str, &str)], unset_bedrock: bool) -> Option<Case> {
        let dir = tempfile::tempdir().unwrap();
        let t = dir.path();
        fs::copy("/bin/sleep", t.join("claude")).unwrap();
        let cswap = t.join("cswap");
        fs::write(
            &cswap,
            "#!/usr/bin/env bash\n[ \"$1\" = list ] && cat \"$LIST_FIXTURE\"\n",
        )
        .unwrap();
        fs::set_permissions(&cswap, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(t.join("list.json"), fixture).unwrap();
        fs::write(t.join("bedrock.env"), "export CLAUDE_CODE_USE_BEDROCK=1\n").unwrap();
        // An idle transcript for this cwd: without the directory the idle
        // check fails closed and no switchback can happen.
        let slug: String = t
            .display()
            .to_string()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let projects = t.join(".claude/projects").join(slug);
        fs::create_dir_all(&projects).unwrap();
        fs::write(projects.join("old.jsonl"), "{}").unwrap();
        let _ = Command::new("touch")
            .args(["-d", "2 hours ago"])
            .arg(projects.join("old.jsonl"))
            .status();

        let mut node = Command::new("node");
        node.arg("-e")
            .arg(format!(
                "require('child_process').spawn('{}/claude',['600'],{{cwd:'{}',stdio:'ignore'}});setTimeout(()=>{{}},900000)",
                t.display(),
                t.display()
            ))
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if unset_bedrock {
            node.env_remove("CLAUDE_CODE_USE_BEDROCK");
        } else {
            node.env("CLAUDE_CODE_USE_BEDROCK", "1");
        }
        for (k, v) in victim_env {
            node.env(k, v);
        }
        let parent = node.spawn().ok()?; // no node on this box: caller skips
        sleep(Duration::from_millis(1500));
        let victim = pgrep(&t.join("claude"))?;
        Some(Case {
            dir,
            parent,
            victim,
        })
    }

    /// One tick with tier switching live. Returns its log lines.
    fn tick(&self, extra: &[(&str, &str)]) -> String {
        let t = self.dir.path();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ccflip"));
        cmd.arg("watch")
            .env("HOME", t)
            .env("XDG_DATA_HOME", t)
            .env("PATH", format!("{}:/usr/bin:/bin", t.display()))
            .env("LIST_FIXTURE", t.join("list.json"))
            .env("CCFLIP_BEDROCK_ENV", t.join("bedrock.env"))
            .env("CCFLIP_POLICY", t.join("no-policy"))
            .env("CCFLIP_LIMIT_FLAGS", t.join("flags.json"))
            .env("CCFLIP_PROC_MIN_AGE", "0")
            .env("CCFLIP_WATCH_ONCE", "1")
            .env_remove("CCFLIP_NO_TIER_SWITCH");
        for (k, v) in extra {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        sleep(Duration::from_millis(500));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn victim_alive(&self) -> bool {
        Path::new(&format!("/proc/{}", self.victim)).exists()
    }
}

impl Drop for Case {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .args([self.victim.to_string(), self.parent.id().to_string()])
            .status();
        let _ = self.parent.kill();
    }
}

fn pgrep(binary: &Path) -> Option<u32> {
    let out = Command::new("pgrep")
        .args(["-f", &format!("^{} 600", binary.display())])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()?
        .trim()
        .parse()
        .ok()
}

#[test]
#[ignore = "scans the real process table; run explicitly with --ignored"]
fn ends_and_spares_processes_as_decided() {
    // Exhausted account, nothing to rotate to, Bedrock configured: end it so
    // the harness revives the session on Bedrock.
    let Some(case) = Case::spawn(DRY, &[], true) else {
        eprintln!("skipped: node is not available to act as the harness parent");
        return;
    };
    let log = case.tick(&[]);
    assert!(log.contains("failover: ended process"), "log was:\n{log}");
    assert!(!case.victim_alive(), "failover must end the process");
    drop(case);

    // Same, but this host has no Bedrock tier: ending it would only loop.
    let case = Case::spawn(DRY, &[], true).unwrap();
    let log = case.tick(&[("CCFLIP_BEDROCK_ENV", "/nonexistent/bedrock.env")]);
    assert!(!log.contains("failover"), "log was:\n{log}");
    assert!(case.victim_alive(), "no Bedrock tier: must be spared");
    drop(case);

    // On Bedrock while the subscription tier is healthy and the transcript
    // has been quiet: end it so the session revives on the account.
    let case = Case::spawn(HEALTHY, &[], false).unwrap();
    let log = case.tick(&[]);
    assert!(
        log.contains("switchback: ended bedrock process"),
        "log was:\n{log}"
    );
    assert!(!case.victim_alive(), "switchback must end the process");
    drop(case);

    // The caller opted this process out: never touched, whatever the state.
    let case = Case::spawn(DRY, &[("CCFLIP_NO_TIER_SWITCH", "1")], true).unwrap();
    let log = case.tick(&[]);
    assert!(!log.contains("failover"), "log was:\n{log}");
    assert!(case.victim_alive(), "opted-out process must be spared");
}
