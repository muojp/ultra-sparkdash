#!/bin/bash
# One measurement pass over a list of deployments: switch, sweep every scenario, probe long context.
# Sequential by necessity — they share port 8888 — and each step is logged so a failure says which
# deployment and which leg.
#
#   tools/measure-pass.sh deepseek-v4-flash deepseek-v4.1-flash
set -u
cd "$(dirname "$0")/.."
LOG_DIR="${LOG_DIR:-/tmp/measure-pass}"
mkdir -p "$LOG_DIR"
stamp() { date -u +%H:%MZ; }

for dep in "$@"; do
  echo "=== $(stamp) $dep: switch ==="
  if ! ./bin/dgx-model switch "$dep" > "$LOG_DIR/$dep.switch.log" 2>&1; then
    echo "$(stamp) $dep: SWITCH FAILED — $(tail -2 "$LOG_DIR/$dep.switch.log" | tr -d '\r' | tail -1)"
    continue
  fi
  echo "=== $(stamp) $dep: bench (all scenarios) ==="
  ./bin/dgx-model bench -- --scenario all -c 1,2,4,8 --no-thinking \
      --note "measurement pass" > "$LOG_DIR/$dep.bench.log" 2>&1 \
    || echo "$(stamp) $dep: bench failed"
  # Reasoning changes review quality as much as the model does, so the review scenario is run both
  # ways. Separate rows, not an average: a lane that reads only message content sees one of them.
  echo "=== $(stamp) $dep: bench (review, thinking on and off) ==="
  ./bin/dgx-model bench -- --scenario review -c 1,4 --thinking-modes on,off \
      --note "reasoning sweep" > "$LOG_DIR/$dep.review-modes.log" 2>&1 \
    || echo "$(stamp) $dep: review-mode sweep failed"
  echo "=== $(stamp) $dep: longctx ==="
  ./bin/dgx-model longctx -- --tokens 115000 -c 4 --no-thinking \
      --note "measurement pass" > "$LOG_DIR/$dep.longctx.log" 2>&1 \
    || echo "$(stamp) $dep: longctx failed"
  ./bin/llm-bench-report > /dev/null 2>&1
  echo "=== $(stamp) $dep: done ==="
done
echo "MEASURE_PASS_DONE $(stamp)"
