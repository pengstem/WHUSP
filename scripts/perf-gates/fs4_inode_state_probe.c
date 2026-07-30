#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <signal.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

enum {
    FINAL_CLOSE_WORKERS = 12,
    REUSE_ATTEMPTS = 512,
    RENAME_ITERATIONS = 200,
    READ_MUTATION_ITERATIONS = 200,
    DRAIN_STRESS_FILES = 48,
    DRAIN_STRESS_WORKERS = 12,
    DIRECTORY_SNAPSHOT_FILES = 96,
    DIRECTORY_RACE_STABLE_FILES = 24,
    MAPPED_WRITE_BLOCK = 4096,
    MAPPED_WRITE_CHUNK = 64 * 1024,
    MAPPED_WRITE_FILE = 4 * MAPPED_WRITE_CHUNK,
    MAPPED_WRITE_WORKERS = 8,
    MAPPED_WRITE_ITERATIONS = 12,
    SEQUENCE_READERS = 4,
    SEQUENCE_WRITERS = 4,
    SEQUENCE_FILE_SIZE = 1024 * 1024,
    SEQUENCE_WRITE_CHUNK = 64 * 1024,
    SEQUENCE_READ_ITERATIONS = 4,
    SEQUENCE_WRITE_ITERATIONS = 32,
    INODE_METADATA_WORKERS = 8,
    INODE_METADATA_ITERATIONS = 1024,
    CREATE_WORKERS = 8,
    CREATE_ITERATIONS = 32,
};

struct probe_linux_dirent64 {
    uint64_t d_ino;
    int64_t d_off;
    unsigned short d_reclen;
    unsigned char d_type;
    char d_name[];
};

static int write_exact(int fd, const void *buffer, size_t length);
static int wait_success(pid_t pid);

static uint64_t monotonic_ns(void)
{
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) {
        return 0;
    }
    return (uint64_t)now.tv_sec * UINT64_C(1000000000) + (uint64_t)now.tv_nsec;
}

static int read_tokens(int fd, int count)
{
    char tokens[8];
    int received = 0;
    while (received < count) {
        ssize_t amount = read(fd, tokens + received, (size_t)(count - received));
        if (amount <= 0) {
            return -1;
        }
        received += (int)amount;
    }
    return 0;
}

static int phase_lookup_stat_vs_namespace_mutation(const char *base)
{
    char dir[512];
    char stable[1024];
    char changing[1024];
    snprintf(dir, sizeof(dir), "%s/namespace-race", base);
    snprintf(stable, sizeof(stable), "%s/stable", dir);
    snprintf(changing, sizeof(changing), "%s/changing", dir);
    if (mkdir(dir, 0700) != 0) {
        return -1;
    }
    int fd = open(stable, O_CREAT | O_EXCL | O_WRONLY, 0600);
    if (fd < 0 || write_exact(fd, "stable", 6) != 0 || close(fd) != 0) {
        return -1;
    }
    pid_t writer = fork();
    if (writer == 0) {
        for (int i = 0; i < READ_MUTATION_ITERATIONS; ++i) {
            int changing_fd = open(changing, O_CREAT | O_EXCL | O_WRONLY, 0600);
            if (changing_fd < 0 || close(changing_fd) != 0 || unlink(changing) != 0) {
                _exit(61);
            }
        }
        _exit(0);
    }
    pid_t reader = fork();
    if (reader == 0) {
        for (int i = 0; i < READ_MUTATION_ITERATIONS * 4; ++i) {
            struct stat statbuf;
            if (stat(stable, &statbuf) != 0 || !S_ISREG(statbuf.st_mode)
                || statbuf.st_size != 6) {
                _exit(62);
            }
            if (stat(changing, &statbuf) != 0 && errno != ENOENT) {
                _exit(63);
            }
        }
        _exit(0);
    }
    if (writer < 0 || reader < 0 || wait_success(writer) != 0 || wait_success(reader) != 0
        || unlink(stable) != 0 || rmdir(dir) != 0) {
        return -1;
    }
    return 0;
}

static int phase_read_vs_mapping_mutation(const char *base)
{
    char path[512];
    unsigned char payload[4096];
    snprintf(path, sizeof(path), "%s/mapping-race", base);
    memset(payload, 0x5a, sizeof(payload));
    int fd = open(path, O_CREAT | O_EXCL | O_RDWR, 0600);
    if (fd < 0 || write_exact(fd, payload, sizeof(payload)) != 0 || close(fd) != 0) {
        return -1;
    }
    pid_t writer = fork();
    if (writer == 0) {
        int write_fd = open(path, O_RDWR);
        if (write_fd < 0) {
            _exit(71);
        }
        for (int i = 0; i < READ_MUTATION_ITERATIONS; ++i) {
            memset(payload, i & 1 ? 0xa5 : 0x5a, sizeof(payload));
            if (pwrite(write_fd, payload, sizeof(payload), 0) != (ssize_t)sizeof(payload)
                || ftruncate(write_fd, i & 1 ? 2048 : 4096) != 0) {
                _exit(72);
            }
        }
        if (fsync(write_fd) != 0 || close(write_fd) != 0) {
            _exit(73);
        }
        _exit(0);
    }
    pid_t reader = fork();
    if (reader == 0) {
        unsigned char observed[4096];
        int read_fd = open(path, O_RDONLY);
        if (read_fd < 0) {
            _exit(74);
        }
        for (int i = 0; i < READ_MUTATION_ITERATIONS * 4; ++i) {
            struct stat statbuf;
            ssize_t amount = pread(read_fd, observed, sizeof(observed), 0);
            if (amount < 0 || amount > (ssize_t)sizeof(observed) || fstat(read_fd, &statbuf) != 0
                || statbuf.st_size < 0 || statbuf.st_size > (off_t)sizeof(observed)) {
                _exit(75);
            }
        }
        if (close(read_fd) != 0) {
            _exit(76);
        }
        _exit(0);
    }
    if (writer < 0 || reader < 0 || wait_success(writer) != 0 || wait_success(reader) != 0
        || unlink(path) != 0) {
        return -1;
    }
    return 0;
}

