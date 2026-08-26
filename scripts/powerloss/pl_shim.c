/*
 * pl_shim.c -- LD_PRELOAD interposer that records the durability-relevant
 * libc call stream of every process in a workload, so a replay tool can
 * reconstruct what a DEVICE would hold after a power cut at any point.
 *
 * SIGKILL destroys a process but leaves the page cache intact, so the
 * filesystem afterwards still contains writes that never reached the device.
 * This shim exists to make those writes visible and droppable.
 *
 * Scope: calls whose path lies under $PL_ROOT (the traced repository), plus
 * writes under $PL_MARK, which are acknowledgement markers ordered into the
 * same global sequence as the write stream.
 *
 * Journal layout, per process, in $PL_JOURNAL_DIR:
 *   j.<pid>  fixed-header records (see struct rec below)
 *   d.<pid>  raw payload bytes referenced by rec.dataoff
 * Global ordering comes from an mmapped 8-byte counter in $PL_JOURNAL_DIR/seq
 * bumped with __atomic_fetch_add, so records from different processes merge
 * into one total order.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <limits.h>
#include <pthread.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/uio.h>

enum {
  OP_WRITE = 1, OP_FSYNC = 2, OP_FDATASYNC = 3, OP_RENAME = 4, OP_LINK = 5,
  OP_UNLINK = 6, OP_MKDIR = 7, OP_RMDIR = 8, OP_CREATE = 9, OP_TRUNCATE = 10,
  OP_MARKER = 11, OP_SYNCFS = 12, OP_MSYNC = 13, OP_SYMLINK = 14,
  OP_WRITE_SYNC = 15, OP_SFR = 16, OP_MMAP_W = 17, OP_CHMOD = 18
};

struct rec {
  uint64_t seq;
  uint32_t op;
  uint32_t pid;
  uint64_t off;
  uint64_t len;
  uint64_t dataoff;
  uint32_t plen1;
  uint32_t plen2;
};

static int (*r_open)(const char *, int, ...);
static int (*r_open64)(const char *, int, ...);
static int (*r_openat)(int, const char *, int, ...);
static int (*r_openat64)(int, const char *, int, ...);
static ssize_t (*r_write)(int, const void *, size_t);
static ssize_t (*r_pwrite)(int, const void *, size_t, off_t);
static ssize_t (*r_pwrite64)(int, const void *, size_t, off64_t);
static ssize_t (*r_writev)(int, const struct iovec *, int);
static ssize_t (*r_pwritev)(int, const struct iovec *, int, off_t);
static int (*r_fsync)(int);
static int (*r_fdatasync)(int);
static int (*r_syncfs)(int);
static int (*r_msync)(void *, size_t, int);
static int (*r_sync_file_range)(int, off64_t, off64_t, unsigned);
static int (*r_rename)(const char *, const char *);
static int (*r_renameat)(int, const char *, int, const char *);
static int (*r_renameat2)(int, const char *, int, const char *, unsigned);
static int (*r_link)(const char *, const char *);
static int (*r_linkat)(int, const char *, int, const char *, int);
static int (*r_symlink)(const char *, const char *);
static int (*r_unlink)(const char *);
static int (*r_unlinkat)(int, const char *, int);
static int (*r_mkdir)(const char *, mode_t);
static int (*r_mkdirat)(int, const char *, mode_t);
static int (*r_rmdir)(const char *);
static int (*r_ftruncate)(int, off_t);
static int (*r_truncate)(const char *, off_t);
static int (*r_close)(int);
static int (*r_dup)(int);
static int (*r_dup2)(int, int);
static int (*r_dup3)(int, int, int);
static void *(*r_mmap)(void *, size_t, int, int, int, off_t);
static void *(*r_mmap64)(void *, size_t, int, int, int, off64_t);
static int (*r_chmod)(const char *, mode_t);
static int (*r_fchmod)(int, mode_t);
static int (*r_fchmodat)(int, const char *, mode_t, int);

#define MAXFD 8192
static char *fdpath[MAXFD];
static unsigned char fdsync[MAXFD];   /* opened O_SYNC / O_DSYNC */
static unsigned char fdmark[MAXFD];   /* acknowledgement marker file */
static unsigned char fdours[MAXFD];   /* our own journal fds */
static unsigned char fdseen[MAXFD];   /* /proc/self/fd already consulted */

