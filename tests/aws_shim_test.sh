#!/usr/bin/env bash
# Self-check for the on-demand login: bin/aws shim + `sso-guard ensure`.
# Hermetic: fake aws/systemctl in a throwaway HOME. Run: ./tests/aws_shim_test.sh
set -euo pipefail
cd "$(dirname "$0")/.."

T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
export HOME="$T/home" CALLS="$T/calls" XDG_STATE_HOME="$T/state"
unset AWS_PROFILE AWS_DEFAULT_PROFILE AWS_ACCESS_KEY_ID AWS_CONFIG_FILE
mkdir -p "$HOME/.local/bin" "$HOME/.aws/sso/cache" "$T/shim"
# sso-guard puts $HOME/.local/bin first on its PATH, so the fakes live there:
# no test can ever reach the real aws.
ln -s "$PWD/bin/aws" "$T/shim/aws"
export PATH="$T/shim:$HOME/.local/bin:/usr/bin:/bin"

cat > "$HOME/.local/bin/systemctl" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
cat > "$HOME/.local/bin/aws" <<'EOF'
#!/usr/bin/env bash
echo "$*" >> "$CALLS"
case "$*" in
  *create-token*) [[ -n "${REFRESH_OK:-}" ]] && echo '{"accessToken":"new","expiresIn":3600}' || exit 1 ;;
  "sso login --sso-session "*)
    sleep "${LOGIN_SLEEP:-0}"
    [[ -n "${LOGIN_OK:-}" ]] || exit 1
    echo "login chatter"   # must not reach the caller's stdout
    f="$HOME/.aws/sso/cache/$(printf %s "${!#}" | sha1sum | cut -c1-40).json"
    jq --arg e "$(date -u -d '+1 hour' +%FT%TZ)" '.expiresAt = $e' "$f" > "$f.t" && mv "$f.t" "$f" ;;
  *) echo "REAL $*"; exit "${REAL_RC:-0}" ;;
esac
EOF
chmod +x "$HOME/.local/bin/"*

cat > "$HOME/.aws/config" <<'EOF'
[sso-session strat]
sso_start_url = https://strat.example/start
sso_region = eu-west-1

[profile strat-staging]
sso_session = strat
sso_account_id = 111
sso_role_name = Admin

[profile plain]
region = eu-west-1
EOF
TOKEN="$HOME/.aws/sso/cache/$(printf strat | sha1sum | cut -c1-40).json"
token() { # $1 = expiresAt offset for date -d
    jq -n --arg e "$(date -u -d "$1" +%FT%TZ)" '{startUrl: "https://strat.example/start",
        region: "eu-west-1", accessToken: "old", expiresAt: $e,
        clientId: "c", clientSecret: "s", refreshToken: "r"}' > "$TOKEN"
}
reset() { : > "$CALLS"; rm -rf "$XDG_STATE_HOME"; unset REFRESH_OK LOGIN_OK LOGIN_SLEEP REAL_RC DISPLAY WAYLAND_DISPLAY; }
calls() { cat "$CALLS"; }
fail() { echo "FAIL: $*"; echo "--- calls"; calls; exit 1; }

# Live token: straight through, untouched.
reset; token '+30 min'
out=$(aws --profile strat-staging sts get-caller-identity)
[[ "$out" == "REAL --profile strat-staging sts get-caller-identity" ]] || fail "live: $out"
[[ $(calls) == "--profile strat-staging sts get-caller-identity" ]] || fail "live: extra calls"

# Expired, IdC session alive: silent refresh, no browser.
reset; token '-5 min'; export REFRESH_OK=1 DISPLAY=:0
aws --profile strat-staging s3 ls >/dev/null
grep -q create-token "$CALLS" && ! grep -q "sso login" "$CALLS" || fail "refresh"
[[ $(jq -r .accessToken "$TOKEN") == new ]] || fail "refresh not written back"

