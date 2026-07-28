#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

enum {
    FINAL_CLOSE_WORKERS = 12,
    REUSE_ATTEMPTS = 512,
    RENAME_ITERATIONS = 200,
    DRAIN_STRESS_FILES = 48,
    DRAIN_STRESS_WORKERS = 12,
};

static int write_exact(int fd, const void *buffer, size_t length)
{
    const unsigned char *cursor = buffer;
    while (length > 0) {
        ssize_t written = write(fd, cursor, length);
        if (written <= 0) {
            return -1;
        }
        cursor += written;
        length -= (size_t)written;
    }
    return 0;
}

static int wait_success(pid_t pid)
{
    int status = 0;
    if (waitpid(pid, &status, 0) != pid) {
        return -1;
    }
    return WIFEXITED(status) && WEXITSTATUS(status) == 0 ? 0 : -1;
}

static int create_with_payload(const char *path, const char *payload, struct stat *stat_out)
{
    int fd = open(path, O_CREAT | O_EXCL | O_RDWR, 0600);
    if (fd < 0 || write_exact(fd, payload, strlen(payload)) != 0
        || fsync(fd) != 0 || fstat(fd, stat_out) != 0) {
        if (fd >= 0) {
            close(fd);
        }
        return -1;
    }
    return fd;
}

static int phase_unlink_open_close(const char *base)
{
    char path[512];
    struct stat old_stat;
    struct stat new_stat;
    char data[8] = {0};
    snprintf(path, sizeof(path), "%s/unlink-open", base);
    int old_fd = create_with_payload(path, "old", &old_stat);
    if (old_fd < 0 || unlink(path) != 0 || pread(old_fd, data, 3, 0) != 3
        || memcmp(data, "old", 3) != 0) {
        return -1;
    }
    int new_fd = create_with_payload(path, "new", &new_stat);
    if (new_fd < 0 || new_stat.st_ino == old_stat.st_ino) {
        return -1;
    }
    if (close(old_fd) != 0 || close(new_fd) != 0 || unlink(path) != 0) {
        return -1;
    }
    return 0;
}

static int phase_concurrent_final_close(const char *base)
{
    char path[512];
    int fds[FINAL_CLOSE_WORKERS];
    pid_t children[FINAL_CLOSE_WORKERS];
    int start_pipe[2];
    snprintf(path, sizeof(path), "%s/final-close", base);
    int seed = open(path, O_CREAT | O_EXCL | O_RDWR, 0600);
    if (seed < 0 || write_exact(seed, "pinned", 6) != 0 || close(seed) != 0) {
        return -1;
    }
    for (int i = 0; i < FINAL_CLOSE_WORKERS; ++i) {
        fds[i] = open(path, O_RDONLY);
        if (fds[i] < 0) {
            return -1;
        }
    }
    if (pipe(start_pipe) != 0) {
        return -1;
    }
    for (int i = 0; i < FINAL_CLOSE_WORKERS; ++i) {
        children[i] = fork();
        if (children[i] < 0) {
            return -1;
        }
        if (children[i] == 0) {
            close(start_pipe[1]);
            for (int j = 0; j < FINAL_CLOSE_WORKERS; ++j) {
                if (j != i) {
                    close(fds[j]);
                }
            }
            char token;
            struct stat statbuf;
            if (read(start_pipe[0], &token, 1) != 1 || fstat(fds[i], &statbuf) != 0
                || statbuf.st_nlink != 0 || close(fds[i]) != 0) {
                _exit(31);
            }
            _exit(0);
        }
    }
    close(start_pipe[0]);
    if (unlink(path) != 0) {
        return -1;
    }
    for (int i = 0; i < FINAL_CLOSE_WORKERS; ++i) {
        close(fds[i]);
    }
    for (int i = 0; i < FINAL_CLOSE_WORKERS; ++i) {
        if (write_exact(start_pipe[1], "x", 1) != 0) {
            return -1;
        }
    }
    close(start_pipe[1]);
    for (int i = 0; i < FINAL_CLOSE_WORKERS; ++i) {
        if (wait_success(children[i]) != 0) {
            return -1;
        }
    }
    return access(path, F_OK) < 0 && errno == ENOENT ? 0 : -1;
}

static int phase_fast_inode_reuse(const char *base)
{
    char path[512];
    char payload[32];
    char observed[32];
    ino_t previous = 0;
    int reused = 0;
    snprintf(path, sizeof(path), "%s/reuse", base);
    for (int i = 0; i < REUSE_ATTEMPTS; ++i) {
        struct stat statbuf;
        int length = snprintf(payload, sizeof(payload), "generation-%d", i);
        int fd = create_with_payload(path, payload, &statbuf);
        if (fd < 0 || length <= 0 || (size_t)length >= sizeof(payload)) {
            return -1;
        }
        memset(observed, 0, sizeof(observed));
        if (pread(fd, observed, (size_t)length, 0) != length
            || memcmp(observed, payload, (size_t)length) != 0
            || unlink(path) != 0 || close(fd) != 0) {
            return -1;
        }
        if (previous != 0 && previous == statbuf.st_ino) {
            reused += 1;
        }
        previous = statbuf.st_ino;
    }
    return reused > 0 ? 0 : -1;
}

