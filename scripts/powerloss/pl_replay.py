#!/usr/bin/env python3
"""Reconstruct what a DEVICE would hold after a power cut, and check I4.

Reads the journal produced by pl_shim.c and replays it under an adversarial
durability model: an effect reaches the reconstructed device ONLY if a
completed fsync/fdatasync covered it at the cut point.

  * file data       durable when that file was fsynced (or written O_SYNC)
  * directory edges (create/link/rename/unlink/mkdir/rmdir) durable when the
    PARENT DIRECTORY was fsynced -- not when the file was
  * everything else is dropped, which is exactly what SIGKILL fails to do

At each cut point the surviving image is materialised into a fresh directory
and handed to `forge fsck --full` plus a ref listing. Every acknowledgement
marker ordered before the cut names a ref that MUST resolve to its
acknowledged oid. A missing one is an I4 violation and is printed by name.
"""

import argparse
import os
import random
import shutil
import struct
import subprocess
import sys

REC = struct.Struct("<QIIQQQII")

(OP_WRITE, OP_FSYNC, OP_FDATASYNC, OP_RENAME, OP_LINK, OP_UNLINK, OP_MKDIR,
 OP_RMDIR, OP_CREATE, OP_TRUNCATE, OP_MARKER, OP_SYNCFS, OP_MSYNC, OP_SYMLINK,
 OP_WRITE_SYNC, OP_SFR, OP_MMAP_W, OP_CHMOD) = range(1, 19)

OPNAME = {
    OP_WRITE: "write", OP_FSYNC: "fsync", OP_FDATASYNC: "fdatasync",
    OP_RENAME: "rename", OP_LINK: "link", OP_UNLINK: "unlink",
    OP_MKDIR: "mkdir", OP_RMDIR: "rmdir", OP_CREATE: "create",
    OP_TRUNCATE: "truncate", OP_MARKER: "marker", OP_SYNCFS: "syncfs",
    OP_MSYNC: "msync", OP_SYMLINK: "symlink", OP_WRITE_SYNC: "write_osync",
    OP_SFR: "sync_file_range", OP_MMAP_W: "mmap_shared_write",
    OP_CHMOD: "chmod",
}


def load(journal_dir):
    """Merge every per-process journal into one total order by global seq."""
    recs = []
    for name in os.listdir(journal_dir):
        if not name.startswith("j."):
            continue
        pid = int(name[2:])
        with open(os.path.join(journal_dir, name), "rb") as fh:
            buf = fh.read()
        off = 0
        while off + REC.size <= len(buf):
            seq, op, rpid, roff, rlen, doff, l1, l2 = REC.unpack_from(buf, off)
            off += REC.size
            if off + l1 + l2 > len(buf):
                break  # torn tail of a process killed mid-record
            p1 = buf[off:off + l1].decode("utf-8", "surrogateescape")
            p2 = buf[off + l1:off + l1 + l2].decode("utf-8", "surrogateescape")
            off += l1 + l2
            recs.append((seq, op, pid, roff, rlen, doff, p1, p2))
    recs.sort(key=lambda r: r[0])
    return recs


class Blobs:
    def __init__(self, journal_dir):
        self.dir = journal_dir
        self.fh = {}

    def read(self, pid, off, n):
        f = self.fh.get(pid)
        if f is None:
            f = self.fh[pid] = open(os.path.join(self.dir, "d.%d" % pid), "rb")
        f.seek(off)
        return f.read(n)


