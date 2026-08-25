/* scripts/w7_fsync_probe.c
 *
 * Optional durability-barrier probe for the docs/BENCH.md W7 comparator.
 *
 * An LD_PRELOAD shim that counts every fsync(2) and fdatasync(2) call made by
 * a process tree and classifies each one as a *file* barrier or a *directory*
 * barrier by fstat()ing the descriptor.
 *
 * It exists because the W7 durability-equivalence gate cannot be settled by
 * reading configuration. `core.fsync` and SQLite `synchronous=FULL` state
 * intent; the gate requires observed barriers. This shim observes them.
 *
 * Build:
 *   cc -shared -fPIC -O2 -o w7_fsync_probe.so scripts/w7_fsync_probe.c -ldl
 * Use:
 *   W7_FSYNC_LOG=/path/log LD_PRELOAD=/path/w7_fsync_probe.so <command>
 *
 * Each barrier appends one "<call> <kind>" line to $W7_FSYNC_LOG, where kind
 * is file, dir, or unknown. O_APPEND writes this small are atomic on Linux,
 * so one log can serve a whole concurrent process tree.
 *
 * Linux/glibc only, and evidence tooling only: it is never linked into,
 * loaded by, or required by the ForgeFS build, tests, or release artifacts.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

static int (*real_fsync)(int);
static int (*real_fdatasync)(int);

static void w7_record(const char *call, int fd)
{
	const char *out = getenv("W7_FSYNC_LOG");
	if (out == NULL || out[0] == 0)
		return;

	const char *kind = "unknown";
	struct stat st;
	if (fstat(fd, &st) == 0)
		kind = S_ISDIR(st.st_mode) ? "dir" : "file";

	/* Opened per record: the probed program may fork, exec, or close fds. */
	int log_fd = open(out, O_WRONLY | O_APPEND | O_CREAT | O_CLOEXEC, 0644);
	if (log_fd < 0)
		return;
	char line[64];
	int n = snprintf(line, sizeof(line), "%s %s\n", call, kind);
	if (n > 0) {
		ssize_t ignored = write(log_fd, line, (size_t)n);
		(void)ignored;
	}
	close(log_fd);
}

int fsync(int fd)
{
	if (real_fsync == NULL)
		real_fsync = (int (*)(int))dlsym(RTLD_NEXT, "fsync");
	w7_record("fsync", fd);
	return real_fsync(fd);
}

int fdatasync(int fd)
{
	if (real_fdatasync == NULL)
		real_fdatasync = (int (*)(int))dlsym(RTLD_NEXT, "fdatasync");
	w7_record("fdatasync", fd);
	return real_fdatasync(fd);
}
