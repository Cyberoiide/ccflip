#!/usr/bin/env bash
# End-to-end check of ccflip's routing with fakes — no accounts consumed.
# Fake cswap serves a fixture for `list --json`; fake claude and fake
# `cswap run` record what reached them. Run: ./tests/bedrock_fallback_test.sh
set -euo pipefail
cd "$(dirname "$0")/.."
CCFLIP="$PWD/bin/ccflip"

T=$(mktemp -d); trap 'rm -rf "$T"' EXIT

cat > "$T/claude" <<'EOF'
#!/usr/bin/env bash
{ echo "VIA=claude BEDROCK=${CLAUDE_CODE_USE_BEDROCK:-unset}"; echo "ARGS=$*"; } > "$OUT"
EOF
cat > "$T/cswap" <<'EOF'
#!/usr/bin/env bash
case "$1" in
  list) cat "$LIST_FIXTURE" ;;
  run)  shift; acct=""; [[ "${1:-}" != "--" && -n "${1:-}" ]] && { acct=$1; shift; }
        [[ "${1:-}" == "--" ]] && shift
        { echo "VIA=cswap-run ACCT=${acct:-default} BEDROCK=${CLAUDE_CODE_USE_BEDROCK:-unset}"
          echo "ARGS=$*"; } > "$OUT" ;;
esac
EOF
chmod +x "$T/claude" "$T/cswap"
export PATH="$T:$PATH" OUT="$T/out" CCFLIP_BEDROCK_ENV="$T/bedrock.env"
export LIST_FIXTURE="$T/list.json" CCFLIP_MAPPINGS="$T/mappings.json"
printf 'export CLAUDE_CODE_USE_BEDROCK=1\n' > "$T/bedrock.env"

# Fixture: 1=sia (spend-only, no windows), 2=qm (windowed, disabled, mapped).
list() { # $1 = qm 5h pct, $2 = sia usageStatus
    cat > "$LIST_FIXTURE" <<EOF
{"schemaVersion":1,"activeAccountNumber":1,"accounts":[
 {"number":1,"email":"sia@x.com","active":true,"usageStatus":"$2",
  "usage":{"spend":{"used":14.0,"limit":100.0}}},
 {"number":2,"email":"qm@x.com","active":false,"usageStatus":"ok","disabled":true,
  "usage":{"fiveHour":{"pct":$1,"resetsAt":"x"},"sevenDay":{"pct":5,"resetsAt":"x"}}}
]}
EOF
}
map_qm() { echo '{"schemaVersion":1,"mappings":{"'"$1"'":{"email":"qm@x.com"}}}' > "$CCFLIP_MAPPINGS"; }
check() { grep -qxF "$1" "$OUT" || { echo "FAIL($2): wanted '$1', got:"; cat "$OUT"; exit 1; }; }

QMDIR="$T/quiet-mind"; mkdir -p "$QMDIR/darkfindr"; map_qm "$QMDIR"

# Unmapped + sia ok -> default login, stale Bedrock env scrubbed.
list 5 ok; cd "$T"
CLAUDE_CODE_USE_BEDROCK=1 "$CCFLIP" -p hello 2>/dev/null
check 'VIA=cswap-run ACCT=default BEDROCK=unset' unmapped-ok
check 'ARGS=-p hello' unmapped-ok-args

# Unmapped + sia dead + qm disabled -> Bedrock (disabled never borrowed).
list 5 token_expired
"$CCFLIP" -p hello 2>/dev/null
check 'VIA=claude BEDROCK=1' unmapped-dead

# Mapped subdir + qm fresh -> pinned account via bare run (disabled is fine
# for its own dir).
list 5 ok; cd "$QMDIR/darkfindr"
CLAUDE_CODE_USE_BEDROCK=1 "$CCFLIP" -p hello 2>/dev/null
check 'VIA=cswap-run ACCT=default BEDROCK=unset' mapped-fresh

# Mapped + qm dry -> explicit alternate account (sia).
list 100 ok
"$CCFLIP" -p hello 2>/dev/null
check 'VIA=cswap-run ACCT=sia@x.com BEDROCK=unset' mapped-dry-alt

# Mapped + qm dry + sia dead -> Bedrock.
list 100 token_expired
"$CCFLIP" -p hello 2>/dev/null
check 'VIA=claude BEDROCK=1' mapped-all-dry

# Bedrock model override rewrites t3code's explicit --model flag.
printf 'export CLAUDE_CODE_USE_BEDROCK=1\nexport CCFLIP_BEDROCK_MODEL_OVERRIDE="claude-opus-5[1m]"\n' > "$T/bedrock.env"
list 100 token_expired
"$CCFLIP" --model 'claude-fable-5[1m]' -p hello 2>/dev/null
check 'ARGS=--model claude-opus-5[1m] -p hello' model-override

# Mapped + everything dry + no bedrock.env -> still launches.
rm "$T/bedrock.env"
"$CCFLIP" -p hello 2>/dev/null
check 'VIA=cswap-run ACCT=default BEDROCK=unset' no-bedrock-env
echo OK
