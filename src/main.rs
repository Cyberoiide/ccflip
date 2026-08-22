//! ccflip — launch Claude Code on the right "account" for this directory.
//!
//! Mapped dir (cswap map):  its pinned account -> any other enabled usable
//!                          account -> Bedrock.
//! Anywhere else:           the default login (cswap auto guards it) ->
//!                          Bedrock only when no subscription account works.
//!
//! ~/.claude/bedrock.env holds the per-host Bedrock config; `ccflip bedrock`
//! forces that tier. Proactive rotation of the default login is the
//! cswap-auto daemon's job — this launcher only routes.
//!
//! Also serves as t3code's claude binary (settings -> Claude -> binary path)
//! and, symlinked as `claude`, as the shim for spawners that ignore that
//! setting. GUI spawns may carry a minimal PATH, hence the PATH append.
//! Because it is a drop-in claude binary, ccflip reserves NO flags and
//! intercepts its argument-free subcommands only when they are the sole
//! argument — everything else passes through.

mod budget;
mod cswap;
mod install;
mod launch;
mod policy;
mod proc;
mod select;
mod sso;
mod update;
mod watch;

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

const USAGE: &str = "\
usage: ccflip [--] [claude args…]   route to the right account and launch
       ccflip bedrock [args…]       force the Bedrock tier for this launch
       ccflip who                   account of every running claude session
       ccflip budget                dollar meters + this host's Bedrock use
       ccflip watch                 pinned-session rotation, policy, alerts
       ccflip sso-guard             renew AWS SSO before it expires
       ccflip statusline            print the account chip (statusline hook)
       ccflip install               systemd units + claude-swap on this host
       ccflip update                self-update from the latest GitHub release

Reserved words above are ccflip's; everything else passes through to claude.
Use `ccflip -- who` to pass a reserved word through.";

fn main() {
    ensure_local_bin_on_path();
    // args_os: a non-UTF8 argument must pass through to claude, not panic us.
    let mut args: Vec<OsString> = env::args_os().skip(1).collect();

    // Invoked as `claude` (symlink shim, for spawners whose settings cannot
    // reach ccflip): route only spawns that expressed no preference. The
    // caller already chose when CCFLIP_ROUTED (our own descendants),
    // CLAUDE_CONFIG_DIR (an explicit `cswap run` session) or
    // CLAUDE_CODE_USE_BEDROCK (forced Bedrock) is set.
    if invoked_as_claude() {
        let chose_already = [
            "CCFLIP_ROUTED",
            "CLAUDE_CONFIG_DIR",
            "CLAUDE_CODE_USE_BEDROCK",
        ]
        .iter()
        .any(|v| env::var_os(v).is_some_and(|x| !x.is_empty()));
        if chose_already {
            launch::exec_claude(&args);
        }
        route(&args)
    }

    // `bedrock` takes claude args, so it is matched as the first word; the
    // argument-free subcommands are matched only alone, which keeps
    // `ccflip who is on call` a prompt. `ccflip -- <word>` escapes either.
    if args.first().is_some_and(|a| a == "bedrock") {
        launch::force_bedrock(&args[1..])
    }
    if args.len() == 1 {
        match args[0].to_str() {
            Some("who") => return who(),
            Some("budget") => return budget::run(),
            Some("watch") => watch::run(),
            Some("sso-guard") => std::process::exit(sso::run()),
            Some("statusline") => return statusline(),
            Some("install") => return exit_on_err(install::run()),
            Some("update") => return exit_on_err(update::run()),
            Some("help") => return println!("{USAGE}"),
            // No flag interception (--help/--version go to claude): probes
            // against the configured claude binary must see claude's answers.
            _ => {}
        }
    }
    if args.first().is_some_and(|a| a == "--") {
        args.remove(0);
    }
    route(&args)
}

/// argv[0] shim check: a `claude` symlink pointing at this binary makes every
/// plain `claude` launch route, no wrapper script needed. `claude_program()`
/// already skips PATH entries that resolve back to us, so the real binary is
/// still reachable.
fn invoked_as_claude() -> bool {
    env::args_os()
        .next()
        .map(PathBuf::from)
        .as_deref()
        .and_then(Path::file_name)
        .is_some_and(|n| n == "claude")
}

/// cswap and claude live in ~/.local/bin, which GUI spawns (t3code) and the
/// systemd user manager may leave off PATH. APPEND, never prepend: the
/// caller's explicit PATH order (wrappers, pinned builds, test fakes) wins.
fn ensure_local_bin_on_path() {
    let local_bin = cswap::home().join(".local/bin");
    let path = env::var_os("PATH").unwrap_or_default();
    if env::split_paths(&path).any(|p| p == local_bin) {
        return;
    }
    if let Ok(new) = env::join_paths(env::split_paths(&path).chain(std::iter::once(local_bin))) {
        // SAFETY: top of main — no other threads yet.
        unsafe { env::set_var("PATH", new) };
    }
}

