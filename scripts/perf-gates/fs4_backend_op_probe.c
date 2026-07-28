#define _GNU_SOURCE
#define _FILE_OFFSET_BITS 64

#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <limits.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

enum probe_mode {
    MODE_INDEPENDENT_FILE,
    MODE_INDEPENDENT_DIR,
    MODE_SAME_INODE,
    MODE_SAME_DIR,
};

static const int WORKER_COUNTS[] = {1, 2, 4, 8};
static const size_t PROBE_READ_SIZE = 512;

static int checked_path(char *dst, size_t len, const char *fmt, const char *base, int worker)
{
    int written = snprintf(dst, len, fmt, base, worker);
    if (written < 0 || (size_t)written >= len) {
        errno = ENAMETOOLONG;
        return -1;
    }
    return 0;
}

static int make_dir(const char *path)
{
    if (mkdir(path, 0755) == 0 || errno == EEXIST) {
        return 0;
    }
    return -1;
}

static int create_file(const char *path, unsigned char seed)
{
    unsigned char data[512];
    memset(data, seed, sizeof(data));
    int fd = open(path, O_CREAT | O_EXCL | O_WRONLY | O_CLOEXEC, 0644);
    if (fd < 0) {
        return -1;
    }
    for (int i = 0; i < 128; ++i) {
        ssize_t written = write(fd, data, sizeof(data));
        if (written != (ssize_t)sizeof(data)) {
            int saved = written < 0 ? errno : EIO;
            close(fd);
            errno = saved;
            return -1;
        }
    }
    if (close(fd) != 0) {
        return -1;
    }
    return 0;
}

static int create_link(const char *target, const char *path)
{
    return symlink(target, path);
}

static int setup_fixtures(const char *base)
{
    char path[PATH_MAX];
    if (make_dir(base) != 0) {
        return -1;
    }
    if (checked_path(path, sizeof(path), "%s/independent", base, 0) != 0 || make_dir(path) != 0) {
        return -1;
    }
    if (checked_path(path, sizeof(path), "%s/same-dir", base, 0) != 0 || make_dir(path) != 0) {
        return -1;
    }
    if (checked_path(path, sizeof(path), "%s/same-inode", base, 0) != 0 || make_dir(path) != 0) {
        return -1;
    }

    char same_file[PATH_MAX];
    char same_link[PATH_MAX];
    if (checked_path(same_file, sizeof(same_file), "%s/same-inode/file", base, 0) != 0 ||
        checked_path(same_link, sizeof(same_link), "%s/same-inode/link", base, 0) != 0 ||
        create_file(same_file, 0x5a) != 0 || create_link("file", same_link) != 0) {
        return -1;
    }

    for (int worker = 0; worker < 8; ++worker) {
        char dir[PATH_MAX];
        char file[PATH_MAX];
        char link[PATH_MAX];
        if (checked_path(dir, sizeof(dir), "%s/independent/worker-%d", base, worker) != 0 ||
            make_dir(dir) != 0 ||
            checked_path(file, sizeof(file), "%s/independent/worker-%d/file", base, worker) != 0 ||
            checked_path(link, sizeof(link), "%s/independent/worker-%d/link", base, worker) != 0 ||
            create_file(file, (unsigned char)(worker + 1)) != 0 || create_link("file", link) != 0) {
            return -1;
        }
        if (checked_path(file, sizeof(file), "%s/same-dir/file-%d", base, worker) != 0 ||
            checked_path(link, sizeof(link), "%s/same-dir/link-%d", base, worker) != 0 ||
            create_file(file, (unsigned char)(worker + 17)) != 0) {
            return -1;
        }
        char target[32];
        int target_len = snprintf(target, sizeof(target), "file-%d", worker);
        if (target_len < 0 || (size_t)target_len >= sizeof(target) || create_link(target, link) != 0) {
            return -1;
        }
    }
    return 0;
}