static int phase_partial_read_plan(const char *base)
{
    enum { PAYLOAD_LEN = 3 * 4096 + 333 };
    char path[512];
    unsigned char payload[PAYLOAD_LEN];
    unsigned char observed[5000];
    snprintf(path, sizeof(path), "%s/partial-read-plan", base);
    for (size_t i = 0; i < sizeof(payload); ++i) {
        payload[i] = (unsigned char)(i * 37u + 11u);
    }
    int fd = open(path, O_CREAT | O_EXCL | O_RDWR, 0600);
    if (fd < 0 || write_exact(fd, payload, sizeof(payload)) != 0 || fsync(fd) != 0) {
        return -1;
    }
    if (pread(fd, observed, sizeof(observed), 37) != (ssize_t)sizeof(observed)
        || memcmp(observed, payload + 37, sizeof(observed)) != 0) {
        return -1;
    }
    if (pread(fd, observed, 511, 4093) != 511
        || memcmp(observed, payload + 4093, 511) != 0) {
        return -1;
    }
    memset(observed, 0, sizeof(observed));
    if (pread(fd, observed, 1000, PAYLOAD_LEN - 333) != 333
        || memcmp(observed, payload + PAYLOAD_LEN - 333, 333) != 0
        || pread(fd, observed, 1, PAYLOAD_LEN) != 0
        || close(fd) != 0 || unlink(path) != 0) {
        return -1;
    }
    return 0;
}

static int phase_mapped_overwrite_plan(const char *base)
{
    enum { PAYLOAD_LEN = 4 * MAPPED_WRITE_BLOCK };
    char mapped[512];
    char sparse[512];
    unsigned char expected[PAYLOAD_LEN];
    unsigned char observed[PAYLOAD_LEN];
    unsigned char full[MAPPED_WRITE_BLOCK];
    unsigned char partial[777];
    snprintf(mapped, sizeof(mapped), "%s/mapped-overwrite", base);
    snprintf(sparse, sizeof(sparse), "%s/mapped-overwrite-sparse", base);
    for (size_t i = 0; i < sizeof(expected); ++i) {
        expected[i] = (unsigned char)(i * 29u + 7u);
    }
    int fd = open(mapped, O_CREAT | O_EXCL | O_RDWR, 0600);
    if (fd < 0 || write_exact(fd, expected, sizeof(expected)) != 0 || fsync(fd) != 0
        || close(fd) != 0) {
        return -1;
    }

    memset(full, 0xb6, sizeof(full));
    memset(partial, 0x3d, sizeof(partial));
    fd = open(mapped, O_RDWR | O_SYNC);
    if (fd < 0 || pwrite(fd, full, sizeof(full), MAPPED_WRITE_BLOCK) != (ssize_t)sizeof(full)
        || pwrite(fd, partial, sizeof(partial), 2 * MAPPED_WRITE_BLOCK + 123)
               != (ssize_t)sizeof(partial)
        || close(fd) != 0) {
        return -1;
    }
    memcpy(expected + MAPPED_WRITE_BLOCK, full, sizeof(full));
    memcpy(expected + 2 * MAPPED_WRITE_BLOCK + 123, partial, sizeof(partial));
    fd = open(mapped, O_RDONLY);
    if (fd < 0 || pread(fd, observed, sizeof(observed), 0) != (ssize_t)sizeof(observed)
        || memcmp(observed, expected, sizeof(expected)) != 0 || close(fd) != 0) {
        return -1;
    }

    // Same-size writes into a hole and writes past EOF must reject the plan
    // and retain the allocating/extending lwext4 transaction path.
    struct stat sparse_stat;
    fd = open(sparse, O_CREAT | O_EXCL | O_RDWR | O_SYNC, 0600);
    if (fd < 0 || ftruncate(fd, 3 * MAPPED_WRITE_BLOCK) != 0
        || pwrite(fd, partial, sizeof(partial), MAPPED_WRITE_BLOCK + 91)
               != (ssize_t)sizeof(partial)
        || pwrite(fd, full, sizeof(full), 3 * MAPPED_WRITE_BLOCK) != (ssize_t)sizeof(full)
        || fstat(fd, &sparse_stat) != 0 || sparse_stat.st_size != PAYLOAD_LEN || close(fd) != 0) {
        return -1;
    }
    fd = open(sparse, O_RDONLY);
    memset(observed, 0xa5, sizeof(observed));
    memset(expected, 0, sizeof(expected));
    memcpy(expected + MAPPED_WRITE_BLOCK + 91, partial, sizeof(partial));
    memcpy(expected + 3 * MAPPED_WRITE_BLOCK, full, sizeof(full));
    if (fd < 0 || pread(fd, observed, sizeof(observed), 0) != (ssize_t)sizeof(observed)
        || memcmp(observed, expected, sizeof(expected)) != 0
        || close(fd) != 0 || unlink(mapped) != 0 || unlink(sparse) != 0) {
        return -1;
    }
    return 0;
}