class Device:
    """Durable state, plus the per-inode and per-directory effects still only
    in the page cache. A power cut discards everything in the second group."""

    def __init__(self, root):
        self.root = root
        self.dur = {}          # ino -> bytearray, durable content
        self.pend_w = {}       # ino -> [("w", off, pid, doff, len) | ("t", size)]
        self.ns_d = {}         # path -> ("d",) | ("f", ino) | ("l", target)
        self.ns_c = {}
        self.pend_ns = {}      # dirpath -> [(path, node_or_None)]
        self.next_ino = 1
        self.counts = {}
        self.modes = {}       # baseline permission bits to reproduce

    # ---- baseline -------------------------------------------------------
    def load_baseline(self, base):
        for dirpath, dirnames, filenames in os.walk(base):
            rel = os.path.relpath(dirpath, base)
            p = self.root if rel == "." else os.path.join(self.root, rel)
            self.ns_d[p] = ("d",)
            self.modes[p] = os.stat(dirpath).st_mode & 0o7777
            for fn in filenames:
                full = os.path.join(p, fn)
                with open(os.path.join(dirpath, fn), "rb") as fh:
                    data = bytearray(fh.read())
                ino = self.new_ino()
                self.dur[ino] = data
                self.ns_d[full] = ("f", ino)
                self.modes[full] = os.stat(os.path.join(dirpath, fn)).st_mode & 0o7777
        self.ns_d[self.root] = ("d",)
        self.ns_c = dict(self.ns_d)

    def new_ino(self):
        i = self.next_ino
        self.next_ino += 1
        return i

    # ---- model ----------------------------------------------------------
    def ns_add(self, path, node, src=None):
        self.ns_c[path] = node
        if path == self.root:
            # The traced root's own directory entry lives in a parent the trace
            # does not cover, so no fsync of it can ever be observed. Treating
            # the root as durable is the boundary of the model, not a claim.
            self.ns_d[path] = node
            return
        self.pend_ns.setdefault(os.path.dirname(path), []).append((path, node, src))

    def ns_del(self, path, moved=False):
        self.ns_c.pop(path, None)
        self.pend_ns.setdefault(os.path.dirname(path), []).append(
            (path, None, "moved" if moved else None))

    @staticmethod
    def _rekey(mapping, old, new):
        for k in [k for k in mapping if k.startswith(old + "/")]:
            mapping[new + k[len(old):]] = mapping.pop(k)

    def flush_dir(self, d, keep=None):
        ops = self.pend_ns.pop(d, [])
        if keep is not None:
            ops = ops[:keep]
        for path, node, src in ops:
            if node is None:
                self.ns_d.pop(path, None)
                if src != "moved":
                    # A real unlink/rmdir; a rename SOURCE keeps its subtree,
                    # which now hangs off the destination name.
                    for k in [k for k in self.ns_d if k.startswith(path + "/")]:
                        self.ns_d.pop(k, None)
            else:
                self.ns_d[path] = node
                if src:
                    # The subtree moved with the directory entry: entries are
                    # reached through the directory's inode, not its pathname.
                    self._rekey(self.ns_d, src, path)

    def apply_wr(self, ino, op, blobs):
        buf = self.dur.setdefault(ino, bytearray())
        if op[0] == "t":
            size = op[1]
            if size < len(buf):
                del buf[size:]
            else:
                buf.extend(b"\0" * (size - len(buf)))
            return
        _, off, pid, doff, n = op
        data = blobs.read(pid, doff, n)
        if off > len(buf):
            buf.extend(b"\0" * (off - len(buf)))
        buf[off:off + len(data)] = data

    def flush_file(self, ino, blobs, keep=None):
        ops = self.pend_w.pop(ino, [])
        if keep is not None:
            ops = ops[:keep]
        for op in ops:
            self.apply_wr(ino, op, blobs)

    def step(self, rec, blobs):
        seq, op, pid, off, ln, doff, p1, p2 = rec
        self.counts[op] = self.counts.get(op, 0) + 1
        if op == OP_CREATE:
            ino = self.new_ino()
            self.dur.setdefault(ino, bytearray())
            self.ns_add(p1, ("f", ino))
            if off:
                self.modes[p1] = off
        elif op == OP_MKDIR:
            self.ns_add(p1, ("d",))
            if off:
                self.modes[p1] = off
        elif op in (OP_UNLINK, OP_RMDIR):
            self.ns_del(p1)
        elif op == OP_SYMLINK:
            self.ns_add(p1, ("l", p2))
        elif op == OP_LINK:
            node = self.ns_c.get(p1)
            if node:
                self.ns_add(p2, node)
        elif op == OP_RENAME:
            node = self.ns_c.get(p1)
            if node:
                isdir = node[0] == "d"
                if isdir:
                    self._rekey(self.ns_c, p1, p2)
                    self._rekey(self.pend_ns, p1, p2)
                    for d, ops in self.pend_ns.items():
                        self.pend_ns[d] = [
                            (p2 + q[len(p1):] if q.startswith(p1 + "/") else q, n, sr)
                            for q, n, sr in ops]
                    self._rekey(self.modes, p1, p2)
                self.ns_del(p1, moved=isdir)
                self.ns_add(p2, node, src=p1 if isdir else None)
        elif op in (OP_WRITE, OP_WRITE_SYNC):
            node = self.ns_c.get(p1)
            if node is None:
                ino = self.new_ino()
                self.ns_add(p1, ("f", ino))
            else:
                ino = node[1]
            entry = ("w", off, pid, doff, ln)
            if op == OP_WRITE_SYNC:
                self.apply_wr(ino, entry, blobs)
            else:
                self.pend_w.setdefault(ino, []).append(entry)
        elif op == OP_TRUNCATE:
            node = self.ns_c.get(p1)
            if node and node[0] == "f":
                self.pend_w.setdefault(node[1], []).append(("t", ln))
        elif op in (OP_FSYNC, OP_FDATASYNC):
            node = self.ns_c.get(p1) or self.ns_d.get(p1)
            if node is None:
                return
            if node[0] == "d":
                self.flush_dir(p1)
            else:
                self.flush_file(node[1], blobs)
        elif op == OP_CHMOD:
            # Permission bits are not an I4 question; they are carried so the
            # replayed image is openable at all (forge refuses keys != 0700).
            if off:
                self.modes[p1] = off
        elif op == OP_SYNCFS:
            for d in list(self.pend_ns):
                self.flush_dir(d)
            for ino in list(self.pend_w):
                self.flush_file(ino, blobs)

    def tear(self, rec, blobs, rng):
        """Model a power cut DURING the next fsync: a prefix of the effects it
        was covering reached the device and the call never returned. Applied
        only for the image at this cut, then undone -- an fsync that DID return
        covered everything, so tearing the ongoing stream would model a machine
        that loses already-durable bytes, and would fail correct code."""
        _, op, _, _, _, _, p1, _ = rec
        if op not in (OP_FSYNC, OP_FDATASYNC):
            return lambda: None
        node = self.ns_c.get(p1) or self.ns_d.get(p1)
        if node is None:
            return lambda: None
        if node[0] == "d":
            ops = self.pend_ns.get(p1, [])
            if not ops:
                return lambda: None
            keep = rng.randint(0, len(ops))
            touched = {q: self.ns_d.get(q, KeyError) for q, _, _ in ops[:keep]}
            for path, n, src in ops[:keep]:
                if n is None:
                    self.ns_d.pop(path, None)
                else:
                    self.ns_d[path] = n
                    if src:
                        self._rekey(self.ns_d, src, path)

            def undo_dir():
                for q, was in touched.items():
                    if was is KeyError:
                        self.ns_d.pop(q, None)
                    else:
                        self.ns_d[q] = was
            return undo_dir
        ino = node[1]
        ops = self.pend_w.get(ino, [])
        if not ops:
            return lambda: None
        keep = rng.randint(0, len(ops))
        saved = bytes(self.dur.get(ino, b""))
        for o in ops[:keep]:
            self.apply_wr(ino, o, blobs)

        def undo_file():
            self.dur[ino] = bytearray(saved)
        return undo_file

    # ---- materialise ----------------------------------------------------
    def materialise(self, out):
        if os.path.exists(out):
            shutil.rmtree(out)
        os.makedirs(out)
        for path in sorted(self.ns_d):
            node = self.ns_d[path]
            rel = os.path.relpath(path, self.root)
            dest = out if rel == "." else os.path.join(out, rel)
            if node[0] == "d":
                os.makedirs(dest, exist_ok=True)
                # The key directory is refused unless it is 0700, so the
                # replay has to reproduce permissions, not just names.
                if path in self.modes:
                    os.chmod(dest, self.modes[path])
        for path in sorted(self.ns_d):
            node = self.ns_d[path]
            if node[0] == "d":
                continue
            rel = os.path.relpath(path, self.root)
            dest = os.path.join(out, rel)
            parent = os.path.dirname(dest)
            if not os.path.isdir(parent):
                # The directory edge itself never reached the device, so this
                # name is unreachable after the cut. Dropping it is the point.
                continue
            if node[0] == "l":
                os.symlink(node[1], dest)
            else:
                with open(dest, "wb") as fh:
                    fh.write(self.dur.get(node[1], b""))
                if path in self.modes:
                    os.chmod(dest, self.modes[path])
        # The SQLite wal-index (-shm) is a rebuildable mmapped cache: the first
        # connection after a crash always re-derives it from the -wal. Removing
        # it makes the replay deterministic and matches what SQLite does.
        for dirpath, _, files in os.walk(out):
            for f in files:
                if f.endswith("-shm"):
                    os.unlink(os.path.join(dirpath, f))


