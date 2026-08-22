//! End-to-end check of ccflip's routing with fakes — no accounts consumed.
//! Fake cswap serves a fixture for `list --json`; fake claude and fake
//! `cswap run` record what reached them. Direct port of the old
//! tests/bedrock_fallback_test.sh.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

fn write_exec(path: &Path, content: &str) {
    fs::write(path, content).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Fixture: 1=sia (spend-only, no windows), 2=qm (windowed, disabled, mapped).
fn list_json(qm_pct: &str, sia_status: &str, sia_spend_pct: &str) -> String {
    format!(
        r#"{{"schemaVersion":1,"activeAccountNumber":1,"accounts":[
 {{"number":1,"email":"sia@x.com","active":true,"usageStatus":"{sia_status}",
  "usage":{{"spend":{{"used":14.0,"limit":100.0,"pct":{sia_spend_pct}}}}}}},
 {{"number":2,"email":"qm@x.com","active":false,"usageStatus":"ok","disabled":true,
  "usage":{{"fiveHour":{{"pct":{qm_pct},"resetsAt":"x"}},"sevenDay":{{"pct":5,"resetsAt":"x"}}}}}}
]}}"#
    )
}

#[test]
fn routing() {
    let t = tempfile::tempdir().unwrap();
    let t = t.path();

    write_exec(
        &t.join("claude"),
        r#"#!/usr/bin/env bash
{ echo "VIA=claude BEDROCK=${CLAUDE_CODE_USE_BEDROCK:-unset} ROUTED=${CCFLIP_ROUTED:-unset}"
  echo "ARGS=$*"; } > "$OUT"
"#,
    );
    write_exec(
        &t.join("cswap"),
        r#"#!/usr/bin/env bash
case "$1" in
  list) cat "$LIST_FIXTURE" ;;
  run)  shift; acct=""; [[ "${1:-}" != "--" && -n "${1:-}" ]] && { acct=$1; shift; }
        [[ "${1:-}" == "--" ]] && shift
        { echo "VIA=cswap-run ACCT=${acct:-default} BEDROCK=${CLAUDE_CODE_USE_BEDROCK:-unset} ROUTED=${CCFLIP_ROUTED:-unset}"
          echo "ARGS=$*"; } > "$OUT" ;;
