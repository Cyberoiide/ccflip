#!/usr/bin/env bash
# End-to-end check of ccflip's tier logic with fakes — no accounts consumed.
# Fake cswap controls the exit code; fake claude records what reached it.
# Run: ./tests/bedrock_fallback_test.sh
set -euo pipefail
cd "$(dirname "$0")/.."
CCFLIP="$PWD/bin/ccflip"

T=$(mktemp -d); trap 'rm -rf "$T"' EXIT

cat > "$T/claude" <<'EOF'
#!/usr/bin/env bash
{ echo "BEDROCK=${CLAUDE_CODE_USE_BEDROCK:-unset}"; echo "ARGS=$*"; } > "$OUT"
EOF
cat > "$T/cswap" <<'EOF'
#!/usr/bin/env bash
case "$1" in
  auto) exit "$CSWAP_AUTO_RC" ;;
  run)  shift; [[ "${1:-}" == "--" ]] && shift
        { echo "VIA=cswap-run BEDROCK=${CLAUDE_CODE_USE_BEDROCK:-unset}"; echo "ARGS=$*"; } > "$OUT" ;;
esac
EOF
chmod +x "$T/claude" "$T/cswap"
printf 'export CLAUDE_CODE_USE_BEDROCK=1\n' > "$T/bedrock.env"
export PATH="$T:$PATH" OUT="$T/out" CCFLIP_BEDROCK_ENV="$T/bedrock.env"

# All accounts exhausted (exit 3) -> Bedrock env applied, args forwarded.
CSWAP_AUTO_RC=3 "$CCFLIP" -p hello 2>/dev/null
grep -qx 'BEDROCK=1' "$OUT" && grep -qx 'ARGS=-p hello' "$OUT"

# Accounts healthy (exit 2) -> hands off to cswap run, and a stale Bedrock
# env in the calling shell must be scrubbed, not passed through.
CLAUDE_CODE_USE_BEDROCK=1 CSWAP_AUTO_RC=2 "$CCFLIP" -p hello 2>/dev/null
grep -qx 'VIA=cswap-run BEDROCK=unset' "$OUT" && grep -qx 'ARGS=-p hello' "$OUT"

# Exhausted but no bedrock.env -> still launches (session will pause at limit).
rm "$T/bedrock.env"
CSWAP_AUTO_RC=3 "$CCFLIP" -p hello 2>/dev/null
grep -qx 'VIA=cswap-run BEDROCK=unset' "$OUT"
echo OK