HEX = __import__("re").compile(r"\b([0-9a-f]{64})\b")


def explain(dev, text):
    """Say WHY a name the checker missed is absent, by naming the first
    directory edge on its path that never reached the device. The leaf entry
    is usually durable -- the batch fsynced the shard it linked into -- and the
    edge that was skipped is an ANCESTOR, which is precisely the failure I4
    describes and precisely what a page-cache-preserving kill cannot show."""
    out = []
    seen = set()
    for oid in HEX.findall(text):
        if oid in seen:
            continue
        seen.add(oid)
        for path, node in dev.ns_c.items():
            if os.path.basename(path) != oid:
                continue
            broken = None
            probe = os.path.dirname(path)
            chain = []
            while probe.startswith(dev.root) and probe != dev.root:
                chain.append(probe)
                probe = os.path.dirname(probe)
            for d in reversed(chain):
                if d not in dev.ns_d:
                    broken = d
                    break
            leaf = "durable" if path in dev.ns_d else "not durable"
            if broken is None:
                out.append("      %s: leaf entry %s, every ancestor durable"
                           % (path, leaf))
            else:
                pend = dev.pend_ns.get(os.path.dirname(broken), [])
                out.append("      %s" % path)
                out.append("        leaf entry %s, but ANCESTOR EDGE %s never "
                           "reached the device (parent %s holds %d unbarriered "
                           "edge(s))"
                           % (leaf, broken, os.path.dirname(broken), len(pend)))
            if node[0] == "f":
                out.append("        file bytes: durable=%d pending_writes=%d"
                           % (len(dev.dur.get(node[1], b"")),
                              len(dev.pend_w.get(node[1], []))))
    return out


