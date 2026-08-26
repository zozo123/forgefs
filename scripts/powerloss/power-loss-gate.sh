#!/bin/bash
# CI-sized power-loss gate for I4. One line for CI, wider on demand.
#
#   power-loss-gate.sh <path to forge> [scratch dir]
#
# Runs, in order:
#   1. the interposition audit -- proves the shim actually sees the write path,
#      by counting the same workload at the kernel boundary with strace and at
#      the libc boundary with the shim. A harness with an unstated hole is
#      worse than none.
#   2. MODE=cli        many short-lived processes, acknowledgement-checked.
#   3. MODE=multibatch one long-lived process, many batches, including dropped
#      ones. The only mode that reaches the process-local durability caches.
#
# Widen with: CUTS=64 PL_SECONDS=15 ITERS=2000 power-loss-gate.sh <forge>
# Add torn fsyncs at the cut with PARTIAL=1.
set -u
F=${1:?usage: power-loss-gate.sh <path to forge> [scratch dir]}
SCRATCH=${2:-${TMPDIR:-/tmp}}
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
CUTS=${CUTS:-6}
PL_SECONDS=${PL_SECONDS:-2}
ITERS=${ITERS:-120}
export CUTS PL_SECONDS ITERS

rc=0
echo "== interposition audit"
bash "$HERE/pl-interpose-audit.sh" "$F" "$SCRATCH" || rc=1

echo "== MODE=cli"
MODE=cli bash "$HERE/power-loss.sh" "$F" "$SCRATCH" || rc=1

echo "== MODE=multibatch"
MODE=multibatch bash "$HERE/power-loss.sh" "$F" "$SCRATCH" || rc=1

if [ "$rc" -eq 0 ]; then
  echo "POWER-LOSS GATE PASS cuts=$CUTS"
else
  echo "POWER-LOSS GATE FAIL"
fi
exit $rc
