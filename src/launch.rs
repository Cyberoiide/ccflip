//! Process launching: exec claude / cswap run, Bedrock env handling.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

use crate::cswap;

/// A stale Bedrock env (shell that sourced bedrock.env, process started
/// under old always-on settings) must not leak through — cswap scrubs
/// auth-token overrides but not the Bedrock switches.
const BEDROCK_VARS: [&str; 4] = [
    "CLAUDE_CODE_USE_BEDROCK",
    "AWS_BEARER_TOKEN_BEDROCK",
    "ANTHROPIC_BEDROCK_BASE_URL",
    "ANTHROPIC_MODEL",
];

fn exec_failed(program: &str, err: io::Error) -> ! {
    eprintln!("ccflip: exec {program} failed: {err}");
    std::process::exit(127);
}

/// Resolve `claude` on PATH, never ourselves: a `claude` symlink pointing at
/// ccflip (the shim setup) must not self-exec forever.
fn claude_program() -> PathBuf {
    let me = env::current_exe()
        .ok()
        .and_then(|p| fs::canonicalize(p).ok());
    let mut saw_self = false;
    for dir in env::split_paths(&env::var_os("PATH").unwrap_or_default()) {
        let candidate = dir.join("claude");
        let Ok(real) = fs::canonicalize(&candidate) else {
            continue;
        };
        if me.as_ref() == Some(&real) {
            saw_self = true;
            continue;
        }
        return candidate;
    }
    if saw_self {
        eprintln!("ccflip: every `claude` on PATH is ccflip itself — refusing to exec-loop");
        std::process::exit(127);
    }
    PathBuf::from("claude") // let exec produce its own not-found error
}

pub fn exec_claude(args: &[OsString]) -> ! {
    let err = Command::new(claude_program())
        .args(args)
        .env("CCFLIP_ROUTED", "1")
        .exec();
    exec_failed("claude", err)
}

/// `cswap run [account] -- args`; bare run resolves the dir mapping itself.
pub fn go_subscription(account: Option<&str>, args: &[OsString]) -> ! {
    let mut cmd = Command::new("cswap");
    cmd.arg("run");
    if let Some(acct) = account {
        cmd.arg(acct);
    }
    cmd.arg("--").args(args);
    for var in BEDROCK_VARS {
        cmd.env_remove(var);
    }
    // Descendants that re-resolve `claude` (cswap does) must get the real
    // binary, not a `claude` symlink pointing back at us.
    cmd.env("CCFLIP_ROUTED", "1");
    let err = cmd.exec();
    exec_failed("cswap", err)
}

pub fn bedrock_env_path() -> PathBuf {
    env::var_os("CCFLIP_BEDROCK_ENV")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| cswap::home().join(".claude/bedrock.env"))
}

/// Policy fallback to the Bedrock tier. No bedrock.env = no Bedrock on this
/// host — launch on subscription anyway (the session pauses at the limit).
/// `mapped_email` keeps a pinned dir's profile so its history survives the
/// tier change.
pub fn go_bedrock(reason: &str, args: &[OsString], mapped_email: Option<&str>) -> ! {
    let path = bedrock_env_path();
    match fs::read_to_string(&path) {
        Ok(text) => {
            eprintln!("ccflip: {reason} -> Bedrock");
            exec_bedrock(&text, args, mapped_email)
        }
        Err(_) => {
            eprintln!(
                "ccflip: {reason} and no {} — launching anyway",
                path.display()
            );
            go_subscription(None, args)
        }
    }
}

/// `ccflip bedrock [args]` — the user asked for this tier explicitly, so a
/// missing bedrock.env is an error, not a fallback.
pub fn force_bedrock(args: &[OsString]) -> ! {
    let path = bedrock_env_path();
    match fs::read_to_string(&path) {
        Ok(text) => exec_bedrock(&text, args, None),
        Err(_) => {
            eprintln!("ccflip: no {} on this host", path.display());
            std::process::exit(1);
        }
    }
}