static int direct_iteration(const char *file, const char *link)
{
    struct stat st;
    unsigned char data[512];
    char target[64];
    if (fstatat(AT_FDCWD, file, &st, 0) != 0) {
        return -1;
    }
    int fd = openat(AT_FDCWD, file, O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        return -1;
    }
    ssize_t read_len = pread(fd, data, PROBE_READ_SIZE, 0);
    int close_result = close(fd);
    if (read_len != (ssize_t)PROBE_READ_SIZE || close_result != 0) {
        errno = read_len < 0 ? errno : EIO;
        return -1;
    }
    if (readlinkat(AT_FDCWD, link, target, sizeof(target)) <= 0) {
        return -1;
    }
    return 0;
}

static int directory_iteration(const char *dir)
{
    struct stat st;
    unsigned char data[512];
    char target[64];
    int dirfd = openat(AT_FDCWD, dir, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    if (dirfd < 0) {
        return -1;
    }
    if (fstatat(dirfd, "file", &st, 0) != 0) {
        close(dirfd);
        return -1;
    }
    int fd = openat(dirfd, "file", O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        close(dirfd);
        return -1;
    }
    ssize_t read_len = pread(fd, data, PROBE_READ_SIZE, 0);
    int file_close = close(fd);
    ssize_t link_len = readlinkat(dirfd, "link", target, sizeof(target));
    int dir_close = close(dirfd);
    if (read_len != (ssize_t)PROBE_READ_SIZE || file_close != 0 || link_len <= 0 || dir_close != 0) {
        errno = read_len < 0 || link_len < 0 ? errno : EIO;
        return -1;
    }
    return 0;
}

static int worker_loop(enum probe_mode mode, const char *base, int worker, int iterations)
{
    char file[PATH_MAX];
    char link[PATH_MAX];
    char dir[PATH_MAX];
    switch (mode) {
    case MODE_INDEPENDENT_FILE:
        if (checked_path(file, sizeof(file), "%s/independent/worker-%d/file", base, worker) != 0 ||
            checked_path(link, sizeof(link), "%s/independent/worker-%d/link", base, worker) != 0) {
            return -1;
        }
        break;
    case MODE_INDEPENDENT_DIR:
        if (checked_path(dir, sizeof(dir), "%s/independent/worker-%d", base, worker) != 0) {
            return -1;
        }
        break;
    case MODE_SAME_INODE:
        if (checked_path(file, sizeof(file), "%s/same-inode/file", base, worker) != 0 ||
            checked_path(link, sizeof(link), "%s/same-inode/link", base, worker) != 0) {
            return -1;
        }
        break;
    case MODE_SAME_DIR:
        if (checked_path(file, sizeof(file), "%s/same-dir/file-%d", base, worker) != 0 ||
            checked_path(link, sizeof(link), "%s/same-dir/link-%d", base, worker) != 0) {
            return -1;
        }
        break;
    }

    for (int iteration = 0; iteration < iterations; ++iteration) {
        int result = mode == MODE_INDEPENDENT_DIR ? directory_iteration(dir)
                                                  : direct_iteration(file, link);
        if (result != 0) {
            return -1;
        }
    }
    return 0;
}

static uint64_t monotonic_ns(void)
{
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) {
        return 0;
    }
    return (uint64_t)now.tv_sec * UINT64_C(1000000000) + (uint64_t)now.tv_nsec;
}

static int write_bytes(int fd, int count)
{
    char bytes[8] = {0};
    ssize_t written = write(fd, bytes, (size_t)count);
    return written == count ? 0 : -1;
}

static int read_bytes(int fd, int count)
{
    char bytes[8];
    int received = 0;
    while (received < count) {
        ssize_t amount = read(fd, bytes + received, (size_t)(count - received));
        if (amount <= 0) {
            return -1;
        }
        received += (int)amount;
    }
    return 0;
}

