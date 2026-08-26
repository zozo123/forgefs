#!/bin/bash
# Power-loss durability proof for I4.
#
# The kill -9 suite destroys the PROCESS and leaves the PAGE CACHE intact, so
# the filesystem it inspects afterwards still contains writes that never
# reached the device. It therefore cannot see the defect class I4 exists to
# prevent. This harness can.
#
# A checkin workload runs under an LD_PRELOAD interposer that records the
# ordered stream of durability-relevant libc calls: writes, the fsyncs that
# cover them, and every namespace operation. A replay tool then reconstructs
# the device image at a chosen cut point, keeping ONLY what a completed fsync
# had covered and adversarially dropping the rest.
#
# The postcondition is I4 itself: a ref that survived the cut is a COMMITTED
# ref, so `forge fsck` -- which roots the metadata and walks every object
# reachable from it -- must find all of its bytes and directory edges.
#
#   power-loss.sh <path to forge> [scratch dir]
#
# MODE=cli    (default) many short-lived forge processes, plus an explicit
#             acknowledgement log ordered into the same stream, so a ref that
#             was PROMISED and then lost is named. One publish batch per
#             process.
# MODE=bench  one long-lived process (`forge bench`) running many publish
#             batches CONCURRENTLY, with `init` itself inside the trace and an
#             EMPTY baseline: nothing at all is durable by assumption.
# MODE=multibatch  one long-lived process running many SEQUENTIAL batches,
#             including batches that are dropped without finishing (the I22
#             refusal path). This is the only mode that reaches the
#             process-local durability caches; a one-batch-per-process CLI
#             workload cannot exercise them at all. Needs the
#             `powerloss_multibatch` example binary, found next to `forge` or
#             named by PL_WORKLOAD.
#
# Env: WRITERS(4) PL_SECONDS(3) CUTS(8) POL(deferred) PARTIAL(0)
#      AGENTS(48) SHARED(24) BWORKERS(16) ITERS(400)
set -u
F=${1:?usage: power-loss.sh <path to forge> [scratch dir]}
F=$(cd "$(dirname "$F")" && pwd)/$(basename "$F")
SCRATCH=$(cd "${2:-${TMPDIR:-/tmp}}" && pwd)/forge-powerloss
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
MODE=${MODE:-cli}
WRITERS=${WRITERS:-4}
RUNSECS=${PL_SECONDS:-3}
CUTS=${CUTS:-8}
POL=${POL:-deferred}
PARTIAL=${PARTIAL:-0}
ITERS=${ITERS:-400}
AGENTS=${AGENTS:-48}
SHARED=${SHARED:-24}
BWORKERS=${BWORKERS:-16}

command -v cc >/dev/null || { echo "PL SKIP: no C compiler"; exit 0; }
command -v python3 >/dev/null || { echo "PL SKIP: no python3"; exit 0; }
case "$(uname -s)" in Linux) ;; *) echo "PL SKIP: LD_PRELOAD harness is Linux-only"; exit 0 ;; esac
if ! ldd "$F" 2>/dev/null | grep -q libc; then
  echo "PL FAIL: $F is not dynamically linked against libc; LD_PRELOAD cannot see it"
  exit 1
fi

rm -rf "$SCRATCH"; mkdir -p "$SCRATCH"
SHIM=$SCRATCH/pl_shim.so
cc -O2 -fPIC -shared -o "$SHIM" "$HERE/pl_shim.c" -ldl -lpthread || exit 1
PLMARK=$SCRATCH/pl_mark
cc -O2 -o "$PLMARK" "$HERE/pl_mark.c" || exit 1

REPO=$SCRATCH/repo
JDIR=$SCRATCH/journal
MARK=$SCRATCH/mark
BASE=$SCRATCH/baseline
OUT=$SCRATCH/replay
mkdir -p "$JDIR" "$MARK"

export FORGEFS_DIR_BARRIER="$POL"
export PL_JOURNAL_DIR="$JDIR" PL_ROOT="$REPO" PL_MARK="$MARK"

