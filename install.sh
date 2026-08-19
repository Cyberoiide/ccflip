#!/usr/bin/env bash
# Install ccflip on this host: claude-swap, launcher, watcher, systemd units.
set -euo pipefail
cd "$(dirname "$0")"

command -v cswap >/dev/null 2>&1 || uv tool install claude-swap

mkdir -p "$HOME/.local/bin" "$HOME/.config/systemd/user"
ln -sf "$PWD/bin/ccflip" "$HOME/.local/bin/ccflip"
ln -sf "$PWD/bin/ccflip-watch" "$HOME/.local/bin/ccflip-watch"
ln -sf "$PWD/statusline.sh" "$HOME/.local/bin/ccflip-statusline"
chmod +x bin/ccflip bin/ccflip-watch statusline.sh

cp systemd/cswap-auto.service systemd/ccflip-watch.service "$HOME/.config/systemd/user/"
systemctl --user daemon-reload
systemctl --user enable --now cswap-auto.service ccflip-watch.service

cat <<'EOF'
ccflip installed. Remaining manual steps:
  1. Log into each Claude account once, then register it:  /login  ->  cswap add
     (do NOT /logout between accounts)
  2. Pin dirs to accounts:   cswap map <email> <dir>     (recursive)
  3. Bedrock tier:           cp bedrock.env.example ~/.claude/bedrock.env  (edit per host)
  4. Launch with `ccflip` in terminals; t3code needs nothing.
EOF