static pthread_mutex_t lk = PTHREAD_MUTEX_INITIALIZER;
static __thread int in_shim = 0;

static int jfd = -1, dfd = -1;
static uint64_t *seqp = NULL;
static uint64_t dataoff = 0;
static pid_t mypid = 0;
static const char *pl_root = NULL; static size_t pl_root_len = 0;
static const char *pl_mark = NULL; static size_t pl_mark_len = 0;
static const char *pl_dir = NULL;
static int disabled = 0;

static void resolve(void) {
#define R(n) if (!r_##n) r_##n = dlsym(RTLD_NEXT, #n)
  R(open); R(open64); R(openat); R(openat64);
  R(write); R(pwrite); R(pwrite64); R(writev); R(pwritev);
  R(fsync); R(fdatasync); R(syncfs); R(msync); R(sync_file_range);
  R(rename); R(renameat); R(renameat2); R(link); R(linkat); R(symlink);
  R(unlink); R(unlinkat); R(mkdir); R(mkdirat); R(rmdir);
  R(ftruncate); R(truncate); R(close);
  R(dup); R(dup2); R(dup3); R(mmap); R(mmap64);
  R(chmod); R(fchmod); R(fchmodat);
#undef R
}

/* Open the per-process journal. Re-runs after fork(), because the child
 * inherits the parent's fds but needs its own files. */
static void init(void) {
  if (disabled) return;
  if (mypid == getpid() && jfd >= 0) return;
  resolve();
  /* A shell rewrites its own environ, which can move or free these strings,
   * so keep private copies rather than pointers into environ. */
  const char *e_dir = getenv("PL_JOURNAL_DIR");
  const char *e_root = getenv("PL_ROOT");
  const char *e_mark = getenv("PL_MARK");
  if (!e_dir || !e_root) { disabled = 1; return; }
  pl_dir = strdup(e_dir);
  pl_root = strdup(e_root);
  pl_mark = e_mark ? strdup(e_mark) : NULL;
  if (!pl_dir || !pl_root) { disabled = 1; return; }
  pl_root_len = strlen(pl_root);
  pl_mark_len = pl_mark ? strlen(pl_mark) : 0;
  mypid = getpid();
  char p[PATH_MAX];
  snprintf(p, sizeof p, "%s/seq", pl_dir);
  int sf = r_open64(p, O_RDWR | O_CREAT, 0644);
  if (sf < 0) { disabled = 1; return; }
  if (r_ftruncate(sf, 8) != 0) { /* already sized by another process */ }
  seqp = (uint64_t *)r_mmap(NULL, 8, PROT_READ | PROT_WRITE, MAP_SHARED, sf, 0);
  r_close(sf);
  if (seqp == MAP_FAILED) { disabled = 1; return; }
  snprintf(p, sizeof p, "%s/j.%d", pl_dir, (int)mypid);
  jfd = r_open64(p, O_WRONLY | O_CREAT | O_APPEND, 0644);
  snprintf(p, sizeof p, "%s/d.%d", pl_dir, (int)mypid);
  dfd = r_open64(p, O_WRONLY | O_CREAT | O_APPEND, 0644);
  dataoff = 0;
  if (jfd < 0 || dfd < 0) { disabled = 1; return; }
  if (jfd < MAXFD) { fdours[jfd] = 1; fdseen[jfd] = 1; }
  if (dfd < MAXFD) { fdours[dfd] = 1; fdseen[dfd] = 1; }
}

static int traced(const char *abs) {
  if (!abs || !abs[0]) return 0;
  return !strncmp(abs, pl_root, pl_root_len) &&
         (abs[pl_root_len] == '/' || abs[pl_root_len] == 0);
}
static int marked(const char *abs) {
  if (!abs || !abs[0] || !pl_mark_len) return 0;
  return !strncmp(abs, pl_mark, pl_mark_len) && abs[pl_mark_len] == '/';
}

/* Lexical normalisation to an absolute path with no "." or ".." components.
 * The harness asserts the traced tree contains no symlinks. */