static int run_cell(enum probe_mode mode, const char *mode_name, const char *base, int workers,
                    int iterations)
{
    int ready_pipe[2];
    int start_pipe[2];
    if (pipe2(ready_pipe, O_CLOEXEC) != 0 || pipe2(start_pipe, O_CLOEXEC) != 0) {
        return -1;
    }
    pid_t children[8];
    for (int worker = 0; worker < workers; ++worker) {
        pid_t child = fork();
        if (child < 0) {
            return -1;
        }
        if (child == 0) {
            close(ready_pipe[0]);
            close(start_pipe[1]);
            char token = 0;
            if (write(ready_pipe[1], &token, 1) != 1 || read(start_pipe[0], &token, 1) != 1) {
                _exit(120);
            }
            int result = worker_loop(mode, base, worker, iterations);
            _exit(result == 0 ? 0 : 121);
        }
        children[worker] = child;
    }
    close(ready_pipe[1]);
    close(start_pipe[0]);
    if (read_bytes(ready_pipe[0], workers) != 0) {
        return -1;
    }
    uint64_t start = monotonic_ns();
    if (start == 0 || write_bytes(start_pipe[1], workers) != 0) {
        return -1;
    }
    close(start_pipe[1]);
    close(ready_pipe[0]);

    int errors = 0;
    for (int worker = 0; worker < workers; ++worker) {
        int status = 0;
        if (waitpid(children[worker], &status, 0) != children[worker] || !WIFEXITED(status) ||
            WEXITSTATUS(status) != 0) {
            ++errors;
        }
    }
    uint64_t end = monotonic_ns();
    if (end <= start) {
        return -1;
    }
    uint64_t elapsed = end - start;
    uint64_t operations = (uint64_t)workers * (uint64_t)iterations * UINT64_C(4);
    uint64_t throughput = operations * UINT64_C(1000000000) / elapsed;
    printf("FS4_BACKEND_OP_CELL mode=%s workers=%d iterations=%d operations=%" PRIu64
           " elapsed_ns=%" PRIu64 " throughput_ops_per_s=%" PRIu64 " errors=%d\n",
           mode_name, workers, iterations, operations, elapsed, throughput, errors);
    fflush(stdout);
    return errors == 0 ? 0 : -1;
}

static int run_probe(const char *base, int iterations)
{
    long online = sysconf(_SC_NPROCESSORS_ONLN);
    if (online < 1) {
        return -1;
    }
    printf("FS4_BACKEND_OP_START online_cpus=%ld iterations=%d\n", online, iterations);
    const struct {
        enum probe_mode mode;
        const char *name;
    } modes[] = {
        {MODE_INDEPENDENT_FILE, "independent_file"},
        {MODE_INDEPENDENT_DIR, "independent_dir"},
        {MODE_SAME_INODE, "same_inode"},
        {MODE_SAME_DIR, "same_dir"},
    };
    for (size_t mode = 0; mode < sizeof(modes) / sizeof(modes[0]); ++mode) {
        for (size_t cell = 0; cell < sizeof(WORKER_COUNTS) / sizeof(WORKER_COUNTS[0]); ++cell) {
            int workers = WORKER_COUNTS[cell];
            if (workers > online) {
                continue;
            }
            if (run_cell(modes[mode].mode, modes[mode].name, base, workers, iterations) != 0) {
                return -1;
            }
        }
    }
    puts("FS4_BACKEND_OP_DONE ok=1");
    return 0;
}

int main(int argc, char **argv)
{
    if (argc < 3) {
        fprintf(stderr, "usage: %s setup|run BASE [ITERATIONS]\n", argv[0]);
        return 2;
    }
    if (strcmp(argv[1], "setup") == 0) {
        if (setup_fixtures(argv[2]) != 0) {
            perror("fs4 probe setup");
            return 1;
        }
        puts("FS4_BACKEND_OP_SETUP ok=1");
        return 0;
    }
    if (strcmp(argv[1], "run") != 0 || argc != 4) {
        return 2;
    }
    char *end = NULL;
    long iterations = strtol(argv[3], &end, 10);
    if (end == argv[3] || *end != '\0' || iterations < 1 || iterations > INT_MAX) {
        return 2;
    }
    if (run_probe(argv[2], (int)iterations) != 0) {
        perror("fs4 probe run");
        return 1;
    }
    return 0;
}
