#!/usr/bin/env bash
# Statusline chip: which "account" this session runs on. Designed to be
# chained after other statusline scripts (prints one word, no newline).
# bedrock | <n-email-slug> (pinned session) | default-login email
cat >/dev/null # account comes from env + files, statusline JSON unused
if [[ -n "${CLAUDE_CODE_USE_BEDROCK:-}" ]]; then
    acct="bedrock"
elif [[ -n "${CLAUDE_CONFIG_DIR:-}" ]]; then
    acct=$(basename "$CLAUDE_CONFIG_DIR")
else
    acct=$(jq -r '.oauthAccount.emailAddress // "?"' "$HOME/.claude.json" 2>/dev/null)
fi
printf '⇄%s' "$acct"