if [ "$MODE" = bench ]; then
  # `bench` creates its own workspace, so the whole repository -- init
  # included -- is inside the trace and the baseline is empty. Nothing is
  # durable by assumption; every byte and every edge has to be earned.
  mkdir -p "$BASE"
  echo "PL workload=bench agents=$AGENTS shared=$SHARED workers=$BWORKERS policy=$POL"
  LD_PRELOAD="$SHIM" "$F" bench --scratch "$REPO" --agents "$AGENTS" \
    --shared "$SHARED" --workers "$BWORKERS" >"$SCRATCH/bench.log" 2>&1
  BRC=$?
  if [ "$BRC" -ne 0 ]; then
    echo "PL FAIL: bench exited $BRC"; tail -5 "$SCRATCH/bench.log"; exit 1
  fi
  ACKED=0
elif [ "$MODE" = multibatch ]; then
  WL=${PL_WORKLOAD:-$(dirname "$F")/examples/powerloss_multibatch}
  if [ ! -x "$WL" ]; then
    echo "PL SKIP: multibatch workload not built ($WL); cargo build --examples"
    exit 0
  fi
  "$F" init "$REPO" >/dev/null || exit 1
  sync
  cp -a "$REPO" "$BASE"
  echo "PL workload=multibatch iterations=$ITERS policy=$POL"
  LD_PRELOAD="$SHIM" "$WL" "$REPO" "$MARK/acks" "$ITERS" || {
    echo "PL FAIL: multibatch workload failed"; exit 1; }
  ACKED=$(wc -l < "$MARK/acks" 2>/dev/null || echo 0)
  if [ "$ACKED" -lt 1 ]; then echo "PL FAIL: workload acknowledged nothing"; exit 1; fi
else
  # The baseline is created OUTSIDE the trace and forced to the device, so the
  # replay starts from a state durable by construction.
  "$F" init "$REPO" >/dev/null || exit 1
  sync
  cp -a "$REPO" "$BASE"
  CAP=$REPO/.forge/keys/root.cap
  if find "$REPO" -type l | grep -q .; then
    echo "PL FAIL: traced tree contains symlinks"; exit 1
  fi
  echo "PL workload=cli writers=$WRITERS seconds=$RUNSECS policy=$POL"
  LD_PRELOAD="$SHIM" setsid bash -c '
    F="$1"; REPO="$2"; CAP="$3"; ACK="$4"; W="$5"; MK="$6"
    for i in $(seq 1 "$W"); do
      (
        while :; do
          NS=$("$F" --dir "$REPO" --cap "$CAP" session open --from=main 2>/dev/null) || exit 0
          "$F" --dir "$REPO" --cap "$CAP" write --ns "$NS" --text "writer-$i-$RANDOM" "/w$i.txt" >/dev/null 2>&1 || exit 0
          OUT=$("$F" --dir "$REPO" --cap "$CAP" checkin --ns "$NS" -m load 2>/dev/null) || exit 0
          # NOT a shell redirection: a builtin flushes through glibc stdio,
          # which calls a libc-internal write no LD_PRELOAD object can
          # interpose, and the promise would miss the ordered stream.
          case "$OUT" in updated*) "$MK" "$ACK" "$OUT" ;; esac
        done
      ) &
    done
    wait
  ' _ "$F" "$REPO" "$CAP" "$MARK/acks" "$WRITERS" "$PLMARK" &
  GROUP=$!
  sleep "$RUNSECS"
  kill -TERM -"$GROUP" 2>/dev/null
  sleep 0.4
  kill -9 -"$GROUP" 2>/dev/null
  wait "$GROUP" 2>/dev/null
  while pgrep -f "cap $CAP" >/dev/null 2>&1; do sleep 0.05; done
  ACKED=$(wc -l < "$MARK/acks" 2>/dev/null || echo 0)
  if [ "$ACKED" -lt 1 ]; then echo "PL FAIL: workload acknowledged nothing"; exit 1; fi
fi
unset LD_PRELOAD

echo "PL acknowledged_checkins=$ACKED journal_bytes=$(cat "$JDIR"/j.* 2>/dev/null | wc -c)"

EXTRA=""
[ "$PARTIAL" = "1" ] && EXTRA=--partial
python3 "$HERE/pl_replay.py" --journal "$JDIR" --root "$REPO" \
  --baseline "$BASE" --out "$OUT" --forge "$F" --cuts "$CUTS" \
  --cap-source "$REPO/.forge/keys/root.cap" --stats $EXTRA
RC=$?
echo "PL SUMMARY mode=$MODE policy=$POL cuts=$CUTS acknowledged=$ACKED rc=$RC"
exit $RC
