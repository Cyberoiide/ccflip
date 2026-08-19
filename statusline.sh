#!/usr/bin/env bash
# Claude Code statusline: [model] account-in-use
# bedrock            -> Bedrock tier active
# N-email-slug       -> pinned cswap session (basename of CLAUDE_CONFIG_DIR)
# email              -> default login
input=$(cat)
model=$(jq -r '.model.display_name // empty' <<<"$input")
if [[ -n "${CLAUDE_CODE_USE_BEDROCK:-}" ]]; then
    acct="bedrock"
elif [[ -n "${CLAUDE_CONFIG_DIR:-}" ]]; then
    acct=$(basename "$CLAUDE_CONFIG_DIR")
else
    acct=$(jq -r '.oauthAccount.emailAddress // "?"' "$HOME/.claude.json" 2>/dev/null)
fi
printf '[%s] %s' "$model" "$acct"
