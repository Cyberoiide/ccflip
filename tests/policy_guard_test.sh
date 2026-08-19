#!/usr/bin/env bash
# Self-check for the watcher's policy guard. Run: ./tests/policy_guard_test.sh
set -euo pipefail
cd "$(dirname "$0")/.."

T=$(mktemp -d); trap 'rm -rf "$T"' EXIT

# Fake cswap: serves the fixture, records switch/disable calls.
cat > "$T/cswap" <<'EOF'
#!/usr/bin/env bash
case "$1" in
  list)    cat "$LIST_FIXTURE" ;;
  switch)  echo "switch $2"  >> "$CALLS" ;;
  disable) echo "disable $2" >> "$CALLS" ;;
esac
EOF
chmod +x "$T/cswap"
export PATH="$T:$PATH" LIST_FIXTURE="$T/list.json" CALLS="$T/calls"
export CCFLIP_POLICY="$T/policy.env"
cat > "$CCFLIP_POLICY" <<'EOF'
CCFLIP_DEFAULT_ACCOUNT=sia@x.com
CCFLIP_PINNED_ONLY="qm@x.com"
EOF

# shellcheck source=/dev/null
source <(sed -n '/^POLICY=/p; /^log()/p; /^enforce_policy()/,/^}/p' bin/ccflip-watch)

# Violation: pinned-only account active AND enabled -> switch back + disable.
cat > "$LIST_FIXTURE" <<'EOF'
{"accounts":[
 {"number":1,"email":"sia@x.com","active":false,"usageStatus":"ok"},
 {"number":2,"email":"qm@x.com","active":true,"usageStatus":"ok"}]}
EOF
: > "$CALLS"; enforce_policy >/dev/null
grep -qx "switch sia@x.com" "$CALLS" && grep -qx "disable qm@x.com" "$CALLS"

# Compliant: sia active, qm disabled -> no calls.
cat > "$LIST_FIXTURE" <<'EOF'
{"accounts":[
 {"number":1,"email":"sia@x.com","active":true,"usageStatus":"ok"},
 {"number":2,"email":"qm@x.com","active":false,"usageStatus":"ok","disabled":true}]}
EOF
: > "$CALLS"; enforce_policy >/dev/null
[[ ! -s "$CALLS" ]]

# No policy file -> no-op.
rm "$CCFLIP_POLICY"; : > "$CALLS"; enforce_policy >/dev/null
[[ ! -s "$CALLS" ]]
echo OK