esac
"#,
    );

    let qmdir = t.join("quiet-mind");
    fs::create_dir_all(qmdir.join("darkfindr")).unwrap();
    fs::write(
        t.join("mappings.json"),
        format!(
            r#"{{"schemaVersion":1,"mappings":{{"{}":{{"email":"qm@x.com"}}}}}}"#,
            qmdir.display()
        ),
    )
    .unwrap();
    fs::write(t.join("bedrock.env"), "export CLAUDE_CODE_USE_BEDROCK=1\n").unwrap();

    let list_spend = |qm_pct: &str, sia_status: &str, sia_spend_pct: &str| {
        fs::write(
            t.join("list.json"),
            list_json(qm_pct, sia_status, sia_spend_pct),
        )
        .unwrap();
    };
    let list = |qm_pct: &str, sia_status: &str| list_spend(qm_pct, sia_status, "14");
    let run_pwd = |cwd: &Path, pwd: &Path, stale_bedrock: bool, args: &[&str]| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ccflip"));
        cmd.args(args)
            .current_dir(cwd)
            // HOME pinned into the tempdir so ~/.local/bin can never leak
            // real binaries into the fake PATH.
            .env("HOME", t)
            .env(
                "PATH",
                format!("{}:{}", t.display(), std::env::var("PATH").unwrap()),
            )
            .env("PWD", pwd)
            .env("OUT", t.join("out"))
            .env("LIST_FIXTURE", t.join("list.json"))
            .env("CCFLIP_MAPPINGS", t.join("mappings.json"))
            .env("CCFLIP_BEDROCK_ENV", t.join("bedrock.env"))
            .env_remove("CCFLIP_THRESHOLD")
            .env_remove("CCFLIP_BEDROCK_MODEL_OVERRIDE")
            .env_remove("CLAUDE_CODE_USE_BEDROCK");
        if stale_bedrock {
            cmd.env("CLAUDE_CODE_USE_BEDROCK", "1");
        }
        assert!(cmd.status().unwrap().success());
        fs::read_to_string(t.join("out")).unwrap()
    };
    let run =
        |cwd: &Path, stale_bedrock: bool, args: &[&str]| run_pwd(cwd, cwd, stale_bedrock, args);
    let check = |out: &str, wanted: &str, case: &str| {
        assert!(
            out.lines().any(|l| l == wanted),
            "{case}: wanted '{wanted}', got:\n{out}"
        );
    };

    let hello: &[&str] = &["-p", "hello"];

    // Unmapped + sia ok -> default login, stale Bedrock env scrubbed.
    list("5", "ok");
    let out = run(t, true, hello);
    check(
        &out,
        "VIA=cswap-run ACCT=default BEDROCK=unset ROUTED=1",
        "unmapped-ok",
    );
    check(&out, "ARGS=-p hello", "unmapped-ok-args");

    // Unmapped + sia dead + qm disabled -> Bedrock (disabled never borrowed).
    list("5", "token_expired");
    check(
        &run(t, false, hello),
        "VIA=claude BEDROCK=1 ROUTED=1",
        "unmapped-dead",
    );

    // Unmapped + sia at 98% of its spend budget -> Bedrock before requests fail.
    list_spend("5", "ok", "98");
    check(
        &run(t, false, hello),
        "VIA=claude BEDROCK=1 ROUTED=1",
        "unmapped-spend-capped",
    );

    // Mapped subdir + qm fresh -> pinned account via bare run (disabled is
    // fine for its own dir).
    list("5", "ok");
    let out = run(&qmdir.join("darkfindr"), true, hello);
    check(
        &out,
        "VIA=cswap-run ACCT=default BEDROCK=unset ROUTED=1",
        "mapped-fresh",
    );

    // Mapped + qm dry -> explicit alternate account (sia).
    list("100", "ok");
    let out = run(&qmdir.join("darkfindr"), false, hello);
    check(
        &out,
        "VIA=cswap-run ACCT=sia@x.com BEDROCK=unset ROUTED=1",
        "mapped-dry-alt",
    );

    // Mapped + qm dry + sia usage UNKNOWN (null, mid-poll) -> Bedrock, never
    // a blind hand-off to an account whose health cannot be confirmed.
    fs::write(
        t.join("list.json"),
        list_json("100", "ok", "14").replace(
            r#""usage":{"spend":{"used":14.0,"limit":100.0,"pct":14}}"#,
            r#""usage":null"#,
        ),
    )
    .unwrap();
    check(
        &run(&qmdir.join("darkfindr"), false, hello),
        "VIA=claude BEDROCK=1 ROUTED=1",
        "mapped-dry-alt-unknown",
    );

    // Mapped + qm dry + sia dead -> Bedrock.
    list("100", "token_expired");
    check(
        &run(&qmdir.join("darkfindr"), false, hello),
        "VIA=claude BEDROCK=1 ROUTED=1",
        "mapped-all-dry",
    );

    // Bedrock model override rewrites t3code's explicit --model flag.
    fs::write(
        t.join("bedrock.env"),
        "export CLAUDE_CODE_USE_BEDROCK=1\nexport CCFLIP_BEDROCK_MODEL_OVERRIDE=\"claude-opus-5[1m]\"\n",
    )
    .unwrap();
    let out = run(
        &qmdir.join("darkfindr"),
        false,
        &["--model", "claude-fable-5[1m]", "-p", "hello"],
    );
    check(
        &out,
        "ARGS=--model claude-opus-5[1m] -p hello",
        "model-override",
    );

    // Mapped + everything dry + no bedrock.env -> still launches.
    fs::remove_file(t.join("bedrock.env")).unwrap();
    let out = run(&qmdir.join("darkfindr"), false, hello);
    check(
        &out,
        "VIA=cswap-run ACCT=default BEDROCK=unset ROUTED=1",
        "no-bedrock-env",
    );

    // Stale inherited PWD must be ignored (bash resets it to the real cwd):
    // real cwd is the mapped dir with a dry pinned account, so routing by it
    // yields the explicit alternate — routing by the stale PWD would not.
    list("100", "ok");
    let out = run_pwd(&qmdir.join("darkfindr"), t, false, hello);
    check(
        &out,
        "VIA=cswap-run ACCT=sia@x.com BEDROCK=unset ROUTED=1",
        "stale-pwd",
    );

    // Reserved words are shim-safe: only a SOLE `who` is intercepted…
    list("5", "ok");
    let out = run(t, false, &["who", "am", "i"]);
    check(&out, "ARGS=who am i", "who-with-args-passes-through");
    // …and `--` forces even a lone reserved word through to claude.
    let out = run(t, false, &["--", "who"]);
    check(&out, "ARGS=who", "double-dash-escape");

    // `ccflip bedrock [args]` forces the tier even with quota to spare.
    fs::write(t.join("bedrock.env"), "export CLAUDE_CODE_USE_BEDROCK=1\n").unwrap();
    list("5", "ok");
    let out = run(t, false, &["bedrock", "-p", "hello"]);
    check(&out, "VIA=claude BEDROCK=1 ROUTED=1", "forced-bedrock");
    check(&out, "ARGS=-p hello", "forced-bedrock-args");

    // Shim: invoked through a `claude` symlink it routes like ccflip…
    let shim_dir = t.join("shim");
    fs::create_dir_all(&shim_dir).unwrap();
    let shim = shim_dir.join("claude");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_ccflip"), &shim).unwrap();
    let run_shim = |routed: bool| {
        let mut cmd = Command::new(&shim);
        cmd.args(["-p", "hello"])
            .current_dir(t)
            .env("HOME", t)
            .env(
                "PATH",
                format!("{}:{}", t.display(), std::env::var("PATH").unwrap()),
            )
            .env("PWD", t)
            .env("OUT", t.join("out"))
            .env("LIST_FIXTURE", t.join("list.json"))
            .env("CCFLIP_MAPPINGS", t.join("mappings.json"))
            .env_remove("CCFLIP_BEDROCK_ENV")
            .env_remove("CLAUDE_CODE_USE_BEDROCK")
            .env_remove("CCFLIP_ROUTED");
        if routed {
            cmd.env("CCFLIP_ROUTED", "1");
        }
        assert!(cmd.status().unwrap().success());
        fs::read_to_string(t.join("out")).unwrap()
    };
    check(
        &run_shim(false),
        "VIA=cswap-run ACCT=default BEDROCK=unset ROUTED=1",
        "shim-routes",
    );
    // …but a marked descendant goes straight to the real claude: no loop.
    check(
        &run_shim(true),
        "VIA=claude BEDROCK=unset ROUTED=1",
        "shim-marked-passthrough",
    );

    // The shim also stands aside when the caller already chose a tier or an
    // account: CLAUDE_CONFIG_DIR (a `cswap run` session) and
    // CLAUDE_CODE_USE_BEDROCK (forced Bedrock) both bypass routing.
    for (var, value) in [
        ("CLAUDE_CONFIG_DIR", t.join("profile").display().to_string()),
        ("CLAUDE_CODE_USE_BEDROCK", "1".to_string()),
    ] {
        let out = Command::new(&shim)
            .args(["-p", "hello"])
            .current_dir(t)
            .env("HOME", t)
            .env(
                "PATH",
                format!("{}:{}", t.display(), std::env::var("PATH").unwrap()),
            )
            .env("PWD", t)
            .env("OUT", t.join("out"))
            .env("LIST_FIXTURE", t.join("list.json"))
            .env("CCFLIP_MAPPINGS", t.join("mappings.json"))
            .env_remove("CCFLIP_ROUTED")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CLAUDE_CODE_USE_BEDROCK")
            .env(var, value)
            .status()
            .unwrap();
        assert!(out.success());
        let out = fs::read_to_string(t.join("out")).unwrap();
        check(&out, "ARGS=-p hello", &format!("shim-bypass-{var}"));
        assert!(
            out.contains("VIA=claude"),
            "shim-bypass-{var}: must reach claude directly, got:\n{out}"
        );
    }
}