static void norm(const char *in, char *out, size_t n) {
  char buf[PATH_MAX * 2];
  out[0] = 0;
  if (!in) return;
  if (in[0] == '/') snprintf(buf, sizeof buf, "%s", in);
  else {
    char cwd[PATH_MAX];
    if (!getcwd(cwd, sizeof cwd)) return;
    snprintf(buf, sizeof buf, "%s/%s", cwd, in);
  }
  char *seg[256]; int ns = 0; char *sp = NULL;
  for (char *s = strtok_r(buf, "/", &sp); s; s = strtok_r(NULL, "/", &sp)) {
    if (!strcmp(s, ".")) continue;
    if (!strcmp(s, "..")) { if (ns) ns--; continue; }
    if (ns < 256) seg[ns++] = s;
  }
  size_t o = 0;
  for (int i = 0; i < ns && o + 1 < n; i++)
    o += (size_t)snprintf(out + o, n - o, "/%s", seg[i]);
  if (o == 0) snprintf(out, n, "/");
}

static void atpath(int dirfd, const char *path, char *out, size_t n) {
  if (path && path[0] == '/') { norm(path, out, n); return; }
  if (dirfd == AT_FDCWD) { norm(path ? path : ".", out, n); return; }
  const char *base = (dirfd >= 0 && dirfd < MAXFD) ? fdpath[dirfd] : NULL;
  if (!base) { out[0] = 0; return; }
  char buf[PATH_MAX * 2];
  snprintf(buf, sizeof buf, "%s/%s", base, path ? path : "");
  norm(buf, out, n);
}

static void emit(int op, const char *p1, const char *p2,
                 uint64_t off, const void *data, uint64_t len) {
  if (disabled || jfd < 0) return;
  struct rec r;
  size_t l1 = p1 ? strlen(p1) : 0, l2 = p2 ? strlen(p2) : 0;
  if (l1 > PATH_MAX || l2 > PATH_MAX) return;
  pthread_mutex_lock(&lk);
  r.seq = __atomic_fetch_add(seqp, 1, __ATOMIC_SEQ_CST);
  r.op = (uint32_t)op; r.pid = (uint32_t)mypid; r.off = off; r.len = len;
  r.dataoff = 0; r.plen1 = (uint32_t)l1; r.plen2 = (uint32_t)l2;
  if (data && len) {
    r.dataoff = dataoff;
    if (r_write(dfd, data, (size_t)len) == (ssize_t)len) dataoff += len;
    else r.len = 0;
  }
  char out[sizeof(struct rec) + 2 * PATH_MAX + 8];
  memcpy(out, &r, sizeof r);
  size_t o = sizeof r;
  if (l1) { memcpy(out + o, p1, l1); o += l1; }
  if (l2) { memcpy(out + o, p2, l2); o += l2; }
  r_write(jfd, out, o);
  pthread_mutex_unlock(&lk);
}

#define GUARD int _g = in_shim; in_shim = 1
#define UNGUARD in_shim = _g

/* An fd can arrive already open: inherited across execve, or installed by a
 * shell redirection we did not see. Recovering its name from /proc/self/fd
 * the first time it is written to or synced removes that whole blind spot,
 * and is what makes the acknowledgement markers -- written by the shell, not
 * by forge -- land in the same ordered stream as forge's own writes. */
static void adopt(int fd) {
  if (fd < 0 || fd >= MAXFD || fdpath[fd] || fdseen[fd]) return;
  fdseen[fd] = 1;
  char link[64], tgt[PATH_MAX];
  snprintf(link, sizeof link, "/proc/self/fd/%d", fd);
  ssize_t n = readlink(link, tgt, sizeof tgt - 1);
  if (n <= 0) return;
  tgt[n] = 0;
  if (tgt[0] != '/') return;                 /* pipe:, socket:, anon_inode: */
  if (!traced(tgt) && !marked(tgt)) return;
  fdpath[fd] = strdup(tgt);
  fdmark[fd] = (unsigned char)marked(tgt);
  int fl = fcntl(fd, F_GETFL);
  fdsync[fd] = (fl > 0 && (fl & (O_SYNC | O_DSYNC))) ? 1 : 0;
}

