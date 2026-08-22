# ccflip

Per-project Claude Code account routing with automatic fallback to Bedrock.
Thin glue around [claude-swap](https://github.com/realiti4/claude-swap) —
everything account-shaped is upstream; this repo only adds the Bedrock tier,
a pinned-session watcher, and a statusline chip.

One static Rust binary, nine subcommands:

| Command | Does |
|---------|------|
| `ccflip [args…]` | route to the right account, exec `claude` (args pass through) |
| `ccflip bedrock [args…]` | force the Bedrock tier for this launch |
| `ccflip budget` | real dollar meters + this host's Bedrock consumption |
| `ccflip who` | account of every running claude session (PID, account, cwd) |
| `ccflip watch` | rotation, mid-session tier switching, policy guard, alerts (systemd unit) |
| `ccflip sso-guard` | renew AWS SSO before it expires (systemd timer) |
| `ccflip statusline` | print the account chip for the statusline |
| `ccflip install` | claude-swap + systemd units on this host |
| `ccflip update` | replace itself with the latest GitHub release |

ccflip is a drop-in claude binary, so it reserves no flags: `--help` and
`--version` reach claude, and the argument-free subcommands above are
recognized only when alone (`ccflip who am i` is a prompt). `bedrock` takes
claude args, so it is matched as the first word. `ccflip -- <word>` forces
any reserved word through.

## What it does

| Where | Tier order |
|-------|-----------|
| Mapped dir (`cswap map`, e.g. `~/quiet-mind`) | pinned account → any other enabled usable account → Bedrock |
| Anywhere else | default login → Bedrock |

Account containment: `cswap disable <email>` keeps an account out of
auto-rotation and out of ccflip's "other usable account" pool, while its
own mapped dirs still use it — e.g. a personal account that must never
serve work sessions. Spend-based accounts (enterprise/API billing) expose
no 5h/7d windows; they count as usable until their token dies, and are
never auto-rotated onto by the watcher.

Three moving parts:

- **`ccflip`** — launcher. Routes per the table above by reading
  `cswap list --json` and cswap's directory mappings; scrubs stale Bedrock
  env off subscription launches; drops to Bedrock (`~/.claude/bedrock.env`)
  only when no subscription account is usable for this directory.
- **`cswap auto`** (upstream, systemd unit) — rotates the *default* login
  before it hits a limit. Covers every plain `claude` launch, including
  t3code-spawned sessions — t3code needs no changes and sees nothing.
- **`ccflip watch`** (systemd unit) — upstream auto deliberately skips
  accounts owned by live pinned sessions, so this watcher rotates *pinned
  session profiles*: when a session's account exhausts a window, the best
  remaining account's credentials are written into that session's
  `CLAUDE_CONFIG_DIR`; Claude Code picks them up on its next token refresh,
  mid-session, no restart.

Bedrock mid-session is not possible by design: it needs env vars that cannot
be injected into a running process. Bedrock engages at (re)launch only.

## Install

```bash
# fast: prebuilt static binary from the latest GitHub release
cargo binstall --git https://github.com/Cyberoiide/ccflip ccflip

# or compile from source
cargo install --git https://github.com/Cyberoiide/ccflip

# then set the host up (claude-swap, systemd units)
ccflip install
```

Update later with `ccflip update` (set `GITHUB_TOKEN` while the repo is
private) or by re-running the install command.

Then on each host:

1. `/login` with each account in `claude`, run `cswap add` after each
   (no `/logout` in between — it revokes the token you're leaving).
   Headless host: `cswap export` on the laptop → `cswap import` there,
   or `claude setup-token` → `cswap add-token`.
2. `cswap map clement.bosle@quiet-mind.io ~/quiet-mind` — recursive,
   covers darkfindr and siblings.
3. `cp bedrock.env.example ~/.claude/bedrock.env`, keep the block for this
   host (dellpro: SSO profile; claude-host: instance role, no `AWS_PROFILE`).
   Plain `KEY=VALUE` / `export KEY=VALUE` lines only — ccflip reads it
   directly, it is not run through a shell.
4. Optional statusline (shows `[model] account-in-use`), in
   `~/.claude/settings.json`:

   ```json
   { "statusLine": { "type": "command", "command": "ccflip statusline" },
     "awsAuthRefresh": "aws sso login --profile code-assist-admin" }
   ```

Launch with `ccflip` in terminals. t3code keeps launching plain `claude`.

## t3code integration

t3code resolves its claude binary from a per-environment **binary path**
setting (Settings → Claude). Point it at `~/.cargo/bin/ccflip` on every
machine (the local app and each `npx t3 connect` host are separate
environments with separate settings). t3code spawns per session with the
workspace as cwd, so dir-pinning and the Bedrock tier apply; settings-wise
nothing else changes and running sessions are untouched.

t3code always passes an explicit `--model` flag, which beats every env var
and is not remapped by slot vars (`ANTHROPIC_DEFAULT_FABLE_MODEL` etc.).
On a host whose IAM role lacks that model, set
`CCFLIP_BEDROCK_MODEL_OVERRIDE="claude-opus-5[1m]"` in `bedrock.env` —
ccflip rewrites the flag when dropping to Bedrock.

## Forcing a tier

Policy routing is the default; explicit picks are always available:

- `ccflip bedrock [claude args]` — launch on the Bedrock tier now.
- `cswap run <alias|email> [-- claude args]` — launch pinned to a specific
  account, this terminal only. (From a fresh shell — a stale
  `CLAUDE_CODE_USE_BEDROCK` poisons cswap's validation probe.)

## Shimming `claude`

Some spawners can't be pointed at ccflip (t3code's remote server ignores the
desktop app's binary path). Symlink `claude` at ccflip somewhere earlier in
their PATH and every bare `claude` invocation routes — the binary is its own
shim, no wrapper script:

```bash
ln -sf ~/.cargo/bin/ccflip ~/.npm-global/bin/claude
```

It stands aside when the caller already chose: `CCFLIP_ROUTED` (ccflip's own
descendants — cswap re-resolves `claude`, and this is what stops the loop),
`CLAUDE_CONFIG_DIR` (a `cswap run` session), or `CLAUDE_CODE_USE_BEDROCK`
(forced Bedrock).

## Mid-session tier switching

Credential rotation covers subscription→subscription, but Bedrock needs env
vars a live process cannot receive. The conversation is the session id, not
the process: SDK harnesses (t3code, claude-mem) respawn a killed process on
the next message with the same session. So `ccflip watch` ends a
**harness-spawned** process whose account is exhausted with no rotation
target — its requests were failing anyway — and it revives on Bedrock; a
Bedrock process is ended once its subscription tier is usable again and its
transcript has been idle for `CCFLIP_SWITCHBACK_IDLE` (300s), reviving on the
account. Terminal sessions are never touched, nor are processes younger than
`CCFLIP_PROC_MIN_AGE` (120s). `CCFLIP_NO_TIER_SWITCH=1` disables the whole
mechanism; rotation, policy and alerts keep running.

A mapped dir's pinned profile is kept across the Bedrock tier, so history
resumes when a session moves between tiers.

## Seeing which account is in use

- `ccflip budget` — the default account's real dollar meters: the seat's
  included monthly pool (a `limit_dollars` bucket cswap doesn't parse — the
  meter that actually drains) plus the extra-usage overflow budget.
- `ccflip who` — every running claude session: PID, account
  (`bedrock` / pinned-session slug / default email), cwd. This is the way
  to see what a t3code session runs on — t3code's UI never shows the CLI
  statusline. (Reads spawn-time env: sessions that went Bedrock via a
  legacy settings.json env block, not via ccflip, show as subscription.)
- `cswap status` / `cswap list` — active account + every window's usage.
- Statusline chip — `bedrock`, the pinned session slug, or the default email.
- `journalctl --user -u ccflip-watch -u cswap-auto` — every rotation, when, why.

## Account containment policy

`~/.config/ccflip/policy.env` (see `policy.env.example`) declares which
account the default login must be and which accounts are pinned-only.
ccflip-watch enforces it every tick: a pinned-only account found as the
default login is switched away within a minute and re-disabled. This guards
against `cswap import` (token refresh transfers re-activate the replaced
slot and clear its disabled flag), manual slips, and anything else that
drifts the default onto a personal account.

## Spend budget (enterprise accounts)

Spend-based accounts expose `usage.spend.pct` instead of 5h/7d windows;
cswap polls it but never acts on it. ccflip does, at two lines:

- **`CCFLIP_SPEND_ALERT_PCT` (default 90)** — the watcher journals a
  `SPEND ALERT` and fires a desktop notification (when `notify-send`
  exists), once per breach, re-armed if spend drops back.
- **`CCFLIP_SPEND_THRESHOLD` (default 97)** — past this the account stops
  counting as usable: the launcher routes new sessions to the next tier
  (other account, then Bedrock) and the watcher stops rotating onto it,
  before requests start failing at the cap.

Set either in the environment or in `policy.env`.

## SSO freshness (hosts using an AWS profile)

`ccflip sso-guard` keeps the SSO session alive so the Bedrock tier never dies
with "Token is expired": it silently renews the access token via the SSO
refresh token when under `SSO_GUARD_MIN_MINUTES` (45) left, and opens the
one-click browser login only once the token is actually spent (a renewal that
did not happen yet is not a dead session — STS can answer from cached role
credentials). Scheduling is adaptive: each run arms a transient timer to wake
just before the earliest token crosses the line, with an hourly systemd timer
as the reboot-safe backstop. `ccflip install` enables it only where
`~/.aws/config` exists — instance-role hosts have nothing to guard. Claude
Code's own `awsAuthRefresh` setting stays as the in-session belt.

## Gotchas

- Never run `cswap run` from a shell still carrying `CLAUDE_CODE_USE_BEDROCK=1`:
  cswap's validity probe answers "bedrock" and it deletes the session profile
  with a misleading "log in and re-add" error. ccflip scrubs those vars; raw
  `cswap run` does not.
- Enterprise/spend accounts have no readable 5h/7d windows; cswap auto
  fails over off them if they're the default login. Keep the other accounts
  `cswap disable`d if the default must stay put.
- One OAuth token family cannot live on two hosts: `cswap export`/`import`
  clones the refresh-token family, and whichever host refreshes first
  rotates it and kills the other copy (`relogin_required` ping-pong). Give
  each host its own `/login` + `cswap add` per account; on headless hosts
  use `claude setup-token` + `cswap add-token` (no rotation). Export/import
  is bootstrap only.
- Bedrock model ids come in four forms (in-region `anthropic.X`, geo
  `eu./us./au.`, `global.`) and IAM grants are per inference-profile ARN.
  Use ALIAS forms (`claude-opus-5[1m]`) in bedrock.env and the override —
  Claude Code resolves the right prefix for `AWS_REGION` and handles the
  `[1m]` context variant. A 403 means the role lacks that profile ARN
  (not a bad name); force a prefix with `ANTHROPIC_BEDROCK_REGION_PREFIX`
  or pin a full id in `CCFLIP_BEDROCK_MODEL_OVERRIDE` if IAM demands it.

## Usage tracking (tokscale)

`bunx tokscale@latest` scans transcripts. Default-profile usage lands in
`~/.claude/projects/…`; each pinned session writes under its own profile in
`~/.local/share/claude-swap/sessions/<n-email>/projects/…` — the path IS the
account, so per-account attribution is a matter of pointing tokscale at each
profile dir.

## Development

```bash
cargo test          # unit tests + routing/watcher e2e against fakes
cargo clippy --all-targets -- -D warnings
```

Nothing in the suite touches real accounts, credentials, the installed
binary or the running systemd units: the integration tests put fake
`cswap`/`claude` scripts first on `PATH` and pin `HOME`, `XDG_DATA_HOME`,
`CCFLIP_MAPPINGS`, `CCFLIP_BEDROCK_ENV` and `CCFLIP_POLICY` into a tempdir.
The watcher runs one tick with `CCFLIP_WATCH_ONCE=1` and
`CCFLIP_NO_TIER_SWITCH=1`, so it never signals a live process.

To try a build against real state without disturbing the installed setup,
run the worktree binary directly — read-only commands (`ccflip who`,
`ccflip budget`, `ccflip statusline`) are safe as-is, and for the routing
path point the seams at copies:

```bash
CCFLIP_MAPPINGS=/tmp/try/mappings.json CCFLIP_BEDROCK_ENV=/tmp/try/bedrock.env \
  ./target/debug/ccflip -p hello
XDG_DATA_HOME=/tmp/try CCFLIP_POLICY=/tmp/try/policy.env \
  CCFLIP_WATCH_ONCE=1 CCFLIP_NO_TIER_SWITCH=1 ./target/debug/ccflip watch
```

The installed `ccflip-watch.service` is untouched by either.

Releasing: bump `version` in `Cargo.toml`, tag `vX.Y.Z`, push the tag —
CI builds the static musl binary and publishes the GitHub release that
`ccflip update` and cargo-binstall consume.

## ToS note

Two subscription accounts owned by the same person, each used through the
official client talking directly to Anthropic — no request proxying, no
pooling. claude-swap swaps credential files locally. Gray area is smaller
than proxy-based approaches, but Anthropic's consumer terms don't bless
account rotation; use at your own risk.