static int mapped_overwrite_worker(const char *path, int worker, int iterations)
{
    unsigned char *payload = malloc(MAPPED_WRITE_CHUNK);
    if (payload == NULL) {
        return -1;
    }
    int fd = open(path, O_RDWR | O_SYNC);
    if (fd < 0) {
        free(payload);
        return -1;
    }
    for (int iteration = 0; iteration < iterations; ++iteration) {
        memset(payload, worker * 17 + iteration, MAPPED_WRITE_CHUNK);
        off_t offset = (iteration % 4) * MAPPED_WRITE_CHUNK;
        if (pwrite(fd, payload, MAPPED_WRITE_CHUNK, offset) != MAPPED_WRITE_CHUNK) {
            close(fd);
            free(payload);
            return -1;
        }
    }
    int result = close(fd);
    free(payload);
    return result;
}

static int run_mapped_overwrite_cell(const char *base, int workers)
{
    int ready_pipe[2];
    int start_pipe[2];
    pid_t children[MAPPED_WRITE_WORKERS];
    if (pipe(ready_pipe) != 0 || pipe(start_pipe) != 0) {
        return -1;
    }
    for (int worker = 0; worker < workers; ++worker) {
        pid_t child = fork();
        if (child < 0) {
            return -1;
        }
        if (child == 0) {
            char path[512];
            char token;
            close(ready_pipe[0]);
            close(start_pipe[1]);
            snprintf(path, sizeof(path), "%s/mapped-worker-%d", base, worker);
            if (write(ready_pipe[1], "r", 1) != 1 || read(start_pipe[0], &token, 1) != 1) {
                _exit(101);
            }
            _exit(mapped_overwrite_worker(path, worker, MAPPED_WRITE_ITERATIONS) == 0 ? 0 : 102);
        }
        children[worker] = child;
    }
    close(ready_pipe[1]);
    close(start_pipe[0]);
    if (read_tokens(ready_pipe[0], workers) != 0) {
        return -1;
    }
    uint64_t start = monotonic_ns();
    if (start == 0) {
        return -1;
    }
    for (int worker = 0; worker < workers; ++worker) {
        if (write(start_pipe[1], "s", 1) != 1) {
            return -1;
        }
    }
    close(ready_pipe[0]);
    close(start_pipe[1]);
    int errors = 0;
    for (int worker = 0; worker < workers; ++worker) {
        if (wait_success(children[worker]) != 0) {
            ++errors;
        }
    }
    uint64_t end = monotonic_ns();
    if (end <= start) {
        return -1;
    }
    uint64_t bytes = (uint64_t)workers * MAPPED_WRITE_ITERATIONS * MAPPED_WRITE_CHUNK;
    uint64_t throughput = bytes * UINT64_C(1000000000) / (end - start);
    printf("FS4_MAPPED_WRITE_CELL workers=%d iterations=%d bytes=%" PRIu64
           " elapsed_ns=%" PRIu64 " throughput_bytes_per_s=%" PRIu64 " errors=%d\n",
           workers, MAPPED_WRITE_ITERATIONS, bytes, end - start, throughput, errors);
    fflush(stdout);
    return errors == 0 ? 0 : -1;
}

static int phase_independent_mapped_overwrite(const char *base)
{
    unsigned char *payload = malloc(MAPPED_WRITE_FILE);
    if (payload == NULL) {
        return -1;
    }
    memset(payload, 0x51, MAPPED_WRITE_FILE);
    for (int worker = 0; worker < MAPPED_WRITE_WORKERS; ++worker) {
        char path[512];
        snprintf(path, sizeof(path), "%s/mapped-worker-%d", base, worker);
        int fd = open(path, O_CREAT | O_EXCL | O_RDWR, 0600);
        if (fd < 0 || write_exact(fd, payload, MAPPED_WRITE_FILE) != 0 || fsync(fd) != 0
            || close(fd) != 0) {
            free(payload);
            return -1;
        }
    }
    free(payload);
    for (int workers = 1; workers <= MAPPED_WRITE_WORKERS; workers *= 2) {
        if (run_mapped_overwrite_cell(base, workers) != 0) {
            return -1;
        }
    }
    for (int worker = 0; worker < MAPPED_WRITE_WORKERS; ++worker) {
        char path[512];
        snprintf(path, sizeof(path), "%s/mapped-worker-%d", base, worker);
        if (unlink(path) != 0) {
            return -1;
        }
    }
    return 0;
}

