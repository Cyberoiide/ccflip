#!/usr/bin/env bash
# Self-check for ccflip-watch's account selection. Run: ./tests/pick_target_test.sh
set -euo pipefail
cd "$(dirname "$0")/.."

fixture='{"schemaVersion":1,"activeAccountNumber":1,"accounts":[
 {"number":1,"email":"a@sia.com","active":true,"usageStatus":"ok",
  "usage":{"fiveHour":{"pct":97.0,"resetsAt":"x"},"sevenDay":{"pct":40.0,"resetsAt":"x"}}},
 {"number":2,"email":"b@qm.io","active":false,"usageStatus":"ok",
  "usage":{"fiveHour":{"pct":10.0,"resetsAt":"x"},"sevenDay":{"pct":20.0,"resetsAt":"x"}}},
 {"number":3,"email":"c@dead.io","active":false,"usageStatus":"token_expired","usage":null,"disabled":true}
]}'

# shellcheck source=/dev/null
source <(sed -n '/^pick_target()/,/^}/p' bin/ccflip-watch)
THRESHOLD=95

[[ "$(pick_target 1 <<<"$fixture")" == "2" ]]  # exhausted 5h window -> best healthy
[[ -z "$(pick_target 2 <<<"$fixture")" ]]      # healthy -> no switch
[[ "$(pick_target 3 <<<"$fixture")" == "2" ]]  # dead token -> best healthy
echo OK