static int rename_worker(const char *first, const char *second, const char *name)
{
    char from[512];
    char to[512];
    snprintf(from, sizeof(from), "%s/%s", first, name);
    snprintf(to, sizeof(to), "%s/%s", second, name);
    for (int i = 0; i < RENAME_ITERATIONS; ++i) {
        if (rename(from, to) != 0 || rename(to, from) != 0) {
            return -1;
        }
    }
    return 0;
}

static int phase_cross_directory_rename(const char *base)
{
    char left[512];
    char right[512];
    char left_file[1024];
    char right_file[1024];
    snprintf(left, sizeof(left), "%s/left", base);
    snprintf(right, sizeof(right), "%s/right", base);
    snprintf(left_file, sizeof(left_file), "%s/a", left);
    snprintf(right_file, sizeof(right_file), "%s/b", right);
    if (mkdir(left, 0700) != 0 || mkdir(right, 0700) != 0) {
        return -1;
    }
    int fd = open(left_file, O_CREAT | O_EXCL | O_WRONLY, 0600);
    if (fd < 0 || close(fd) != 0) {
        return -1;
    }
    fd = open(right_file, O_CREAT | O_EXCL | O_WRONLY, 0600);
    if (fd < 0 || close(fd) != 0) {
        return -1;
    }
    pid_t forward = fork();
    if (forward == 0) {
        _exit(rename_worker(left, right, "a") == 0 ? 0 : 41);
    }
    pid_t reverse = fork();
    if (reverse == 0) {
        _exit(rename_worker(right, left, "b") == 0 ? 0 : 42);
    }
    if (forward < 0 || reverse < 0 || wait_success(forward) != 0
        || wait_success(reverse) != 0 || unlink(left_file) != 0
        || unlink(right_file) != 0 || rmdir(left) != 0 || rmdir(right) != 0) {
        return -1;
    }
    return 0;
}

static int phase_shutdown_drain_stress(const char *base)
{
    int fds[DRAIN_STRESS_FILES];
    char path[512];
    for (int i = 0; i < DRAIN_STRESS_FILES; ++i) {
        snprintf(path, sizeof(path), "%s/drain-%d", base, i);
        fds[i] = open(path, O_CREAT | O_EXCL | O_RDWR, 0600);
        if (fds[i] < 0 || write_exact(fds[i], "x", 1) != 0 || unlink(path) != 0) {
            return -1;
        }
    }
    int start_pipe[2];
    if (pipe(start_pipe) != 0) {
        return -1;
    }
    pid_t children[DRAIN_STRESS_WORKERS];
    for (int i = 0; i < DRAIN_STRESS_WORKERS; ++i) {
        children[i] = fork();
        if (children[i] < 0) {
            return -1;
        }
        if (children[i] == 0) {
            close(start_pipe[1]);
            for (int j = 0; j < DRAIN_STRESS_FILES; ++j) {
                if (j % DRAIN_STRESS_WORKERS != i) {
                    close(fds[j]);
                }
            }
            char token;
            if (read(start_pipe[0], &token, 1) != 1) {
                _exit(51);
            }
            for (int j = i; j < DRAIN_STRESS_FILES; j += DRAIN_STRESS_WORKERS) {
                if (close(fds[j]) != 0) {
                    _exit(52);
                }
            }
            _exit(0);
        }
    }
    close(start_pipe[0]);
    for (int i = 0; i < DRAIN_STRESS_FILES; ++i) {
        close(fds[i]);
    }
    for (int i = 0; i < DRAIN_STRESS_WORKERS; ++i) {
        if (write_exact(start_pipe[1], "x", 1) != 0) {
            return -1;
        }
    }
    close(start_pipe[1]);
    for (int i = 0; i < DRAIN_STRESS_WORKERS; ++i) {
        if (wait_success(children[i]) != 0) {
            return -1;
        }
    }
    return 0;
}

int main(int argc, char **argv)
{
    if (argc != 2) {
        return 2;
    }
    alarm(45);
    if (mkdir(argv[1], 0700) != 0 && errno != EEXIST) {
        return 2;
    }
#define RUN_CASE(name)                                                            \
    do {                                                                          \
        if (phase_##name(argv[1]) != 0) {                                         \
            puts("FS4_INODE_STATE_CASE_FAIL case=" #name);                       \
            return 1;                                                             \
        }                                                                         \
        puts("FS4_INODE_STATE_CASE_PASS case=" #name);                           \
    } while (0)
    RUN_CASE(unlink_open_close);
    RUN_CASE(concurrent_final_close);
    RUN_CASE(fast_inode_reuse);
    RUN_CASE(cross_directory_rename);
    RUN_CASE(shutdown_drain_stress);
#undef RUN_CASE
    puts("FS4_INODE_STATE_PROBE_PASS cases=5");
    return 0;
}