static int sequence_reader_worker(const char *path)
{
    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        return -1;
    }
    for (int iteration = 0; iteration < SEQUENCE_READ_ITERATIONS; ++iteration) {
        off_t hole = lseek(fd, 0, SEEK_HOLE);
        if (hole != SEQUENCE_FILE_SIZE) {
            close(fd);
            return -1;
        }
    }
    return close(fd);
}

static int sequence_writer_worker(const char *path, int worker)
{
    unsigned char *payload = malloc(SEQUENCE_WRITE_CHUNK);
    if (payload == NULL) {
        return -1;
    }
    int fd = open(path, O_RDWR | O_SYNC);
    if (fd < 0) {
        free(payload);
        return -1;
    }
    for (int iteration = 0; iteration < SEQUENCE_WRITE_ITERATIONS; ++iteration) {
        memset(payload, worker * 29 + iteration + 1, SEQUENCE_WRITE_CHUNK);
        off_t offset = (iteration * SEQUENCE_WRITE_CHUNK) % SEQUENCE_FILE_SIZE;
        if (pwrite(fd, payload, SEQUENCE_WRITE_CHUNK, offset) != SEQUENCE_WRITE_CHUNK) {
            close(fd);
            free(payload);
            return -1;
        }
    }
    int result = close(fd);
    free(payload);
    return result;
}

static int phase_disjoint_sequence_conflict(const char *base)
{
    enum { WORKERS = SEQUENCE_READERS + SEQUENCE_WRITERS };
    unsigned char *payload = malloc(SEQUENCE_FILE_SIZE);
    int ready_pipe[2];
    int start_pipe[2];
    pid_t children[WORKERS];
    if (payload == NULL || pipe(ready_pipe) != 0 || pipe(start_pipe) != 0) {
        free(payload);
        return -1;
    }
    memset(payload, 0x5d, SEQUENCE_FILE_SIZE);
    for (int worker = 0; worker < WORKERS; ++worker) {
        char path[512];
        snprintf(path, sizeof(path), "%s/sequence-%s-%d", base,
                 worker < SEQUENCE_READERS ? "reader" : "writer", worker);
        int fd = open(path, O_CREAT | O_EXCL | O_RDWR, 0600);
        if (fd < 0 || write_exact(fd, payload, SEQUENCE_FILE_SIZE) != 0 || fsync(fd) != 0
            || close(fd) != 0) {
            free(payload);
            return -1;
        }
    }
    free(payload);

    for (int worker = 0; worker < WORKERS; ++worker) {
        pid_t child = fork();
        if (child < 0) {
            return -1;
        }
        if (child == 0) {
            char path[512];
            char token;
            close(ready_pipe[0]);
            close(start_pipe[1]);
            snprintf(path, sizeof(path), "%s/sequence-%s-%d", base,
                     worker < SEQUENCE_READERS ? "reader" : "writer", worker);
            if (write(ready_pipe[1], "r", 1) != 1 || read(start_pipe[0], &token, 1) != 1) {
                _exit(131);
            }
            if (worker < SEQUENCE_READERS) {
                _exit(sequence_reader_worker(path) == 0 ? 0 : 132);
            }
            _exit(sequence_writer_worker(path, worker - SEQUENCE_READERS) == 0 ? 0 : 133);
        }
        children[worker] = child;
    }
    close(ready_pipe[1]);
    close(start_pipe[0]);
    if (read_tokens(ready_pipe[0], WORKERS) != 0) {
        return -1;
    }
    uint64_t start = monotonic_ns();
    if (start == 0) {
        return -1;
    }
    for (int worker = 0; worker < WORKERS; ++worker) {
        if (write(start_pipe[1], "s", 1) != 1) {
            return -1;
        }
    }
    close(ready_pipe[0]);
    close(start_pipe[1]);
    int errors = 0;
    for (int worker = 0; worker < WORKERS; ++worker) {
        if (wait_success(children[worker]) != 0) {
            ++errors;
        }
    }
    uint64_t end = monotonic_ns();
    if (end <= start) {
        return -1;
    }
    for (int worker = 0; worker < WORKERS; ++worker) {
        char path[512];
        snprintf(path, sizeof(path), "%s/sequence-%s-%d", base,
                 worker < SEQUENCE_READERS ? "reader" : "writer", worker);
        if (unlink(path) != 0) {
            return -1;
        }
    }
    printf("FS4_SEQUENCE_CELL readers=%d reader_iterations=%d reader_bytes=%d "
           "writers=%d writer_iterations=%d writer_bytes=%d elapsed_ns=%" PRIu64
           " errors=%d\n",
           SEQUENCE_READERS, SEQUENCE_READ_ITERATIONS,
           SEQUENCE_READERS * SEQUENCE_READ_ITERATIONS * SEQUENCE_FILE_SIZE,
           SEQUENCE_WRITERS, SEQUENCE_WRITE_ITERATIONS,
           SEQUENCE_WRITERS * SEQUENCE_WRITE_ITERATIONS * SEQUENCE_WRITE_CHUNK,
           end - start, errors);
    fflush(stdout);
    return errors == 0 ? 0 : -1;
}