/* The replayed image has to be openable, and forge refuses a key directory
 * that is not 0700, so names created inside the trace carry their real
 * resulting permission bits. */
static uint64_t path_mode(const char *p) {
  struct stat st;
  if (stat(p, &st) != 0) return 0;
  return (uint64_t)(st.st_mode & 07777);
}

static void note_open(int fd, const char *abs, int flags, int created) {
  if (fd < 0 || fd >= MAXFD) return;
  free(fdpath[fd]); fdpath[fd] = NULL;
  fdsync[fd] = 0; fdmark[fd] = 0; fdours[fd] = 0; fdseen[fd] = 1;
  int t = traced(abs), m = marked(abs);
  if (!t && !m) return;
  fdpath[fd] = strdup(abs);
  fdmark[fd] = (unsigned char)m;
  fdsync[fd] = (flags & (O_SYNC | O_DSYNC)) ? 1 : 0;
  if (t && created) emit(OP_CREATE, abs, NULL, path_mode(abs), NULL, 0);
  /* O_TRUNC on a file that already existed is a length change like any other
   * and is not durable until the file is fsynced. */
  if (t && !created && (flags & O_TRUNC)) emit(OP_TRUNCATE, abs, NULL, 0, NULL, 0);
}

#define OPEN_BODY(REAL, ABSEXPR)                                              \
  mode_t mode = 0;                                                            \
  if (flags & O_CREAT) { va_list a; va_start(a, flags);                       \
                         mode = (mode_t)va_arg(a, int); va_end(a); }          \
  init();                                                                     \
  if (disabled || in_shim) return REAL;                                       \
  GUARD;                                                                      \
  char abs[PATH_MAX]; ABSEXPR;                                                \
  int pre = (flags & O_CREAT) ? access(abs, F_OK) : 0;                        \
  int fd = REAL;                                                              \
  if (fd >= 0) note_open(fd, abs, flags, (flags & O_CREAT) && pre != 0);      \
  UNGUARD; return fd;

int open(const char *path, int flags, ...) {
  OPEN_BODY(r_open(path, flags, mode), norm(path, abs, sizeof abs));
}
int open64(const char *path, int flags, ...) {
  OPEN_BODY(r_open64(path, flags, mode), norm(path, abs, sizeof abs));
}
int openat(int dirfd, const char *path, int flags, ...) {
  OPEN_BODY(r_openat(dirfd, path, flags, mode),
            atpath(dirfd, path, abs, sizeof abs));
}
int openat64(int dirfd, const char *path, int flags, ...) {
  OPEN_BODY(r_openat64(dirfd, path, flags, mode),
            atpath(dirfd, path, abs, sizeof abs));
}
int creat(const char *path, mode_t mode) {
  return open64(path, O_CREAT | O_WRONLY | O_TRUNC, mode);
}

static void note_write(int fd, const void *buf, size_t n, off_t off, int have_off) {
  if (fd < 0 || fd >= MAXFD) return;
  adopt(fd);
  if (!fdpath[fd] || fdours[fd]) return;
  if (fdmark[fd]) { emit(OP_MARKER, fdpath[fd], NULL, 0, buf, n); return; }
  off_t o = off;
  if (!have_off) {
    o = lseek(fd, 0, SEEK_CUR);
    o = (o < 0) ? 0 : o - (off_t)n;
    if (o < 0) o = 0;
  }
  emit(fdsync[fd] ? OP_WRITE_SYNC : OP_WRITE, fdpath[fd], NULL,
       (uint64_t)o, buf, n);
}