fn exit_on_err(result: anyhow::Result<()>) {
    if let Err(e) = result {
        eprintln!("ccflip: {e:#}");
        std::process::exit(1);
    }
}

/// The directory to route by. bash silently resets an inherited PWD that no
/// longer names the real cwd; match that — a stale PWD (t3code spawns with a
/// cwd option that does not rewrite PWD) must not route to the wrong account.
fn routing_dir() -> String {
    if let Ok(pwd) = env::var("PWD")
        && !pwd.is_empty()
        && let (Ok(a), Ok(b)) = (std::fs::metadata(&pwd), std::fs::metadata("."))
    {
        use std::os::unix::fs::MetadataExt;
        if a.dev() == b.dev() && a.ino() == b.ino() {
            return pwd; // logical path (keeps symlinked spellings, like bash)
        }
    }
    env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The routing table from the module doc. Every arm execs and never returns.
fn route(args: &[OsString]) -> ! {
    let policy = policy::read();
    // The launcher acts only on truly-dry windows, and treats a spend-billed
    // account as dry near its cap so routing falls to the next tier before
    // requests start failing.
    let threshold = policy.knob("CCFLIP_THRESHOLD", 99.0);
    let spend_threshold = policy.knob("CCFLIP_SPEND_THRESHOLD", 97.0);
    // cswap missing or broken: fail open, plain claude.
    let Some(list) = cswap::list() else {
        launch::exec_claude(args)
    };

    if let Some(mapped) = cswap::mapped_email(&routing_dir()) {
        if list
            .by_email(&mapped)
            .is_some_and(|a| select::usable(a, threshold, spend_threshold))
        {
            launch::go_subscription(None, args); // bare run resolves the mapping itself
        }
        // Pinned account is dry: any other enabled electable account may
        // serve this dir (disabled accounts are dir-pinned only — never
        // borrowed; unknown-health accounts are never a blind hand-off).
        if let Some(alt) = select::pick_alt(&list, &mapped, threshold, spend_threshold) {
            eprintln!("ccflip: {mapped} exhausted -> {}", alt.email);
            launch::go_subscription(Some(&alt.email), args);
        }
        launch::go_bedrock(
            "every usable subscription account exhausted",
            args,
            Some(&mapped),
        );
    }

    // Unmapped dir: the default login, unless it is dead/dry. If another
    // enabled account could replace it, launch anyway — the cswap-auto
    // daemon moves the default login, not us.
    let active_usable = list
        .active_account()
        .is_some_and(|a| select::usable(a, threshold, spend_threshold));
    if active_usable || select::any_enabled_electable(&list, threshold, spend_threshold) {
        launch::go_subscription(None, args);
    }
    launch::go_bedrock("no usable subscription account", args, None)
}

/// The account a config dir is logged into, by its live login rather than its
/// (possibly stale) directory name; falls back to the name for a profile that
/// has never been used.
fn config_dir_account(dir: &str) -> String {
    cswap::profile_email(Path::new(dir)).unwrap_or_else(|| {
        Path::new(dir)
            .file_name()
            .map_or_else(|| "?".to_string(), |n| n.to_string_lossy().into_owned())
    })
}

fn default_email() -> String {
    cswap::profile_email(&cswap::home()).unwrap_or_else(|| "?".to_string())
}

/// Statusline chip: which "account" this session runs on. Designed to be
/// chained after other statusline scripts (prints one word, no newline).
/// bedrock | the pinned profile's account | default-login email
fn statusline() {
    // Account comes from env + files; the statusline JSON on stdin is unused.
    let _ = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
    let account = if env::var("CLAUDE_CODE_USE_BEDROCK").is_ok_and(|v| !v.is_empty()) {
        "bedrock".to_string()
    } else if let Some(dir) = env::var("CLAUDE_CONFIG_DIR").ok().filter(|d| !d.is_empty()) {
        config_dir_account(&dir)
    } else {
        default_email()
    };
    print!("⇄{account}");
}

/// `ccflip who` — which account is every running claude session on?
fn who() {
    let default_acct = default_email();
    println!("{:<9} {:<45} CWD", "PID", "ACCOUNT");
    for p in proc::claude_procs() {
        let account = if p.bedrock {
            "bedrock".to_string()
        } else {
            match &p.config_dir {
                Some(dir) => config_dir_account(dir),
                None => default_acct.clone(),
            }
        };
        println!("{:<9} {account:<45} {}", p.pid, p.cwd);
    }
}

/// Env override or default; a value that does not parse warns and defaults —
/// never silently changes routing.
pub fn env_parsed<T: std::str::FromStr + std::fmt::Display>(var: &str, default: T) -> T {
    match env::var(var) {
        Err(_) => default,
        Ok(raw) => raw.parse().unwrap_or_else(|_| {
            eprintln!("ccflip: {var}={raw} is not a number, using {default}");
            default
        }),
    }
}