static int run_inode_metadata_cell(const char *base, int workers)
{
    int ready_pipe[2];
    int start_pipe[2];
    pid_t children[INODE_METADATA_WORKERS];
    if (pipe(ready_pipe) != 0 || pipe(start_pipe) != 0) {
        return -1;
    }
    for (int worker = 0; worker < workers; ++worker) {
        pid_t child = fork();
        if (child < 0) {
            return -1;
        }
        if (child == 0) {
            char path[512];
            char token;
            close(ready_pipe[0]);
            close(start_pipe[1]);
            snprintf(path, sizeof(path), "%s/metadata-worker-%d", base, worker);
            int fd = open(path, O_RDWR);
            if (fd < 0 || write(ready_pipe[1], "r", 1) != 1
                || read(start_pipe[0], &token, 1) != 1) {
                _exit(111);
            }
            for (int iteration = 0; iteration < INODE_METADATA_ITERATIONS; ++iteration) {
                mode_t mode = iteration & 1 ? 0640 : 0600;
                if (fchmod(fd, mode) != 0) {
                    _exit(112);
                }
            }
            struct stat statbuf;
            if (fstat(fd, &statbuf) != 0 || (statbuf.st_mode & 0777) != 0640) {
                fprintf(stderr, "FS5_INODE_METADATA_CHILD_MODE worker=%d mode=%04o errno=%d\n",
                        worker, (unsigned)(statbuf.st_mode & 0777), errno);
                _exit(114);
            }
            _exit(close(fd) == 0 ? 0 : 113);
        }
        children[worker] = child;
    }
    close(ready_pipe[1]);
    close(start_pipe[0]);
    if (read_tokens(ready_pipe[0], workers) != 0) {
        return -1;
    }
    uint64_t start = monotonic_ns();
    if (start == 0) {
        return -1;
    }
    for (int worker = 0; worker < workers; ++worker) {
        if (write(start_pipe[1], "s", 1) != 1) {
            return -1;
        }
    }
    close(ready_pipe[0]);
    close(start_pipe[1]);
    int errors = 0;
    for (int worker = 0; worker < workers; ++worker) {
        if (wait_success(children[worker]) != 0) {
            ++errors;
        }
    }
    uint64_t end = monotonic_ns();
    if (end <= start) {
        return -1;
    }
    uint64_t operations = (uint64_t)workers * INODE_METADATA_ITERATIONS;
    uint64_t throughput = operations * UINT64_C(1000000000) / (end - start);
    printf("FS5_INODE_METADATA_CELL workers=%d iterations=%d operations=%" PRIu64
           " elapsed_ns=%" PRIu64 " throughput_ops_per_s=%" PRIu64 " errors=%d\n",
           workers, INODE_METADATA_ITERATIONS, operations, end - start, throughput, errors);
    fflush(stdout);
    return errors == 0 ? 0 : -1;
}

static int phase_independent_inode_metadata(const char *base)
{
    for (int worker = 0; worker < INODE_METADATA_WORKERS; ++worker) {
        char path[512];
        snprintf(path, sizeof(path), "%s/metadata-worker-%d", base, worker);
        int fd = open(path, O_CREAT | O_EXCL | O_RDWR, 0600);
        if (fd < 0 || close(fd) != 0) {
            return -1;
        }
    }
    for (int workers = 1; workers <= INODE_METADATA_WORKERS; workers *= 2) {
        if (run_inode_metadata_cell(base, workers) != 0) {
            return -1;
        }
        for (int worker = 0; worker < workers; ++worker) {
            char path[512];
            struct stat statbuf;
            snprintf(path, sizeof(path), "%s/metadata-worker-%d", base, worker);
            if (stat(path, &statbuf) != 0 || (statbuf.st_mode & 0777) != 0640) {
                fprintf(stderr,
                        "FS5_INODE_METADATA_CELL_MODE workers=%d worker=%d mode=%04o errno=%d\n",
                        workers, worker, (unsigned)(statbuf.st_mode & 0777), errno);
                return -1;
            }
        }
    }
    for (int worker = 0; worker < INODE_METADATA_WORKERS; ++worker) {
        char path[512];
        struct stat statbuf;
        snprintf(path, sizeof(path), "%s/metadata-worker-%d", base, worker);
        if (stat(path, &statbuf) != 0 || (statbuf.st_mode & 0777) != 0640
            || unlink(path) != 0) {
            fprintf(stderr,
                    "FS5_INODE_METADATA_FINAL_FAIL worker=%d stat_rc=%d errno=%d mode=%04o\n",
                    worker, stat(path, &statbuf), errno,
                    stat(path, &statbuf) == 0 ? (unsigned)(statbuf.st_mode & 0777) : 0u);
            return -1;
        }
    }
    return 0;
}

