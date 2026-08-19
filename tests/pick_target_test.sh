#!/usr/bin/env bash
# Self-check for ccflip-watch's account selection. Run: ./tests/pick_target_test.sh
set -euo pipefail
cd "$(dirname "$0")/.."

fixture='{"schemaVersion":1,"activeAccountNumber":1,"accounts":[
 {"number":1,"email":"a@sia.com","active":true,"usageStatus":"ok",
  "usage":{"fiveHour":{"pct":97.0,"resetsAt":"x"},"sevenDay":{"pct":40.0,"resetsAt":"x"}}},
 {"number":2,"email":"b@qm.io","active":false,"usageStatus":"ok",
  "usage":{"fiveHour":{"pct":10.0,"resetsAt":"x"},"sevenDay":{"pct":20.0,"resetsAt":"x"}}},
 {"number":3,"email":"c@dead.io","active":false,"usageStatus":"token_expired","usage":null,"disabled":true},
 {"number":4,"email":"d@api.com","active":false,"usageStatus":"ok",
  "usage":{"spend":{"used":14.12,"limit":100.0}}},
 {"number":5,"email":"e@unknown.io","active":false,"usageStatus":"ok","usage":null}
]}'

# shellcheck source=/dev/null
source <(sed -n '/^pick_target()/,/^}/p' bin/ccflip-watch)
THRESHOLD=95

[[ "$(pick_target 1 <<<"$fixture")" == "2" ]]  # exhausted -> windowed beats spend
[[ -z "$(pick_target 2 <<<"$fixture")" ]]      # healthy -> no switch
[[ "$(pick_target 3 <<<"$fixture")" == "2" ]]  # dead token -> best windowed

# Every windowed account dry -> spend account is the last resort; the
# unknown-usage account (5) is never picked over it.
allspent=$(jq '.accounts[1].usage.fiveHour.pct = 99' <<<"$fixture")
[[ "$(pick_target 1 <<<"$allspent")" == "4" ]]

# Spend gone too (token dead) -> nothing left, hold.
nospend=$(jq '.accounts[3].usageStatus = "token_expired"' <<<"$allspent")
[[ -z "$(pick_target 1 <<<"$nospend")" ]]
echo OK
