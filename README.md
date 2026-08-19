# ccflip

Per-project Claude Code account routing with automatic fallback to Bedrock.
Thin glue around [claude-swap](https://github.com/realiti4/claude-swap) —
everything account-shaped is upstream; this repo only adds the Bedrock tier,
a pinned-session watcher, and a statusline chip.

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
- **`ccflip-watch`** (systemd unit) — upstream auto deliberately skips
  accounts owned by live pinned sessions, so this watcher rotates *pinned
  session profiles*: when a session's account exhausts a window, the best
  remaining account's credentials are written into that session's
  `CLAUDE_CONFIG_DIR`; Claude Code picks them up on its next token refresh,
  mid-session, no restart.

Bedrock mid-session is not possible by design: it needs env vars that cannot
be injected into a running process. Bedrock engages at (re)launch only.

## Install

```bash
./install.sh
```

Then on each host:

1. `/login` with each account in `claude`, run `cswap add` after each
   (no `/logout` in between — it revokes the token you're leaving).
   Headless host: `cswap export` on the laptop → `cswap import` there,
   or `claude setup-token` → `cswap add-token`.
2. `cswap map clement.bosle@quiet-mind.io ~/quiet-mind` — recursive,
   covers darkfindr and siblings.
3. `cp bedrock.env.example ~/.claude/bedrock.env`, keep the block for this
   host (dellpro: SSO profile; claude-host: instance role, no `AWS_PROFILE`).
4. Optional statusline (shows `[model] account-in-use`), in
   `~/.claude/settings.json`:

   ```json
   { "statusLine": { "type": "command", "command": "~/.local/bin/ccflip-statusline" },
     "awsAuthRefresh": "aws sso login --profile code-assist-admin" }
   ```

Launch with `ccflip` in terminals. t3code keeps launching plain `claude`.

## Seeing which account is in use

- `cswap status` / `cswap list` — active account + every window's usage.
- Statusline chip — `bedrock`, the pinned session slug, or the default email.
- `journalctl --user -u ccflip-watch -u cswap-auto` — every rotation, when, why.

## Usage tracking (tokscale)

`bunx tokscale@latest` scans transcripts. Default-profile usage lands in
`~/.claude/projects/…`; each pinned session writes under its own profile in
`~/.local/share/claude-swap/sessions/<n-email>/projects/…` — the path IS the
account, so per-account attribution is a matter of pointing tokscale at each
profile dir.

## ToS note

Two subscription accounts owned by the same person, each used through the
official client talking directly to Anthropic — no request proxying, no
pooling. claude-swap swaps credential files locally. Gray area is smaller
than proxy-based approaches, but Anthropic's consumer terms don't bless
account rotation; use at your own risk.