static int run_create_cell(const char *base, int workers)
{
    int ready_pipe[2];
    int start_pipe[2];
    pid_t children[CREATE_WORKERS];
    for (int worker = 0; worker < workers; ++worker) {
        char dir[512];
        snprintf(dir, sizeof(dir), "%s/create-cell-%d-worker-%d", base, workers, worker);
        if (mkdir(dir, 0700) != 0) {
            return -1;
        }
    }
    if (pipe(ready_pipe) != 0 || pipe(start_pipe) != 0) {
        return -1;
    }
    for (int worker = 0; worker < workers; ++worker) {
        pid_t child = fork();
        if (child < 0) {
            return -1;
        }
        if (child == 0) {
            char token;
            close(ready_pipe[0]);
            close(start_pipe[1]);
            if (write(ready_pipe[1], "r", 1) != 1
                || read(start_pipe[0], &token, 1) != 1) {
                _exit(121);
            }
            for (int iteration = 0; iteration < CREATE_ITERATIONS; ++iteration) {
                char path[512];
                snprintf(path, sizeof(path), "%s/create-cell-%d-worker-%d/file-%d", base,
                         workers, worker, iteration);
                int fd = open(path, O_CREAT | O_EXCL | O_WRONLY, 0600);
                if (fd < 0 || close(fd) != 0) {
                    _exit(122);
                }
            }
            _exit(0);
        }
        children[worker] = child;
    }
    close(ready_pipe[1]);
    close(start_pipe[0]);
    if (read_tokens(ready_pipe[0], workers) != 0) {
        return -1;
    }
    uint64_t start = monotonic_ns();
    if (start == 0) {
        return -1;
    }
    for (int worker = 0; worker < workers; ++worker) {
        if (write(start_pipe[1], "s", 1) != 1) {
            return -1;
        }
    }
    close(ready_pipe[0]);
    close(start_pipe[1]);
    int errors = 0;
    for (int worker = 0; worker < workers; ++worker) {
        if (wait_success(children[worker]) != 0) {
            ++errors;
        }
    }
    uint64_t end = monotonic_ns();
    if (end <= start) {
        return -1;
    }
    for (int worker = 0; worker < workers; ++worker) {
        char dir[512];
        snprintf(dir, sizeof(dir), "%s/create-cell-%d-worker-%d", base, workers, worker);
        for (int iteration = 0; iteration < CREATE_ITERATIONS; ++iteration) {
            char path[1024];
            struct stat statbuf;
            snprintf(path, sizeof(path), "%s/file-%d", dir, iteration);
            if (stat(path, &statbuf) != 0 || !S_ISREG(statbuf.st_mode)
                || (statbuf.st_mode & 0777) != 0600 || unlink(path) != 0) {
                ++errors;
                break;
            }
        }
        if (errors == 0 && rmdir(dir) != 0) {
            ++errors;
        }
    }
    uint64_t operations = (uint64_t)workers * CREATE_ITERATIONS;
    uint64_t throughput = operations * UINT64_C(1000000000) / (end - start);
    printf("FS5_CREATE_CELL workers=%d iterations=%d operations=%" PRIu64
           " elapsed_ns=%" PRIu64 " throughput_ops_per_s=%" PRIu64 " errors=%d\n",
           workers, CREATE_ITERATIONS, operations, end - start, throughput, errors);
    fflush(stdout);
    return errors == 0 ? 0 : -1;
}

static int phase_independent_create(const char *base)
{
    for (int workers = 1; workers <= CREATE_WORKERS; workers *= 2) {
        if (run_create_cell(base, workers) != 0) {
            return -1;
        }
    }
    return 0;
}

static int phase_readlink_plan(const char *base)
{
    char short_path[512];
    char long_path[512];
    char long_target[241];
    char observed[256];
    snprintf(short_path, sizeof(short_path), "%s/readlink-inline", base);
    snprintf(long_path, sizeof(long_path), "%s/readlink-external", base);
    for (size_t i = 0; i + 1 < sizeof(long_target); ++i) {
        long_target[i] = (char)('a' + i % 26);
    }
    long_target[sizeof(long_target) - 1] = '\0';
    if (symlink("inline-target", short_path) != 0 || symlink(long_target, long_path) != 0) {
        return -1;
    }
    memset(observed, 0, sizeof(observed));
    if (readlink(short_path, observed, sizeof(observed)) != 13
        || memcmp(observed, "inline-target", 13) != 0) {
        return -1;
    }
    memset(observed, 0, sizeof(observed));
    if (readlink(long_path, observed, sizeof(observed)) != (ssize_t)strlen(long_target)
        || memcmp(observed, long_target, strlen(long_target)) != 0) {
        return -1;
    }
    memset(observed, 0, sizeof(observed));
    if (readlink(long_path, observed, 17) != 17 || memcmp(observed, long_target, 17) != 0
        || unlink(short_path) != 0 || unlink(long_path) != 0) {
        return -1;
    }
    return 0;
}