ssize_t write(int fd, const void *buf, size_t n) {
  init(); if (disabled || in_shim) return r_write(fd, buf, n);
  GUARD; ssize_t k = r_write(fd, buf, n);
  if (k > 0) note_write(fd, buf, (size_t)k, 0, 0);
  UNGUARD; return k;
}
ssize_t pwrite(int fd, const void *buf, size_t n, off_t off) {
  init(); if (disabled || in_shim) return r_pwrite(fd, buf, n, off);
  GUARD; ssize_t k = r_pwrite(fd, buf, n, off);
  if (k > 0) note_write(fd, buf, (size_t)k, off, 1);
  UNGUARD; return k;
}
ssize_t pwrite64(int fd, const void *buf, size_t n, off64_t off) {
  init(); if (disabled || in_shim) return r_pwrite64(fd, buf, n, off);
  GUARD; ssize_t k = r_pwrite64(fd, buf, n, off);
  if (k > 0) note_write(fd, buf, (size_t)k, (off_t)off, 1);
  UNGUARD; return k;
}
static void note_iov(int fd, const struct iovec *v, int c, ssize_t k, off_t start) {
  off_t o = start; ssize_t left = k;
  for (int i = 0; i < c && left > 0; i++) {
    size_t take = v[i].iov_len < (size_t)left ? v[i].iov_len : (size_t)left;
    note_write(fd, v[i].iov_base, take, o, 1);
    o += (off_t)take; left -= (ssize_t)take;
  }
}
ssize_t writev(int fd, const struct iovec *v, int c) {
  init(); if (disabled || in_shim) return r_writev(fd, v, c);
  GUARD;
  if (fd >= 0 && fd < MAXFD) adopt(fd);
  int tracked = (fd >= 0 && fd < MAXFD && fdpath[fd]);
  off_t start = tracked ? lseek(fd, 0, SEEK_CUR) : 0;
  ssize_t k = r_writev(fd, v, c);
  if (k > 0 && tracked) note_iov(fd, v, c, k, start < 0 ? 0 : start);
  UNGUARD; return k;
}
ssize_t pwritev(int fd, const struct iovec *v, int c, off_t off) {
  init(); if (disabled || in_shim) return r_pwritev(fd, v, c, off);
  GUARD; ssize_t k = r_pwritev(fd, v, c, off);
  if (k > 0 && fd >= 0 && fd < MAXFD && fdpath[fd]) note_iov(fd, v, c, k, off);
  UNGUARD; return k;
}

#define SYNC_BODY(REAL, OP)                                                   \
  init(); if (disabled || in_shim) return REAL;                               \
  GUARD; int k = REAL;                                                        \
  if (fd >= 0 && fd < MAXFD) adopt(fd);                                       \
  if (k == 0 && fd >= 0 && fd < MAXFD && fdpath[fd] && !fdours[fd] &&         \
      !fdmark[fd])                                                            \
    emit(OP, fdpath[fd], NULL, 0, NULL, 0);                                   \
  UNGUARD; return k;

int fsync(int fd)     { SYNC_BODY(r_fsync(fd), OP_FSYNC); }
int fdatasync(int fd) { SYNC_BODY(r_fdatasync(fd), OP_FDATASYNC); }
int syncfs(int fd)    { SYNC_BODY(r_syncfs(fd), OP_SYNCFS); }

int sync_file_range(int fd, off64_t off, off64_t n, unsigned f) {
  init(); if (disabled || in_shim) return r_sync_file_range(fd, off, n, f);
  GUARD; int k = r_sync_file_range(fd, off, n, f);
  if (k == 0 && fd >= 0 && fd < MAXFD && fdpath[fd] && !fdours[fd])
    emit(OP_SFR, fdpath[fd], NULL, (uint64_t)off, NULL, (uint64_t)n);
  UNGUARD; return k;
}
int msync(void *a, size_t n, int f) {
  init(); if (disabled || in_shim) return r_msync(a, n, f);
  GUARD; int k = r_msync(a, n, f);
  if (k == 0) emit(OP_MSYNC, "<mmap>", NULL, (uint64_t)(uintptr_t)a, NULL, (uint64_t)n);
  UNGUARD; return k;
}
/* A shared writable mapping is the one store this interposer cannot follow:
 * the stores happen in userspace with no syscall at all. Recording that a
 * mapping was CREATED is what lets the harness state the blind spot as a
 * measured number instead of an assumption. */
#define MMAP_BODY(REAL)                                                       \
  init(); if (disabled || in_shim) return REAL;                               \
  GUARD; if (fd >= 0 && fd < MAXFD) adopt(fd);                                \
  void *p = REAL;                                                             \
  if (p != MAP_FAILED && fd >= 0 && fd < MAXFD && fdpath[fd] && !fdours[fd] && \
      (prot & PROT_WRITE) && (flags & MAP_SHARED))                            \
    emit(OP_MMAP_W, fdpath[fd], NULL, (uint64_t)off, NULL, (uint64_t)n);      \
  UNGUARD; return p;

