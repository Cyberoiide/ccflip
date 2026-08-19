# ccflip Rust rewrite — design

Date: 2026-08-19. Approved in chat before implementation.

## Goal

Replace the bash scripts (launcher, watcher, statusline, install.sh) with one
strictly typed Rust binary, installable and updatable from GitHub releases.
Behavior parity with the bash version; the only intended behavior change is
that `~/.claude/bedrock.env` is parsed as `KEY=VALUE` lines instead of being
sourced by a shell.

## Decisions

- **Scope: thin wrapper.** cswap stays upstream and owns everything
  account-shaped (credentials, swapping, mappings, `cswap auto`). ccflip
  shells out to `cswap list --json`, `cswap run`, `cswap switch`.
- **Distribution: GitHub releases.** Tag `v*` → CI builds a static
  `x86_64-unknown-linux-musl` binary → GitHub release. Install via
  `cargo binstall --git` (prebuilt) or `cargo install --git` (source).
  `ccflip update` self-updates from the latest release (`self_update` crate,
  ureq + rustls). `publish = false`: not on crates.io (niche, ToS-gray).
- **Strict typing everywhere.** All JSON goes through serde structs — no
  `serde_json::Value`. `usageStatus` is an enum (`Ok` / `NotOk` via
  `#[serde(other)]`). Unknown env threshold values warn and fall back to the
  default, never silently change routing.
- **One binary, five subcommands**: bare launcher (args pass through to
  claude; `--` escapes the four reserved words), `watch`, `statusline`,
  `install`, `update`.

## Layout

- `src/main.rs` — dispatch, launcher routing table, statusline, env helpers
- `src/cswap.rs` — typed cswap interface (`List`/`Account`/`Usage`), mappings
- `src/select.rs` — the ONE account-selection implementation (was two
  divergent jq programs); launcher `usable`/`pick_alt`/`any_enabled_usable`
  and watcher `pick_target`
- `src/launch.rs` — exec claude / `cswap run`, Bedrock env scrub + parse
- `src/watch.rs` — watcher loop (systemd unit runs `ccflip watch`)
- `src/install.rs` — claude-swap bootstrap, systemd units (embedded;
  ExecStart points at `current_exe()`)
- `src/update.rs` — self-update from GitHub releases

## Behavior parity notes

- Launcher threshold 99 (`CCFLIP_THRESHOLD`), watcher 95
  (`CCFLIP_WATCH_THRESHOLD`), interval 60 (`CCFLIP_WATCH_INTERVAL`),
  `CCFLIP_BEDROCK_ENV`, `CCFLIP_MAPPINGS` — all preserved.
- Fail open: cswap missing/broken/unparseable → exec plain `claude`.
- Launcher: spend-only and unknown-usage accounts count as usable; disabled
  accounts serve only their own mapped dirs. Watcher: spend-only is the
  last-resort target, unknown-usage never a target.
- Watcher log lines lose the hand-rolled timestamp — journald stamps them.

## Testing

- `src/select.rs` unit tests: direct port of `tests/pick_target_test.sh`
  plus launcher-side selection cases.
- `tests/routing.rs`: direct port of `tests/bedrock_fallback_test.sh` —
  fake `cswap`/`claude` scripts on PATH record what reached them; all six
  routing scenarios asserted end to end.
- CI (`ci.yml`): fmt, clippy `-D warnings`, tests on every push/PR.