static int phase_readlink_vs_unlink(const char *base)
{
    enum { TARGET_LEN = 191 };
    char path[512];
    char target_a[TARGET_LEN + 1];
    char target_b[TARGET_LEN + 1];
    snprintf(path, sizeof(path), "%s/readlink-unlink-race", base);
    memset(target_a, 'A', TARGET_LEN);
    memset(target_b, 'B', TARGET_LEN);
    target_a[TARGET_LEN] = '\0';
    target_b[TARGET_LEN] = '\0';
    if (symlink(target_a, path) != 0) {
        return -1;
    }
    pid_t writer = fork();
    if (writer == 0) {
        for (int i = 0; i < READ_MUTATION_ITERATIONS; ++i) {
            const char *target = i & 1 ? target_a : target_b;
            if (unlink(path) != 0 || symlink(target, path) != 0) {
                _exit(81);
            }
        }
        _exit(0);
    }
    pid_t reader = fork();
    if (reader == 0) {
        char observed[TARGET_LEN + 1];
        for (int i = 0; i < READ_MUTATION_ITERATIONS * 4; ++i) {
            ssize_t amount = readlink(path, observed, TARGET_LEN);
            if (amount < 0) {
                if (errno == ENOENT) {
                    continue;
                }
                _exit(82);
            }
            if (amount != TARGET_LEN || (observed[0] != 'A' && observed[0] != 'B')) {
                fprintf(stderr,
                        "FS4_INODE_STATE_READLINK_RACE amount=%ld first=%u errno=%d\n",
                        (long)amount, amount > 0 ? (unsigned char)observed[0] : 0u, errno);
                _exit(83);
            }
            for (int j = 1; j < TARGET_LEN; ++j) {
                if (observed[j] != observed[0]) {
                    _exit(84);
                }
            }
        }
        _exit(0);
    }
    if (writer < 0 || reader < 0 || wait_success(writer) != 0 || wait_success(reader) != 0
        || unlink(path) != 0) {
        return -1;
    }
    return 0;
}

static int phase_directory_snapshot(const char *base)
{
    char dir[512];
    char path[1024];
    snprintf(dir, sizeof(dir), "%s/directory-snapshot", base);
    if (mkdir(dir, 0700) != 0) {
        return -1;
    }
    for (int i = 0; i < DIRECTORY_SNAPSHOT_FILES; ++i) {
        snprintf(path, sizeof(path), "%s/entry-%03d-abcdefghijklmnopqrstuvwxyz", dir, i);
        int fd = open(path, O_CREAT | O_EXCL | O_WRONLY, 0600);
        if (fd < 0 || close(fd) != 0) {
            return -1;
        }
    }
    int dirfd = open(dir, O_RDONLY | O_DIRECTORY);
    if (dirfd < 0) {
        return -1;
    }
    unsigned char seen[DIRECTORY_SNAPSHOT_FILES];
    unsigned char buffer[257];
    memset(seen, 0, sizeof(seen));
    int dot = 0;
    int dotdot = 0;
    int files = 0;
    for (;;) {
        long amount = syscall(SYS_getdents64, dirfd, buffer, sizeof(buffer));
        if (amount < 0) {
            return -1;
        }
        if (amount == 0) {
            break;
        }
        size_t cursor = 0;
        while (cursor < (size_t)amount) {
            struct probe_linux_dirent64 *entry = (void *)(buffer + cursor);
            size_t header = offsetof(struct probe_linux_dirent64, d_name);
            if (entry->d_reclen < header + 1 || entry->d_reclen % 8 != 0
                || cursor + entry->d_reclen > (size_t)amount || entry->d_off <= 0) {
                return -1;
            }
            size_t name_capacity = entry->d_reclen - header;
            size_t name_len = strnlen(entry->d_name, name_capacity);
            if (name_len == name_capacity) {
                return -1;
            }
            if (strcmp(entry->d_name, ".") == 0) {
                if (++dot != 1 || entry->d_type != DT_DIR) {
                    return -1;
                }
            } else if (strcmp(entry->d_name, "..") == 0) {
                if (++dotdot != 1 || entry->d_type != DT_DIR) {
                    return -1;
                }
            } else {
                int index = -1;
                char suffix[40];
                if (sscanf(entry->d_name, "entry-%03d-%39s", &index, suffix) != 2
                    || index < 0 || index >= DIRECTORY_SNAPSHOT_FILES || seen[index]
                    || strcmp(suffix, "abcdefghijklmnopqrstuvwxyz") != 0
                    || entry->d_type != DT_REG) {
                    return -1;
                }
                seen[index] = 1;
                ++files;
            }
            cursor += entry->d_reclen;
        }
    }
    if (dot != 1 || dotdot != 1 || files != DIRECTORY_SNAPSHOT_FILES || close(dirfd) != 0) {
        return -1;
    }
    for (int i = 0; i < DIRECTORY_SNAPSHOT_FILES; ++i) {
        if (!seen[i]) {
            return -1;
        }
        snprintf(path, sizeof(path), "%s/entry-%03d-abcdefghijklmnopqrstuvwxyz", dir, i);
        if (unlink(path) != 0) {
            return -1;
        }
    }
    return rmdir(dir);
}