void *mmap(void *a, size_t n, int prot, int flags, int fd, off_t off) {
  MMAP_BODY(r_mmap(a, n, prot, flags, fd, off));
}
void *mmap64(void *a, size_t n, int prot, int flags, int fd, off64_t off) {
  MMAP_BODY(r_mmap64(a, n, prot, flags, fd, off));
}

#define NS2_BODY(REAL, A, B, OP)                                              \
  init(); if (disabled || in_shim) return REAL;                               \
  GUARD; char P1[PATH_MAX], P2[PATH_MAX]; A; B;                               \
  int k = REAL;                                                               \
  if (k == 0 && (traced(P1) || traced(P2))) emit(OP, P1, P2, 0, NULL, 0);     \
  UNGUARD; return k;

int rename(const char *a, const char *b) {
  NS2_BODY(r_rename(a, b), norm(a, P1, sizeof P1), norm(b, P2, sizeof P2), OP_RENAME);
}
int renameat(int da, const char *a, int db, const char *b) {
  NS2_BODY(r_renameat(da, a, db, b), atpath(da, a, P1, sizeof P1),
           atpath(db, b, P2, sizeof P2), OP_RENAME);
}
int renameat2(int da, const char *a, int db, const char *b, unsigned f) {
  NS2_BODY(r_renameat2(da, a, db, b, f), atpath(da, a, P1, sizeof P1),
           atpath(db, b, P2, sizeof P2), OP_RENAME);
}
int link(const char *a, const char *b) {
  NS2_BODY(r_link(a, b), norm(a, P1, sizeof P1), norm(b, P2, sizeof P2), OP_LINK);
}
int linkat(int da, const char *a, int db, const char *b, int fl) {
  NS2_BODY(r_linkat(da, a, db, b, fl), atpath(da, a, P1, sizeof P1),
           atpath(db, b, P2, sizeof P2), OP_LINK);
}
int symlink(const char *t, const char *p) {
  init(); if (disabled || in_shim) return r_symlink(t, p);
  GUARD; char P[PATH_MAX]; norm(p, P, sizeof P);
  int k = r_symlink(t, p);
  if (k == 0 && traced(P)) emit(OP_SYMLINK, P, t, 0, NULL, 0);
  UNGUARD; return k;
}

#define NS1_BODY(REAL, A, OP)                                                 \
  init(); if (disabled || in_shim) return REAL;                               \
  GUARD; char P[PATH_MAX]; A;                                                 \
  int k = REAL;                                                               \
  if (k == 0 && traced(P)) emit(OP, P, NULL, 0, NULL, 0);                     \
  UNGUARD; return k;

