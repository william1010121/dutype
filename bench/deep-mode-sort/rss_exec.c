#define _GNU_SOURCE
#include <sys/resource.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>
#include <stdio.h>
// Linux-only individual-child peak RSS, no elapsed-time measurement.
int main(int argc, char **argv) {
    if (argc < 2) { fprintf(stderr, "usage: rss_exec binary [args...]\n"); return 2; }
    pid_t pid = fork();
    if (pid < 0) { perror("fork"); return 2; }
    if (pid == 0) { execv(argv[1], argv + 1); perror("execv"); _exit(127); }
    int status;
    struct rusage usage;
    if (wait4(pid, &status, 0, &usage) < 0) { perror("wait4"); return 2; }
    fprintf(stderr, "{\"rss_provider\":\"wait4-rusage\",\"max_rss_kib\":%ld}\n", usage.ru_maxrss);
    if (WIFEXITED(status)) return WEXITSTATUS(status);
    if (WIFSIGNALED(status)) return 128 + WTERMSIG(status);
    return 2;
}