static int phase_readdir_vs_namespace_mutation(const char *base)
{
    char dir[512];
    char path[1024];
    char changing[1024];
    snprintf(dir, sizeof(dir), "%s/readdir-race", base);
    snprintf(changing, sizeof(changing), "%s/changing", dir);
    if (mkdir(dir, 0700) != 0) {
        return -1;
    }
    for (int i = 0; i < DIRECTORY_RACE_STABLE_FILES; ++i) {
        snprintf(path, sizeof(path), "%s/stable-%02d", dir, i);
        int fd = open(path, O_CREAT | O_EXCL | O_WRONLY, 0600);
        if (fd < 0 || close(fd) != 0) {
            return -1;
        }
    }
    pid_t writer = fork();
    if (writer == 0) {
        for (int i = 0; i < READ_MUTATION_ITERATIONS; ++i) {
            int fd = open(changing, O_CREAT | O_EXCL | O_WRONLY, 0600);
            if (fd < 0 || close(fd) != 0 || unlink(changing) != 0) {
                _exit(91);
            }
        }
        _exit(0);
    }
    pid_t reader = fork();
    if (reader == 0) {
        int dirfd = open(dir, O_RDONLY | O_DIRECTORY);
        if (dirfd < 0) {
            _exit(92);
        }
        unsigned char buffer[4096];
        for (int iteration = 0; iteration < READ_MUTATION_ITERATIONS * 2; ++iteration) {
            unsigned char seen[DIRECTORY_RACE_STABLE_FILES];
            memset(seen, 0, sizeof(seen));
            if (lseek(dirfd, 0, SEEK_SET) != 0) {
                _exit(93);
            }
            long amount = syscall(SYS_getdents64, dirfd, buffer, sizeof(buffer));
            if (amount <= 0) {
                _exit(94);
            }
            int dot = 0;
            int dotdot = 0;
            int stable = 0;
            int changing_count = 0;
            size_t cursor = 0;
            while (cursor < (size_t)amount) {
                struct probe_linux_dirent64 *entry = (void *)(buffer + cursor);
                size_t header = offsetof(struct probe_linux_dirent64, d_name);
                if (entry->d_reclen < header + 1 || entry->d_reclen % 8 != 0
                    || cursor + entry->d_reclen > (size_t)amount) {
                    _exit(95);
                }
                size_t capacity = entry->d_reclen - header;
                if (strnlen(entry->d_name, capacity) == capacity) {
                    _exit(96);
                }
                if (strcmp(entry->d_name, ".") == 0) {
                    ++dot;
                } else if (strcmp(entry->d_name, "..") == 0) {
                    ++dotdot;
                } else if (strcmp(entry->d_name, "changing") == 0) {
                    ++changing_count;
                } else {
                    int index = -1;
                    if (sscanf(entry->d_name, "stable-%02d", &index) != 1 || index < 0
                        || index >= DIRECTORY_RACE_STABLE_FILES || seen[index]) {
                        _exit(97);
                    }
                    seen[index] = 1;
                    ++stable;
                }
                cursor += entry->d_reclen;
            }
            if (dot != 1 || dotdot != 1 || stable != DIRECTORY_RACE_STABLE_FILES
                || changing_count > 1) {
                _exit(98);
            }
        }
        if (close(dirfd) != 0) {
            _exit(99);
        }
        _exit(0);
    }
    if (writer < 0 || reader < 0 || wait_success(writer) != 0 || wait_success(reader) != 0) {
        return -1;
    }
    for (int i = 0; i < DIRECTORY_RACE_STABLE_FILES; ++i) {
        snprintf(path, sizeof(path), "%s/stable-%02d", dir, i);
        if (unlink(path) != 0) {
            return -1;
        }
    }
    if (unlink(changing) != 0 && errno != ENOENT) {
        return -1;
    }
    return rmdir(dir);
}

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
    if (WIFEXITED(status) && WEXITSTATUS(status) == 0) {
        return 0;
    }
    if (WIFEXITED(status)) {
        fprintf(stderr, "FS4_INODE_STATE_CHILD_FAIL pid=%ld exit=%d\n", (long)pid,
                WEXITSTATUS(status));
    } else if (WIFSIGNALED(status)) {
        fprintf(stderr, "FS4_INODE_STATE_CHILD_FAIL pid=%ld signal=%d\n", (long)pid,
                WTERMSIG(status));
    }
    return -1;
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
    alarm(180);
    if (mkdir(argv[1], 0700) != 0 && errno != EEXIST) {
        return 2;
    }
#define RUN_CASE(name)                                                            \
    do {                                                                          \
        uint64_t case_start = monotonic_ns();                                     \
        if (phase_##name(argv[1]) != 0) {                                         \
            puts("FS4_INODE_STATE_CASE_FAIL case=" #name);                       \
            return 1;                                                             \
        }                                                                         \
        uint64_t case_end = monotonic_ns();                                       \
        puts("FS4_INODE_STATE_CASE_PASS case=" #name);                           \
        printf("FS4_INODE_STATE_CASE_TIME case=" #name " elapsed_ns=%" PRIu64   \
               "\n", case_end > case_start ? case_end - case_start : 0);          \
    } while (0)
    RUN_CASE(unlink_open_close);
    RUN_CASE(concurrent_final_close);
    RUN_CASE(fast_inode_reuse);
    RUN_CASE(cross_directory_rename);
    RUN_CASE(lookup_stat_vs_namespace_mutation);
    RUN_CASE(read_vs_mapping_mutation);
    RUN_CASE(partial_read_plan);
    RUN_CASE(mapped_overwrite_plan);
    RUN_CASE(independent_mapped_overwrite);
    RUN_CASE(disjoint_sequence_conflict);
    RUN_CASE(independent_create);
    RUN_CASE(independent_inode_metadata);
    RUN_CASE(readlink_plan);
    RUN_CASE(readlink_vs_unlink);
    RUN_CASE(directory_snapshot);
    RUN_CASE(readdir_vs_namespace_mutation);
    RUN_CASE(shutdown_drain_stress);
#undef RUN_CASE
    puts("FS4_INODE_STATE_PROBE_PASS cases=17");
    return 0;
}
