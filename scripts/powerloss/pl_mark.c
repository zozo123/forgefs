/*
 * pl_mark.c -- append one acknowledgement line, using write(2) directly.
 *
 * The shell cannot be used for this. A shell builtin's redirected output goes
 * through glibc stdio, and stdio flushes through a libc-INTERNAL alias of
 * write that no LD_PRELOAD object can interpose. The acknowledgement would
 * then land on disk but not in the ordered stream, and the harness would not
 * know which refs had been promised at a cut point.
 *
 *   pl_mark <file> <line>
 */
#define _GNU_SOURCE
#include <fcntl.h>
#include <string.h>
#include <unistd.h>

int main(int argc, char **argv) {
  if (argc < 3) return 2;
  int fd = open(argv[1], O_WRONLY | O_CREAT | O_APPEND, 0644);
  if (fd < 0) return 1;
  size_t n = strlen(argv[2]);
  char buf[8192];
  if (n > sizeof buf - 2) n = sizeof buf - 2;
  memcpy(buf, argv[2], n);
  buf[n] = '\n';
  ssize_t w = write(fd, buf, n + 1);
  close(fd);
  return w == (ssize_t)(n + 1) ? 0 : 1;
}
