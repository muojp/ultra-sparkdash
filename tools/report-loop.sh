#!/bin/bash
# Regenerate results/report.html every 30 minutes while measurements are running.
# The report is a pure function of the run files, so re-running it is always safe: it adds whatever
# runs have landed since and changes nothing else.
#   start: nohup ./tools/report-loop.sh >/dev/null 2>&1 &
#   stop:  kill "$(cat /tmp/ultra-report-loop.pid)"
set -u
cd "$(dirname "$0")/.."
echo $$ > /tmp/ultra-report-loop.pid
while true; do
  ./bin/llm-bench-report >> /tmp/ultra-report-loop.log 2>&1
  sleep 1800
done