int unlink(const char *p) { NS1_BODY(r_unlink(p), norm(p, P, sizeof P), OP_UNLINK); }
int mkdir(const char *p, mode_t m) {
  init(); if (disabled || in_shim) return r_mkdir(p, m);
  GUARD; char P[PATH_MAX]; norm(p, P, sizeof P);
  int k = r_mkdir(p, m);
  if (k == 0 && traced(P)) emit(OP_MKDIR, P, NULL, path_mode(P), NULL, 0);
  UNGUARD; return k;
}
int rmdir(const char *p) { NS1_BODY(r_rmdir(p), norm(p, P, sizeof P), OP_RMDIR); }
int mkdirat(int d, const char *p, mode_t m) {
  init(); if (disabled || in_shim) return r_mkdirat(d, p, m);
  GUARD; char P[PATH_MAX]; atpath(d, p, P, sizeof P);
  int k = r_mkdirat(d, p, m);
  if (k == 0 && traced(P)) emit(OP_MKDIR, P, NULL, path_mode(P), NULL, 0);
  UNGUARD; return k;
}
int unlinkat(int d, const char *p, int fl) {
  init(); if (disabled || in_shim) return r_unlinkat(d, p, fl);
  GUARD; char P[PATH_MAX]; atpath(d, p, P, sizeof P);
  int k = r_unlinkat(d, p, fl);
  if (k == 0 && traced(P))
    emit((fl & AT_REMOVEDIR) ? OP_RMDIR : OP_UNLINK, P, NULL, 0, NULL, 0);
  UNGUARD; return k;
}
int ftruncate(int fd, off_t n) {
  init(); if (disabled || in_shim) return r_ftruncate(fd, n);
  GUARD; if (fd >= 0 && fd < MAXFD) adopt(fd);
  int k = r_ftruncate(fd, n);
  if (k == 0 && fd >= 0 && fd < MAXFD && fdpath[fd] && !fdours[fd] && !fdmark[fd])
    emit(OP_TRUNCATE, fdpath[fd], NULL, 0, NULL, (uint64_t)n);
  UNGUARD; return k;
}
int ftruncate64(int fd, off64_t n) { return ftruncate(fd, (off_t)n); }
int truncate(const char *p, off_t n) {
  init(); if (disabled || in_shim) return r_truncate(p, n);
  GUARD; char P[PATH_MAX]; norm(p, P, sizeof P);
  int k = r_truncate(p, n);
  if (k == 0 && traced(P)) emit(OP_TRUNCATE, P, NULL, 0, NULL, (uint64_t)n);
  UNGUARD; return k;
}
int close(int fd) {
  init(); if (disabled || in_shim) return r_close(fd);
  GUARD;
  if (fd >= 0 && fd < MAXFD) {
    free(fdpath[fd]); fdpath[fd] = NULL;
    fdsync[fd] = fdmark[fd] = fdours[fd] = fdseen[fd] = 0;
  }
  int k = r_close(fd);
  UNGUARD; return k;
}
static void note_dup(int old, int nw) {
  if (old < 0 || old >= MAXFD || nw < 0 || nw >= MAXFD) return;
  free(fdpath[nw]);
  adopt(old);
  fdpath[nw] = fdpath[old] ? strdup(fdpath[old]) : NULL;
  fdsync[nw] = fdsync[old]; fdmark[nw] = fdmark[old]; fdours[nw] = fdours[old];
  fdseen[nw] = 1;
}
/* Rust's create_dir_all mkdirs with the default mode and then chmods, so the
 * mode at mkdir time is not the mode the repository ends up refusing on. */
int chmod(const char *p, mode_t m) {
  init(); if (disabled || in_shim) return r_chmod(p, m);
  GUARD; char P[PATH_MAX]; norm(p, P, sizeof P);
  int k = r_chmod(p, m);
  if (k == 0 && traced(P)) emit(OP_CHMOD, P, NULL, path_mode(P), NULL, 0);
  UNGUARD; return k;
}
int fchmodat(int d, const char *p, mode_t m, int fl) {
  init(); if (disabled || in_shim) return r_fchmodat(d, p, m, fl);
  GUARD; char P[PATH_MAX]; atpath(d, p, P, sizeof P);
  int k = r_fchmodat(d, p, m, fl);
  if (k == 0 && traced(P)) emit(OP_CHMOD, P, NULL, path_mode(P), NULL, 0);
  UNGUARD; return k;
}
int fchmod(int fd, mode_t m) {
  init(); if (disabled || in_shim) return r_fchmod(fd, m);
  GUARD; if (fd >= 0 && fd < MAXFD) adopt(fd);
  int k = r_fchmod(fd, m);
  if (k == 0 && fd >= 0 && fd < MAXFD && fdpath[fd] && !fdours[fd] && !fdmark[fd])
    emit(OP_CHMOD, fdpath[fd], NULL, path_mode(fdpath[fd]), NULL, 0);
  UNGUARD; return k;
}
int dup(int old) {
  init(); if (disabled || in_shim) return r_dup(old);
  GUARD; int k = r_dup(old); if (k >= 0) note_dup(old, k); UNGUARD; return k;
}
int dup2(int old, int nw) {
  init(); if (disabled || in_shim) return r_dup2(old, nw);
  GUARD; int k = r_dup2(old, nw); if (k >= 0) note_dup(old, k); UNGUARD; return k;
}
int dup3(int old, int nw, int fl) {
  init(); if (disabled || in_shim) return r_dup3(old, nw, fl);
  GUARD; int k = r_dup3(old, nw, fl); if (k >= 0) note_dup(old, k); UNGUARD; return k;
}