fn exec_bedrock(env_file: &str, args: &[OsString], mapped_email: Option<&str>) -> ! {
    let vars = parse_env_file(env_file);
    let value = |key: &str| {
        vars.iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .or_else(|| env::var(key).ok())
            .filter(|v| !v.is_empty())
    };
    // An explicit --model flag (t3code always sends one) beats every env var,
    // and slot vars like ANTHROPIC_DEFAULT_FABLE_MODEL do not remap it. Hosts
    // whose IAM role lacks the requested model set CCFLIP_BEDROCK_MODEL_OVERRIDE
    // in bedrock.env; we rewrite the flag (and --fallback-model — same IAM gap).
    let over = value("CCFLIP_BEDROCK_MODEL_OVERRIDE");
    let args = match &over {
        Some(model) => rewrite_model(args, model),
        None => args.to_vec(),
    };

    let mut cmd = Command::new(claude_program());
    cmd.args(&args).env("CCFLIP_ROUTED", "1");
    for (key, value) in &vars {
        cmd.env(key, value);
    }
    // Override without ANTHROPIC_MODEL in bedrock.env: a launch with no
    // --model flag would still request a model the role lacks.
    if let Some(model) = &over
        && value("ANTHROPIC_MODEL").is_none()
    {
        cmd.env("ANTHROPIC_MODEL", model);
    }
    // A mapped dir's conversation history lives in its pinned profile — keep
    // that profile so sessions resume across tiers (Bedrock SigV4 auth
    // ignores the profile's subscription credentials).
    if let Some(email) = mapped_email
        && let Some(profile) = cswap::profile_for_email(email)
    {
        cmd.env("CLAUDE_CONFIG_DIR", profile);
    }
    let err = cmd.exec();
    exec_failed("claude", err)
}

/// Replace the value of every `--model X` / `--model=X` (and the
/// --fallback-model forms) with `model`. Stops at `--`: past the
/// end-of-options separator everything is prompt text, not flags.
fn rewrite_model(args: &[OsString], model: &str) -> Vec<OsString> {
    let mut out = Vec::with_capacity(args.len());
    let mut expect_value = false;
    let mut past_options = false;
    for arg in args {
        if past_options {
            out.push(arg.clone());
        } else if expect_value {
            out.push(model.into());
            expect_value = false;
        } else {
            match arg.to_str() {
                Some("--") => {
                    past_options = true;
                    out.push(arg.clone());
                }
                Some("--model" | "--fallback-model") => {
                    out.push(arg.clone());
                    expect_value = true;
                }
                Some(s) if s.starts_with("--model=") => out.push(format!("--model={model}").into()),
                Some(s) if s.starts_with("--fallback-model=") => {
                    out.push(format!("--fallback-model={model}").into())
                }
                _ => out.push(arg.clone()),
            }
        }
    }
    out
}

/// `KEY=VALUE` lines, optional `export ` prefix. Values follow shell
/// assignment shape: a quoted value is taken whole (quotes stripped), an
/// unquoted value ends at the first whitespace — so trailing `# comments`
/// fall off, like sourcing would. This is NOT a shell: no expansion, no
/// command substitution. Anything unparseable is warned about loudly.
pub(crate) fn parse_env_file(text: &str) -> Vec<(String, String)> {
    let mut vars = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let parsed = line.split_once('=').and_then(|(key, value)| {
            let key = key.trim();
            let ident =
                !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            ident.then(|| (key.to_string(), parse_value(value.trim(), line)))
        });
        match parsed {
            Some(kv) => vars.push(kv),
            None => eprintln!("ccflip: ignoring unparseable env-file line: {line}"),
        }
    }
    vars
}

fn parse_value(raw: &str, line: &str) -> String {
    for q in ['"', '\''] {
        if let Some(inner) = raw.strip_prefix(q).and_then(|s| s.strip_suffix(q)) {
            return inner.to_string();
        }
    }
    let (value, rest) = raw.split_once(char::is_whitespace).unwrap_or((raw, ""));
    let rest = rest.trim_start();
    if !rest.is_empty() && !rest.starts_with('#') {
        eprintln!("ccflip: ignoring trailing text after value in env line: {line}");
    }
    value.to_string()
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::{parse_env_file, rewrite_model};

    #[test]
    fn parses_bedrock_env_example() {
        let text = "\
# comment\n\
export CLAUDE_CODE_USE_BEDROCK=1\n\
export AWS_PROFILE=code-assist-admin # inline comment\n\
AWS_REGION=\"eu-west-1\"\n\
QUOTED='x y'\n\
EMPTY=\n\
\n\
this is not a var\n";
        assert_eq!(
            parse_env_file(text),
            vec![
                ("CLAUDE_CODE_USE_BEDROCK".into(), "1".into()),
                ("AWS_PROFILE".into(), "code-assist-admin".into()),
                ("AWS_REGION".into(), "eu-west-1".into()),
                ("QUOTED".into(), "x y".into()),
                ("EMPTY".into(), String::new()),
            ]
        );
    }

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn rewrites_model_flags_but_not_past_double_dash() {
        let input = args(&[
            "--model",
            "claude-fable-5[1m]",
            "--fallback-model=x",
            "--",
            "--model",
            "prompt-word",
        ]);
        assert_eq!(
            rewrite_model(&input, "claude-opus-5[1m]"),
            args(&[
                "--model",
                "claude-opus-5[1m]",
                "--fallback-model=claude-opus-5[1m]",
                "--",
                "--model",
                "prompt-word",
            ])
        );
    }
}