def check(forge, out, acks, verbose, cap_source=None):
    """The I4 postcondition, and only that.

    `forge fsck` roots the metadata and walks every object reachable from it:
    that is exactly RECOVERY.md's crash contract -- "a crash must not leave a
    successfully committed ref pointing at a missing or corrupt object".

    `forge fsck --full` additionally roots ORPHAN object files, and a power cut
    inside a publish batch legitimately leaves one: the batch's directory
    barriers are taken per shard, so a commit object's own edge can reach the
    device one barrier before the edge of a tree it names. Nothing points at
    that commit, GC reclaims it, and I4 says nothing about it. It is reported
    here, never counted as a violation. The kill -9 suite asserts `fsck --full`
    clean and gets away with it only because the page cache holds the whole
    batch; this harness must not repeat that conflation in reverse.
    """
    if not os.path.isdir(os.path.join(out, ".forge")):
        # `init` stages into a temporary directory and renames it into place.
        # Before that rename reaches the device there is no repository, so
        # there is no committed ref and I4 claims nothing.
        if verbose:
            print("      pre-init: no durable .forge yet, nothing committed")
        return [], 0
    cap = os.path.join(out, ".forge", "keys", "root.cap")
    if not os.path.exists(cap):
        # A capability is a bearer token the holder keeps; losing the copy
        # inside the repository is not an I4 question. Supply the operator's.
        if cap_source and os.path.exists(cap_source):
            os.makedirs(os.path.dirname(cap), exist_ok=True)
            os.chmod(os.path.dirname(cap), 0o700)
            shutil.copyfile(cap_source, cap)
        else:
            return ["cap absent and no --cap-source given"], 0
    base = [forge, "--dir", out, "--cap", cap]
    bad = []
    fsck = subprocess.run(base + ["fsck"], capture_output=True, text=True)
    if fsck.returncode != 0:
        head = (fsck.stdout + fsck.stderr).strip().splitlines()
        bad.append("fsck (ref-rooted) failed rc=%d: %s"
                   % (fsck.returncode, " | ".join(head[:4])))
    refs = subprocess.run(base + ["refs"], capture_output=True, text=True)
    live = {}
    for line in refs.stdout.splitlines():
        f = line.split()
        if len(f) >= 4:
            live[f[2]] = f[3]
    for ref, oid in acks:
        if live.get(ref) != oid:
            bad.append("ACK LOST ref=%s acked_oid=%s replayed=%s"
                       % (ref, oid, live.get(ref, "<absent>")))
    orphan = 0
    full = subprocess.run(base + ["fsck", "--full"], capture_output=True, text=True)
    if full.returncode != 0:
        orphan = 1
    if verbose and not bad:
        print("      refs=%d acks=%d fsck=clean orphan_partial_batch=%d"
              % (len(live), len(acks), orphan))
    return bad, orphan


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--journal", required=True)
    ap.add_argument("--root", required=True, help="traced repo path (PL_ROOT)")
    ap.add_argument("--baseline", required=True, help="post-init durable image")
    ap.add_argument("--out", required=True, help="scratch dir for replays")
    ap.add_argument("--forge", required=True)
    ap.add_argument("--cuts", type=int, default=16)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--partial", action="store_true",
                    help="also tear an fsync at the cut: only a prefix of the "
                         "effects it covered reached the device")
    ap.add_argument("--cap-source", default=None,
                    help="capability token to supply when the replayed image "
                         "does not carry one; a cap is a bearer token, not a "
                         "durability claim")
    ap.add_argument("--stats", action="store_true")
    args = ap.parse_args()

    recs = load(args.journal)
    if not recs:
        print("PL FAIL: journal is empty -- the shim intercepted nothing")
        return 2
    blobs = Blobs(args.journal)
    rng = random.Random(args.seed)

    marker_idx = [i for i, r in enumerate(recs) if r[1] == OP_MARKER]
    if args.stats:
        c = {}
        for r in recs:
            c[r[1]] = c.get(r[1], 0) + 1
        for op in sorted(c):
            print("op %-18s %d" % (OPNAME.get(op, str(op)), c[op]))
        print("records %d markers %d" % (len(recs), len(marker_idx)))

    # The sharpest cut is one record after an acknowledgement: the promise is
    # outstanding and nothing has had a chance to become durable since.
    cuts = set()
    if marker_idx:
        pick = min(args.cuts, len(marker_idx))
        for i in rng.sample(marker_idx, pick):
            cuts.add(i + 1)
    # With no acknowledgement markers (a single-process workload) the durable
    # catalog is itself the acknowledgement, so spread cuts across the stream.
    extra = args.cuts if not marker_idx else max(0, args.cuts // 2)
    for _ in range(extra):
        cuts.add(rng.randrange(1, len(recs) + 1))
    cuts = sorted(cuts)

    dev = Device(os.path.realpath(args.root))
    dev.load_baseline(args.baseline)
    acks = []
    violations = []
    orphans = 0
    ci = 0
    for i, r in enumerate(recs):
        while ci < len(cuts) and cuts[ci] == i:
            print("  cut %d/%d at record %d (acks=%d)"
                  % (ci + 1, len(cuts), i, len(acks)))
            undo = dev.tear(r, blobs, rng) if args.partial else (lambda: None)
            dev.materialise(args.out)
            bad, orph = check(args.forge, args.out, acks, True, args.cap_source)
            undo()
            orphans += orph
            for b in bad:
                print("    VIOLATION %s" % b)
                for line in explain(dev, b):
                    print(line)
                violations.append((i, b))
            ci += 1
        if r[1] == OP_MARKER:
            payload = blobs.read(r[2], r[5], r[4]).decode("utf-8", "replace")
            for line in payload.splitlines():
                f = line.split()
                if len(f) >= 3 and f[0] == "updated":
                    acks.append((f[1], f[2]))
        else:
            dev.step(r, blobs)

    mm = dev.counts.get(OP_MMAP_W, 0) + dev.counts.get(OP_MSYNC, 0)
    print("PL blindspot-probe shared-writable-mmap=%d msync=%d sync_file_range=%d"
          % (dev.counts.get(OP_MMAP_W, 0), dev.counts.get(OP_MSYNC, 0),
             dev.counts.get(OP_SFR, 0)))
    if violations:
        print("PL RESULT VIOLATION cuts=%d acks=%d violations=%d"
              % (len(cuts), len(acks), len(violations)))
        return 1
    print("PL RESULT CLEAN cuts=%d records=%d acks_replayed=%d "
          "orphan_partial_batch_cuts=%d"
          % (len(cuts), len(recs), len(acks), orphans))
    return 0


if __name__ == "__main__":
    sys.exit(main())
