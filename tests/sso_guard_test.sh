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
  *create-token*) # every IdC session is over (unless REFRESH_IN), or the network is down
    [[ -n "${REFRESH_IN:-}" ]] && { echo "{\"accessToken\":\"new\",\"expiresIn\":$REFRESH_IN}"; exit 0; }
    [[ -n "${OFFLINE:-}" ]] && echo 'aws: [ERROR]: Could not connect to the endpoint URL: "https://x/token"' >&2
    exit 1 ;;
  "sso login --sso-session "*)
    s=${!#}; sleep "${SLOW:-2}"
    # Like a browser: a child that inherits our fds and outlives the login.
    [[ -n "${LINGER:-}" ]] && { sleep 30 & }
    [[ " ${LOGIN_FAIL:-} " == *" $s "* ]] && exit 1
    f="$HOME/.aws/sso/cache/$(printf %s "$s" | sha1sum | cut -c1-40).json"
    jq --arg e "$(date -u -d '+1 hour' +%FT%TZ)" '.expiresAt = $e' "$f" > "$f.t" && mv "$f.t" "$f" ;;
  "sts get-caller-identity --profile "*) # mint 1h role creds where botocore caches them
    jq -n --arg e "$(date -u -d '+1 hour' +%FT%TZ)" '{Credentials: {Expiration: $e}}' > "$(cat "$HOME/key-${!#}")" ;;
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
sso_account_id = 222
sso_role_name = Admin
[profile s2]
sso_session = strat
sso_account_id = 333
sso_role_name = Admin
EOF
mkdir -p "$HOME/.aws/cli/cache"
for p in s:222 s2:333; do # where botocore caches each profile's role creds
    printf '%s/.aws/cli/cache/%s.json' "$HOME" "$(printf '{"accountId":"%s","roleName":"Admin","sessionName":"strat"}' \
        "${p#*:}" | sha1sum | cut -c1-40)" > "$HOME/key-${p%:*}"
done
expire() { # every session's token expired 5 min ago, refresh token dead
    for n in heka strat; do
        jq -n --arg u "https://$n.example/start" --arg e "$(date -u -d '-5 min' +%FT%TZ)" \
            '{startUrl: $u, region: "eu-west-1", accessToken: "old", expiresAt: $e,
              clientId: "c", clientSecret: "s", refreshToken: "r"}' \
            > "$HOME/.aws/sso/cache/$(printf %s "$n" | sha1sum | cut -c1-40).json"
    done
}
reset() { : > "$CALLS"; rm -rf "$XDG_STATE_HOME"; rm -f "$CCFLIP_POLICY"; expire
          unset SSO_GUARD_SESSIONS SSO_GUARD_BROWSER LOGIN_FAIL TIMER_RC LINGER OFFLINE SLOW REFRESH_IN
          rm -f "$HOME"/.aws/cli/cache/*; export DISPLAY=:0; }
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

# The timer's login holds the lock (its tab unseen): `login` replaces it at
# once instead of waiting silently for the 10 min timeout.
reset; echo 'SSO_GUARD_SESSIONS=strat' > "$CCFLIP_POLICY"
SLOW=30 bin/sso-guard >/dev/null 2>&1 & g=$!
until grep -q "^sso login" "$CALLS"; do sleep 0.1; done
timed bin/sso-guard login strat > "$T/out" 2>&1 || fail "takeover login: $(cat "$T/out")"
(( elapsed < 6 )) || fail "login waited on the timer's tab: ${elapsed}s"
grep -q "strat: replacing the login already open" "$T/out" || fail "takeover msg: $(cat "$T/out")"
wait "$g" || true

# Killing `login` (Ctrl-C) takes its jobs down: no orphan opens a tab later.
reset; SLOW=30 bin/sso-guard login strat >/dev/null 2>&1 & l=$!
until grep -q "^sso login" "$CALLS"; do sleep 0.1; done
kill -TERM "$l"; wait "$l" || true; sleep 0.5
! pgrep -f "sso-guard login strat$|aws sso login --sso-session strat$" >/dev/null || fail "login left orphans"
pkill -f '^sleep 30$' || true

# Session ends in 10 min: no bank yet, wake 2 min before the end.
reset; echo 'SSO_GUARD_SESSIONS=strat' > "$CCFLIP_POLICY"; export REFRESH_IN=600
out=$(bin/sso-guard)
grep -q "strat session ends" <<<"$out" && grep -qE "next check in [78] min" <<<"$out" || fail "bank wake: $out"
grep -q "^sts" "$CALLS" && fail "banked too early"

# 2 min left: the used profile's creds are re-minted to outlive the session,
# keeping the mtime of its last real use; the unused one is left alone.
reset; echo 'SSO_GUARD_SESSIONS=strat' > "$CCFLIP_POLICY"; export REFRESH_IN=100
jq -n --arg e "$(date -u -d '+50 min' +%FT%TZ)" '{Credentials: {Expiration: $e}}' > "$(cat "$HOME/key-s")"
touch -d '-10 min' "$(cat "$HOME/key-s")"
jq -n --arg e "$(date -u -d '-1 hour' +%FT%TZ)" '{Credentials: {Expiration: $e}}' > "$(cat "$HOME/key-s2")"
touch -d '-2 hour' "$(cat "$HOME/key-s2")"
m=$(stat -c %Y "$(cat "$HOME/key-s")")
out=$(bin/sso-guard)
grep -q "strat: s creds banked until" <<<"$out" || fail "not banked: $out"
(( $(date -d "$(jq -r .Credentials.Expiration "$(cat "$HOME/key-s")")" +%s) > $(date +%s) + 3000 )) || fail "bank not fresh"
[[ $(stat -c %Y "$(cat "$HOME/key-s")") == "$m" ]] || fail "bank faked a use"
[[ $(grep -c "^sts" "$CALLS") == 1 && $(logins) == 0 ]] || fail "bank: wrong calls"
! ls "$HOME"/.aws/cli/cache/*.old >/dev/null 2>&1 || fail "bank left a backup"

# push/receive: a headless host gets the access token under its own session
# name (same start URL), never the refresh token or client secret; an older
# token never overwrites a newer one; 2 min before the end it banks its own
# used profiles' creds.
R="$T/remote"; mkdir -p "$R/.local/bin" "$R/.aws/cli/cache"
cp "$HOME/.local/bin/aws" "$HOME/.local/bin/systemctl" "$R/.local/bin/"
ln -s "$PWD/bin/sso-guard" "$R/.local/bin/sso-guard"
cat > "$HOME/.local/bin/ssh" <<EOF
#!/usr/bin/env bash
echo "ssh \${@: -2:1}" >> "\$CALLS"
HOME="$R" bash -c "\${!#}"
EOF
chmod +x "$HOME/.local/bin/ssh"
cat > "$R/.aws/config" <<'EOF'
[sso-session strat-x]
sso_start_url = https://strat.example/start
[profile r]
sso_session = strat-x
sso_account_id = 444
sso_role_name = Admin
EOF
RT="$R/.aws/sso/cache/$(printf strat-x | sha1sum | cut -c1-40).json"
RC="$R/.aws/cli/cache/$(printf '{"accountId":"444","roleName":"Admin","sessionName":"strat-x"}' | sha1sum | cut -c1-40).json"
echo "$RC" > "$R/key-r"
LT="$HOME/.aws/sso/cache/$(printf strat | sha1sum | cut -c1-40).json"
fresh() { jq --arg e "$(date -u -d "$1" +%FT%TZ)" '.expiresAt = $e' "$LT" > "$LT.t" && mv "$LT.t" "$LT"; }

reset; bin/sso-guard push || fail "push with no hosts"
[[ ! -s "$CALLS" ]] || fail "push with no hosts called out"
reset; export SSO_GUARD_PUSH=box; fresh '+50 min'
out=$(bin/sso-guard push) || fail "push: $out"
grep -q "strat-x token until" <<<"$out" || fail "push not received: $out"
[[ $(jq -r .accessToken "$RT") == old ]] || fail "token not filed under the host's session"
[[ $(jq -r '.refreshToken // .clientSecret // .clientId // "none"' "$RT") == none ]] || fail "secrets pushed"
[[ $(stat -c %a "$RT") == 600 ]] || fail "pushed token mode $(stat -c %a "$RT")"
fresh '+20 min'; bin/sso-guard push >/dev/null
(( $(date -d "$(jq -r .expiresAt "$RT")" +%s) > $(date +%s) + 2400 )) || fail "older token overwrote a newer one"
grep -q "^sts" "$CALLS" && fail "banked too early on the host"

reset; export SSO_GUARD_PUSH=box; rm -f "$RT"; fresh '+100 sec'
jq -n --arg e "$(date -u -d '+50 min' +%FT%TZ)" '{Credentials: {Expiration: $e}}' > "$RC"; touch -d '-10 min' "$RC"
out=$(bin/sso-guard push)
grep -q "strat-x: r creds banked until" <<<"$out" || fail "host did not bank: $out"
unset SSO_GUARD_PUSH

echo OK
