#!/bin/bash
# Does the interposer actually SEE the write path? Measure, do not assume.
#
# A durability harness with an unstated hole is worse than none, because it
# manufactures confidence. This runs the same single checkin twice -- once
# under strace, once under the shim -- and prints both counts side by side.
# strace sees the kernel boundary; the shim sees the libc boundary. Where they
# disagree, the shim is blind, and the difference is named here rather than
# discovered later.
#
#   pl-interpose-audit.sh <path to forge> [scratch dir]
set -u
F=${1:?usage: pl-interpose-audit.sh <path to forge> [scratch dir]}
F=$(cd "$(dirname "$F")" && pwd)/$(basename "$F")
SCRATCH=$(cd "${2:-${TMPDIR:-/tmp}}" && pwd)/forge-pl-audit
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
command -v strace >/dev/null || { echo "AUDIT SKIP: strace not installed"; exit 0; }
command -v cc >/dev/null || { echo "AUDIT SKIP: no C compiler"; exit 0; }

rm -rf "$SCRATCH"; mkdir -p "$SCRATCH"
SHIM=$SCRATCH/pl_shim.so
cc -O2 -fPIC -shared -o "$SHIM" "$HERE/pl_shim.c" -ldl -lpthread || exit 1

SYSCALLS=write,pwrite64,writev,pwritev,fsync,fdatasync,sync_file_range,rename,renameat,renameat2,link,linkat,unlink,unlinkat,mkdir,mkdirat,rmdir,ftruncate,truncate,mmap,msync,copy_file_range,sendfile,io_uring_setup

run_once() {  # $1 = repo, $2 = extra env prefix marker
  local REPO=$1 CAP=$1/.forge/keys/root.cap
  local NS
  NS=$("$F" --dir "$REPO" --cap "$CAP" session open --from=main) || return 1
  "$F" --dir "$REPO" --cap "$CAP" write --ns "$NS" --text "audit-payload" /a.txt >/dev/null || return 1
  "$F" --dir "$REPO" --cap "$CAP" checkin --ns "$NS" -m audit >/dev/null || return 1
}

# ---- kernel boundary -------------------------------------------------------
R1=$SCRATCH/r1
"$F" init "$R1" >/dev/null || exit 1
strace -f -qq -e trace=$SYSCALLS -o "$SCRATCH/strace.txt" \
  bash -c "$(declare -f run_once); F=$F; run_once $R1" >/dev/null 2>&1

# ---- libc boundary ---------------------------------------------------------
R2=$SCRATCH/r2
JDIR=$SCRATCH/journal; mkdir -p "$JDIR"
"$F" init "$R2" >/dev/null || exit 1
export LD_PRELOAD="$SHIM" PL_JOURNAL_DIR="$JDIR" PL_ROOT="$R2" PL_MARK="$SCRATCH/mark"
run_once "$R2" >/dev/null 2>&1
unset LD_PRELOAD PL_JOURNAL_DIR PL_ROOT PL_MARK

python3 - "$SCRATCH/strace.txt" "$JDIR" "$R1" <<'PY'
import os, re, struct, sys
# The two runs use two repositories, so the strace filter must name the one
# strace actually watched.
strace_path, jdir, root = sys.argv[1], sys.argv[2], sys.argv[3]

# strace: count successful calls, excluding std fds for the write family and
# excluding paths outside the repository for the path-taking calls.
line_re = re.compile(r'^(?:\d+\s+)?([a-z_0-9]+)\((.*)\)\s+=\s+(-?\d+)')
kern = {}
for line in open(strace_path, errors="replace"):
    m = line_re.match(line.strip())
    if not m:
        continue
    call, args, ret = m.group(1), m.group(2), int(m.group(3))
    if ret < 0:
        continue
    if call in ("write", "pwrite64", "writev", "pwritev", "fsync", "fdatasync",
                "ftruncate", "sync_file_range"):
        fd = args.split(",")[0].strip()
        if fd in ("0", "1", "2"):
            continue
    elif call in ("rename", "renameat", "renameat2", "link", "linkat", "unlink",
                  "unlinkat", "mkdir", "mkdirat", "rmdir", "truncate"):
        if root not in args:
            continue
    elif call == "mmap":
        if "MAP_SHARED" not in args or "PROT_WRITE" not in args:
            continue
    kern[call] = kern.get(call, 0) + 1

