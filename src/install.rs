//! `ccflip install` — set this host up: claude-swap, systemd units.

use std::env;
use std::fs;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, ensure};

use crate::cswap;

pub fn run() -> Result<()> {
    if Command::new("cswap")
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_err()
    {
        println!("installing claude-swap via uv…");
        let ok = Command::new("uv")
            .args(["tool", "install", "claude-swap"])
            .status()
            .context("running `uv tool install claude-swap` (is uv installed?)")?
            .success();
        ensure!(ok, "`uv tool install claude-swap` failed");
    }

    let unit_dir = cswap::home().join(".config/systemd/user");
    fs::create_dir_all(&unit_dir)?;
    fs::write(
        unit_dir.join("cswap-auto.service"),
        include_str!("../systemd/cswap-auto.service"),
    )?;
    let exe = env::current_exe().context("resolving own binary path")?;
    let exe_str = exe.display().to_string();
    for (name, template) in [
        (
            "ccflip-watch.service",
            include_str!("../systemd/ccflip-watch.service.in"),
        ),
        (
            "sso-guard.service",
            include_str!("../systemd/sso-guard.service.in"),
        ),
    ] {
        fs::write(unit_dir.join(name), template.replace("{exe}", &exe_str))?;
    }
    fs::write(
        unit_dir.join("sso-guard.timer"),
        include_str!("../systemd/sso-guard.timer"),
    )?;

    systemctl(&["daemon-reload"])?;
    systemctl(&[
        "enable",
        "--now",
        "cswap-auto.service",
        "ccflip-watch.service",
    ])?;
    // SSO freshness is only meaningful where an AWS profile is configured;
    // instance-role hosts have no sso-session to guard.
    if cswap::home().join(".aws/config").exists() {
        systemctl(&["enable", "--now", "sso-guard.timer"])?;
    }

    println!(
        "\
ccflip installed. Remaining manual steps:
  1. Log into each Claude account once, then register it:  /login  ->  cswap add
     (do NOT /logout between accounts)
  2. Pin dirs to accounts:   cswap map <email> <dir>     (recursive)
  3. Bedrock tier:           create ~/.claude/bedrock.env, plain KEY=VALUE
     lines (template: bedrock.env.example in the repo / README)
  3b. Containment policy (optional): create ~/.config/ccflip/policy.env
     (template: policy.env.example — pinned-only accounts + forced default)
  3c. t3code: point its Claude binary path (Settings -> Claude) at this
     binary in every environment. For spawners that ignore that setting,
     shim `claude` instead — ccflip routes when invoked under that name:
       ln -sf {exe} ~/.local/bin/claude    (must precede the real binary)
  4. Optional statusline, in ~/.claude/settings.json:
       \"statusLine\": {{ \"type\": \"command\", \"command\": \"{exe} statusline\" }}
  5. Launch with `ccflip` in terminals; t3code needs nothing.",
        exe = exe_str
    );
    Ok(())
}

fn systemctl(args: &[&str]) -> Result<()> {
    let ok = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .with_context(|| format!("running systemctl --user {}", args.join(" ")))?
        .success();
    ensure!(ok, "systemctl --user {} failed", args.join(" "));
    Ok(())
}
