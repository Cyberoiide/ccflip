# Changelog

## v1.0.0 — 2026-09-06

First tagged release. Per-project Claude Code account routing across two
subscription accounts with automatic Bedrock fallback, running on dellpro
(SSO profile) and claude-host (EC2 instance role).

### Routing

- Directory-based tiers: a mapped dir uses its pinned account, then any other
  enabled usable account, then Bedrock; everywhere else the default login,
  then Bedrock. Decisions read `cswap list --json` plus cswap's own mappings.
- `ccflip bedrock` forces the Bedrock tier; `cswap run <acct>` forces an
  account. The shim passes explicit choices through untouched.
- Stale Bedrock env (`CLAUDE_CODE_USE_BEDROCK` from an old shell or the
  pre-ccflip settings.json) is scrubbed off the subscription path — it
  otherwise silently sent subscription launches to Bedrock, and poisoned
  cswap's validity probe into deleting session profiles.
- `CCFLIP_BEDROCK_MODEL_OVERRIDE` rewrites an explicit `--model` flag on the
  Bedrock tier, for hosts whose IAM role lacks the requested model's
  inference-profile ARN (claude-host has no fable access → opus 5).

### Mid-session switching

- Bedrock needs spawn-time env a live process cannot receive, so the process
  is treated as disposable and the session id as the conversation: the watcher
  ends a harness-spawned (t3code) process whose account is exhausted — the
  session revives on Bedrock on the next message with full history — and ends
  an idle Bedrock process once a subscription tier is usable again.
- The pinned profile is kept across the Bedrock tier, so mapped-dir sessions
  can find their own history on both tiers.
- Terminal sessions, claude-mem observers, and processes younger than 2 min
  are never touched.

### Knowing when an account is dead

- **Limit flags:** the watcher scans freshly written transcripts for limit
  errors and flags the account unusable for 15 min. cswap's polled usage lags
  minutes behind a freshly hit limit (exhausted accounts poll on backoff), so
  this is what makes the switch take one tick instead of ten minutes.
- Selection fails closed on unknown usage; kill decisions fail open.
- Setup-token accounts cannot query the usage API (`usageStatus:
  "unavailable"` forever) but work fine for requests: such an account may
  serve its own pinned dir and keeps its sessions, while still never being
  elected as an alternate for other dirs.
- Spend-based (enterprise) accounts count as dry near their cap
  (`CCFLIP_SPEND_THRESHOLD`, default 97%) and are last-resort rotation
  targets.

### Containment

- `policy.env` declares the default account and any pinned-only accounts. The
  watcher enforces it every tick: a pinned-only account found as the default
  login is switched away and re-disabled within a minute. `cswap import`
  re-activates a replaced slot and clears its disabled flag, so nothing else
  re-checked intent.

### Observability

- `ccflip who` — every running claude session: PID, account, cwd.
- `ccflip budget` — the seat's included monthly pool (a `limit_dollars` bucket
  cswap doesn't parse — the meter that actually drains), the extra-usage
  overflow, and this host's own Bedrock spend in dollars, priced per model
  from local transcripts and cached per file by mtime+size (sub-second warm).
- Spend alerts at 90% (journal + desktop notification), once per breach.
- Statusline chip: `bedrock` / pinned-session slug / default email.

### AWS SSO

- `sso-guard` + timer keeps every `sso-session` in `~/.aws/config` fresh:
  silent refresh-token renewal, browser login only when the Identity Center
  session itself died. Scheduling is adaptive — each run arms a transient
  timer for ~1 min before the earliest token crosses the threshold; the static
  timer is an hourly reboot-safe backstop.

### Known issues

- One OAuth token family cannot live on two hosts: `cswap export`/`import`
  clones the refresh-token family and whichever host refreshes first kills the
  other copy. Give each host its own login, or a `claude setup-token`
  (long-lived, no rotation) on headless hosts.
- Replacing an account (`add-token`, `import`) silently prunes its directory
  mappings and resets its alias and disabled flag — re-apply all three.
- t3code's remote server ignores the desktop app's binary-path setting;
  routing there depends on the `claude` shim being first in the server's spawn
  PATH.
- The Bedrock dollar figure is an API-rate estimate (Bedrock partner pricing
  differs); Cost Explorer remains billing truth.