# Expired, session over, display: one browser login, then the command; the
# login output stays off stdout (kubectl parses get-token's stdout as JSON).
reset; token '-5 min'; export LOGIN_OK=1 WAYLAND_DISPLAY=wayland-1
out=$(AWS_PROFILE=strat-staging aws eks get-token --cluster-name thor 2>/dev/null)
[[ "$out" == "REAL eks get-token --cluster-name thor" ]] || fail "login stdout: $out"
[[ $(grep -c "sso login --sso-session strat" "$CALLS") == 1 ]] || fail "login count"
(( $(date -d "$(jq -r .expiresAt "$TOKEN")" +%s) > $(date +%s) + 3000 )) || fail "login not fresh"

# Parallel callers (k9s): one login, everyone waits for it.
reset; token '-5 min'; export LOGIN_OK=1 LOGIN_SLEEP=1 DISPLAY=:0
for _ in 1 2 3; do aws --profile=strat-staging sts get-caller-identity >/dev/null & done; wait
[[ $(grep -c "sso login" "$CALLS") == 1 ]] || fail "parallel: $(grep -c 'sso login' "$CALLS") logins"
[[ $(grep -c "REAL\|sts get-caller-identity" "$CALLS") == 3 ]] || fail "parallel: not all ran"

# Expired, but role creds (banked) still serve the call: no refresh, no login.
# Under 16 min left, the real aws would need the token: log in.
CREDS="$HOME/.aws/cli/cache/$(printf '{"accountId":"111","roleName":"Admin","sessionName":"strat"}' | sha1sum | cut -c1-40).json"
mkdir -p "$HOME/.aws/cli/cache"
reset; token '-5 min'; export LOGIN_OK=1 DISPLAY=:0
jq -n --arg e "$(date -u -d '+40 min' +%FT%TZ)" '{Credentials: {Expiration: $e}}' > "$CREDS"
out=$(aws --profile strat-staging sts get-caller-identity)
[[ "$out" == "REAL --profile strat-staging sts get-caller-identity" ]] || fail "banked: $out"
grep -q "create-token\|sso login" "$CALLS" && fail "banked creds ignored"
reset; token '-5 min'; export LOGIN_OK=1 DISPLAY=:0
jq -n --arg e "$(date -u -d '+10 min' +%FT%TZ)" '{Credentials: {Expiration: $e}}' > "$CREDS"
aws --profile strat-staging sts get-caller-identity >/dev/null 2>&1
grep -q "sso login" "$CALLS" || fail "near-expiry banked creds should log in"
rm -f "$CREDS"

# No display: no login attempt, the real command still runs, its rc kept.
reset; token '-5 min'; export REAL_RC=3
rc=0; aws --profile strat-staging sts get-caller-identity >/dev/null 2>&1 || rc=$?
[[ $rc == 3 ]] && ! grep -q "sso login" "$CALLS" || fail "no display: rc=$rc"

# Never intercepted: logins themselves, non-SSO profiles, env keys.
reset; token '-5 min'; export LOGIN_OK=1 DISPLAY=:0
aws sso login --profile strat-staging >/dev/null
aws --profile plain sts get-caller-identity >/dev/null
AWS_ACCESS_KEY_ID=x AWS_PROFILE=strat-staging aws sts get-caller-identity >/dev/null
[[ $(grep -c "sso login --sso-session" "$CALLS") == 0 ]] || fail "intercepted a skip case"
grep -q create-token "$CALLS" && fail "refreshed a skip case"

# A second, different shim later on PATH: no exec ping-pong, the real aws runs.
reset; token '+30 min'
mkdir -p "$T/shim2"; cp bin/aws bin/sso-guard "$T/shim2/"
out=$(PATH="$T/shim:$T/shim2:$PATH" timeout 10 aws --profile strat-staging s3 ls) || fail "two shims looped"
[[ "$out" == "REAL --profile strat-staging s3 ls" ]] || fail "two shims: $out"

echo OK
