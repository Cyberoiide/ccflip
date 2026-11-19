#!/usr/bin/env bash
# Self-check for sso-guard's session list (SSO_GUARD_SESSIONS, policy.env),
# parallel keep-alive and `sso-guard login`. Hermetic: fake aws/systemctl in
# a throwaway HOME. Run: ./tests/sso_guard_test.sh
set -euo pipefail
cd "$(dirname "$0")/.."

T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
export HOME="$T/home" CALLS="$T/calls" XDG_STATE_HOME="$T/state" CCFLIP_POLICY="$T/policy.env"
unset AWS_PROFILE AWS_CONFIG_FILE SSO_GUARD_SESSIONS SSO_GUARD_BROWSER SSO_GUARD_MIN_MINUTES
mkdir -p "$HOME/.local/bin" "$HOME/.aws/sso/cache"
export PATH="$HOME/.local/bin:/usr/bin:/bin"

# sso-guard puts $HOME/.local/bin first on its PATH: no test reaches a real aws.
cat > "$HOME/.local/bin/systemctl" <<'EOF'
#!/usr/bin/env bash
case "$*" in *is-enabled*) exit "${TIMER_RC:-0}" ;; esac
EOF
cat > "$HOME/.local/bin/systemd-run" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
cat > "$HOME/.local/bin/aws" <<'EOF'
#!/usr/bin/env bash
echo "$*" >> "$CALLS"
case "$*" in
  *create-token*) # every IdC session is over, or the network is down
    [[ -n "${OFFLINE:-}" ]] && echo 'aws: [ERROR]: Could not connect to the endpoint URL: "https://x/token"' >&2
    exit 1 ;;
  "sso login --sso-session "*)
    s=${!#}; sleep 2
    # Like a browser: a child that inherits our fds and outlives the login.
    [[ -n "${LINGER:-}" ]] && { sleep 30 & }
    [[ " ${LOGIN_FAIL:-} " == *" $s "* ]] && exit 1
    f="$HOME/.aws/sso/cache/$(printf %s "$s" | sha1sum | cut -c1-40).json"
    jq --arg e "$(date -u -d '+1 hour' +%FT%TZ)" '.expiresAt = $e' "$f" > "$f.t" && mv "$f.t" "$f" ;;
esac
EOF
chmod +x "$HOME/.local/bin/"*

cat > "$HOME/.aws/config" <<'EOF'
[sso-session heka]
sso_start_url = https://heka.example/start
[profile h]
sso_session = heka

[sso-session strat]
sso_start_url = https://strat.example/start
[profile s]
sso_session = strat
EOF
expire() { # every session's token expired 5 min ago, refresh token dead
    for n in heka strat; do
        jq -n --arg u "https://$n.example/start" --arg e "$(date -u -d '-5 min' +%FT%TZ)" \
            '{startUrl: $u, region: "eu-west-1", accessToken: "old", expiresAt: $e,
              clientId: "c", clientSecret: "s", refreshToken: "r"}' \
            > "$HOME/.aws/sso/cache/$(printf %s "$n" | sha1sum | cut -c1-40).json"
    done
}
reset() { : > "$CALLS"; rm -rf "$XDG_STATE_HOME"; rm -f "$CCFLIP_POLICY"; expire
          unset SSO_GUARD_SESSIONS SSO_GUARD_BROWSER LOGIN_FAIL TIMER_RC LINGER OFFLINE; export DISPLAY=:0; }
logins() { grep -c "^sso login" "$CALLS" || true; }
fail() { echo "FAIL: $*"; echo "--- calls"; cat "$CALLS"; exit 1; }
timed() { local t0=$SECONDS r=0; "$@" || r=$?; elapsed=$(( SECONDS - t0 )); return "$r"; }

# Both sessions listed in policy.env: both re-logged, in parallel (2 s each).
reset; echo 'SSO_GUARD_SESSIONS="heka strat"' > "$CCFLIP_POLICY"
timed bin/sso-guard >/dev/null
[[ $(logins) == 2 ]] || fail "policy list: $(logins) logins"
(( elapsed < 4 )) || fail "guard not parallel: ${elapsed}s"

# Only heka listed: strat is unused, so it lapses instead of opening a tab.
reset; echo 'SSO_GUARD_SESSIONS=heka' > "$CCFLIP_POLICY"
out=$(bin/sso-guard)
[[ $(logins) == 1 ]] && grep -q "sso login --sso-session heka" "$CALLS" || fail "heka only"
grep -q "strat expired, unused, letting it lapse" <<<"$out" || fail "strat not gated: $out"

# The environment wins over policy.env; the old name still works.
reset; echo 'SSO_GUARD_SESSIONS=heka' > "$CCFLIP_POLICY"
SSO_GUARD_SESSIONS=strat bin/sso-guard >/dev/null
[[ $(logins) == 1 ]] && grep -q "sso-session strat" "$CALLS" || fail "env should win"
reset; SSO_GUARD_BROWSER=strat bin/sso-guard >/dev/null
[[ $(logins) == 1 ]] && grep -q "sso-session strat" "$CALLS" || fail "SSO_GUARD_BROWSER fallback"

# `sso-guard login`: every listed session at once, per-session result, rc.
reset; echo 'SSO_GUARD_SESSIONS="heka strat"' > "$CCFLIP_POLICY"; export LOGIN_FAIL=strat
rc=0; timed bin/sso-guard login > "$T/out" 2>&1 || rc=$?
(( elapsed < 4 )) || fail "login not parallel: ${elapsed}s"
[[ $rc == 1 && $(logins) == 2 ]] || fail "login rc=$rc logins=$(logins)"
grep -q "heka ok" "$T/out" && grep -q "strat FAILED" "$T/out" || fail "login report: $(cat "$T/out")"

# Explicit names; works with the timer off (sso-off); a fresh session is a no-op.
reset; export TIMER_RC=1
bin/sso-guard login strat >/dev/null 2>&1 || fail "login while sso-off"
bin/sso-guard login strat >/dev/null 2>&1
[[ $(logins) == 1 ]] || fail "second login of a fresh session: $(logins)"
bin/sso-guard login nope >/dev/null 2>&1 && fail "unknown session should fail"

# Offline (resume before Wi-Fi is back): no browser tab, retry in 1 min.
reset; echo 'SSO_GUARD_SESSIONS="heka strat"' > "$CCFLIP_POLICY"; export OFFLINE=1
out=$(bin/sso-guard)
[[ $(logins) == 0 ]] || fail "offline guard opened $(logins) tabs"
grep -q "strat: no network, retry in 1 min" <<<"$out" && grep -q "next check in 1 min" <<<"$out" || fail "offline: $out"
bin/sso-guard login strat >/dev/null 2>&1 && fail "offline login should fail"
[[ $(logins) == 0 ]] || fail "offline login opened a tab"
[[ ! -e "$XDG_STATE_HOME/sso-guard/strat.needs-you" ]] || fail "offline marked needs-you"

# A lingering browser must not hold a caller's stderr pipe open.
reset; export LINGER=1
timed bash -c 'bin/sso-guard login strat 2>&1 | cat >/dev/null'
(( elapsed < 6 )) || fail "browser held the pipe: ${elapsed}s"
pkill -f '^sleep 30$' || true

echo OK