REC = struct.Struct("<QIIQQQII")
NAMES = {1: "write", 2: "fsync", 3: "fdatasync", 4: "rename", 5: "link",
         6: "unlink", 7: "mkdir", 8: "rmdir", 9: "create", 10: "ftruncate",
         11: "marker", 12: "syncfs", 13: "msync", 14: "symlink",
         15: "write_osync", 16: "sync_file_range", 17: "mmap_shared_write",
         18: "chmod"}
shim = {}
mapped = set()
for n in os.listdir(jdir):
    if not n.startswith("j."):
        continue
    b = open(os.path.join(jdir, n), "rb").read()
    o = 0
    while o + REC.size <= len(b):
        _, op, _, _, _, _, l1, l2 = REC.unpack_from(b, o)
        p1 = b[o + REC.size:o + REC.size + l1].decode("utf-8", "replace")
        o += REC.size + l1 + l2
        name = NAMES.get(op, str(op))
        shim[name] = shim.get(name, 0) + 1
        if op == 17:
            mapped.add(os.path.basename(p1))

# The shim reports one logical name per group; fold the kernel's spellings.
fold = {
    "write": ["write", "pwrite64", "writev", "pwritev"],
    "fsync": ["fsync"],
    "fdatasync": ["fdatasync"],
    "rename": ["rename", "renameat", "renameat2"],
    "link": ["link", "linkat"],
    "unlink": ["unlink", "unlinkat"],
    "mkdir": ["mkdir", "mkdirat"],
    "rmdir": ["rmdir"],
    "ftruncate": ["ftruncate", "truncate"],
    "sync_file_range": ["sync_file_range"],
    "mmap_shared_write": ["mmap"],
    "msync": ["msync"],
}
print("%-20s %10s %10s   %s" % ("operation", "kernel", "shim", "verdict"))
worst = 0
for name, spellings in fold.items():
    k = sum(kern.get(s, 0) for s in spellings)
    v = shim.get(name, 0)
    if k == 0 and v == 0:
        verdict = "not used"
    elif name in ("mmap_shared_write", "msync"):
        verdict = "OBSERVED-NOT-FOLLOWED (stores bypass libc)"
    elif v >= k:
        verdict = "covered"
    else:
        verdict = "*** BLIND: %d call(s) unseen ***" % (k - v)
        worst = 1
    print("%-20s %10d %10d   %s" % (name, k, v, verdict))
if mapped:
    # WHICH files are mapped decides whether the blind spot matters. The
    # SQLite wal-index (-shm) is a rebuildable cache: the first connection
    # after a crash re-derives it from the -wal, so nothing durable lives
    # only there. A shared writable mapping of the DATABASE or of an object
    # file would be a different story, and would show up right here.
    print("shared-writable mappings cover: %s" % ", ".join(sorted(mapped)))
    unsafe = sorted(m for m in mapped if not m.endswith("-shm"))
    if unsafe:
        print("*** BLIND: mapped files that are not a rebuildable wal-index: %s"
              % ", ".join(unsafe))
        worst = 1
for exotic in ("copy_file_range", "sendfile", "io_uring_setup"):
    if kern.get(exotic):
        print("%-20s %10d %10s   *** BLIND: not interposed ***"
              % (exotic, kern[exotic], "-"))
        worst = 1
print("AUDIT %s" % ("BLIND-SPOT-FOUND" if worst else "OK"))
sys.exit(worst)
PY
RC=$?
echo "AUDIT rc=$RC (kernel counts from strace, shim counts from the journal)"
exit $RC
