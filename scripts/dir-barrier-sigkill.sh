#!/bin/bash
# SIGKILL durability proof for the directory-barrier phase.
#
# Sustained concurrent checkin load; each writer appends `updated <ref> <oid>`
# to an acknowledgement log ONLY after its checkin process exited 0 with that
# line on stdout. The log lives on tmpfs, which is kernel memory: SIGKILL of
# the writers cannot lose an acknowledgement, so nothing the repository must
# account for is missing from it.
#
# After the kill the repository is reopened cold, `fsck --full` must pass, and
# every acknowledged ref must resolve to exactly its acknowledged oid.
set -u
F=${1:?usage: dir-barrier-sigkill.sh <path to forge> [scratch dir]}
SCRATCH=${2:-${TMPDIR:-/tmp}}
POL=${POL:-deferred}
RUNS=${RUNS:-8}
WRITERS=${WRITERS:-8}
total_acked=0
total_runs=0
for run in $(seq 1 "$RUNS"); do
  REPO=$SCRATCH/forge-sigkill-repo
  ACK=/dev/shm/acks.$run
  rm -rf "$REPO"; mkdir -p "$REPO"; : > "$ACK"
  "$F" init "$REPO" >/dev/null || exit 1
  CAP=$REPO/.forge/keys/root.cap
  setsid bash -c '
    F="$1"; REPO="$2"; CAP="$3"; ACK="$4"; W="$5"; POL="$6"
    export FORGEFS_DIR_BARRIER="$POL"
    for i in $(seq 1 "$W"); do
      (
        while :; do
          NS=$("$F" --dir "$REPO" --cap "$CAP" session open --from=main 2>/dev/null) || exit 0
          "$F" --dir "$REPO" --cap "$CAP" write --ns "$NS" --text "writer-$i-$RANDOM" "/w$i.txt" >/dev/null 2>&1 || exit 0
          OUT=$("$F" --dir "$REPO" --cap "$CAP" checkin --ns "$NS" -m load 2>/dev/null) || exit 0
          case "$OUT" in updated*) printf "%s\n" "$OUT" >> "$ACK" ;; esac
        done
      ) &
    done
    wait
  ' _ "$F" "$REPO" "$CAP" "$ACK" "$WRITERS" "$POL" &
  GROUP=$!
  sleep "$(awk -v s=$run 'BEGIN{srand(s*7919);printf "%.2f", 1.5+rand()*2.5}')"
  kill -9 -"$GROUP" 2>/dev/null
  wait "$GROUP" 2>/dev/null
  # nothing of ours may still be running against the repository
  while pgrep -f "cap $CAP" >/dev/null 2>&1; do sleep 0.05; done
  sleep 0.3
  acked=$(wc -l < "$ACK")
  # Cold reopen. fsck --full rereads durable bytes; it never repairs.
  if ! "$F" --dir "$REPO" --cap "$CAP" fsck --full >/tmp/fsck.$run 2>&1; then
    echo "RUN $run FAIL fsck --full"; cat /tmp/fsck.$run; exit 1
  fi
  "$F" --dir "$REPO" --cap "$CAP" refs 2>/dev/null | awk '{print $3, $4}' | sort > /tmp/refs.$run
  missing=0
  while read -r _ ref oid; do
    if ! grep -qx "$ref $oid" /tmp/refs.$run; then
      echo "RUN $run LOST ack $ref $oid"; missing=$((missing+1))
    fi
  done < "$ACK"
  if [ "$missing" -ne 0 ]; then echo "RUN $run FAIL $missing lost acknowledgements"; exit 1; fi
  echo "run=$run policy=$POL writers=$WRITERS acked=$acked fsck=clean lost=0"
  total_acked=$((total_acked+acked)); total_runs=$((total_runs+1))
  rm -f "$ACK"
done
echo "SIGKILL SUMMARY policy=$POL runs=$total_runs acknowledged_checkins=$total_acked lost=0"
