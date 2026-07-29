#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>

#define MAX_WORKERS 8
#define HIT_ITERATIONS 256
#define SCALE_ITERATIONS 8000
#define COLD_FILES_PER_WORKER 256

struct worker {
    int dirfd;
    const char *path;
    int iterations;
    pthread_barrier_t *barrier;
    int target_cpu;
    int start_cpu;
    int end_cpu;
    uint64_t elapsed_ns;
    int failed;
};

struct cold_worker {
    int dirfd;
    pthread_barrier_t *barrier;
    int target_cpu;
    int start_cpu;
    int end_cpu;
    uint64_t elapsed_ns;
    int failed;
};

static uint64_t monotonic_ns(void)
{
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) {
        return 0;
    }
    return (uint64_t)ts.tv_sec * 1000000000ULL + (uint64_t)ts.tv_nsec;
}

static int read_counter(const char *name, unsigned long long *value)
{
    static int perf_fd = -1;
    if (perf_fd < 0) {
        perf_fd = open("/proc/oskernel/perf", O_RDONLY);
    }
    if (perf_fd < 0 || lseek(perf_fd, 0, SEEK_SET) < 0) {
        return -1;
    }
    char *content = malloc(256 * 1024);
    if (content == NULL) {
        return -1;
    }
    size_t length = 0;
    while (length < 256 * 1024 - 1) {
        ssize_t count = read(perf_fd, content + length, 256 * 1024 - 1 - length);
        if (count < 0) {
            free(content);
            return -1;
        }
        if (count == 0) {
            break;
        }
        length += (size_t)count;
    }
    content[length] = '\0';
    char key[160];
    unsigned long long observed;
    int found = 0;
    char *save = NULL;
    for (char *line = strtok_r(content, "\n", &save); line != NULL;
         line = strtok_r(NULL, "\n", &save)) {
        if (sscanf(line, "%159s %llu", key, &observed) == 2 && strcmp(key, name) == 0) {
            *value = observed;
            found = 1;
            break;
        }
    }
    free(content);
    return found ? 0 : -1;
}

static int create_file(const char *path, const char *payload)
{
    int fd = open(path, O_CREAT | O_EXCL | O_RDWR, 0600);
    if (fd < 0) {
        return -1;
    }
    size_t len = strlen(payload);
    ssize_t written = write(fd, payload, len);
    int saved = errno;
    if (close(fd) != 0 || written != (ssize_t)len) {
        errno = saved;
        return -1;
    }
    return 0;
}

static void *stat_worker(void *opaque)
{
    struct worker *worker = opaque;
    struct stat statbuf;
    cpu_set_t affinity;
    CPU_ZERO(&affinity);
    CPU_SET(worker->target_cpu, &affinity);
    if (sched_setaffinity(0, sizeof(affinity), &affinity) != 0) {
        worker->failed = 1;
    }
    int barrier = pthread_barrier_wait(worker->barrier);
    if (barrier != 0 && barrier != PTHREAD_BARRIER_SERIAL_THREAD) {
        worker->failed = 1;
        return NULL;
    }
    worker->start_cpu = sched_getcpu();
    uint64_t start = monotonic_ns();
    if (!worker->failed) {
        for (int iteration = 0; iteration < worker->iterations; ++iteration) {
            int result;
            if (worker->dirfd == -2) {
                result = syscall(SYS_gettid) > 0 ? 0 : -1;
            } else {
                result = worker->dirfd >= 0
                    ? fstatat(worker->dirfd, worker->path, &statbuf, 0)
                    : stat(worker->path, &statbuf);
            }
            if (result != 0 || (worker->dirfd != -2 && !S_ISREG(statbuf.st_mode))) {
                worker->failed = 1;
                break;
            }
        }
    }
    uint64_t end = monotonic_ns();
    if (start == 0 || end < start) {
        worker->failed = 1;
    } else {
        worker->elapsed_ns = end - start;
    }
    worker->end_cpu = sched_getcpu();
    barrier = pthread_barrier_wait(worker->barrier);
    if (barrier != 0 && barrier != PTHREAD_BARRIER_SERIAL_THREAD) {
        worker->failed = 1;
    }
    return NULL;
}

static int run_workers(
    int dirfd, const char *const *paths, int workers, int iterations, uint64_t *elapsed_ns,
    int *start_cpus, int *end_cpus, uint64_t *worker_elapsed_ns)
{
    pthread_t threads[MAX_WORKERS];
    struct worker args[MAX_WORKERS];
    pthread_barrier_t barrier;
    if (pthread_barrier_init(&barrier, NULL, (unsigned)workers + 1) != 0) {
        return -1;
    }
    for (int index = 0; index < workers; ++index) {
        args[index] = (struct worker){
            .dirfd = dirfd,
            .path = paths != NULL ? paths[index] : NULL,
            .iterations = iterations,
            .barrier = &barrier,
            .target_cpu = index,
            .start_cpu = -1,
            .end_cpu = -1,
            .elapsed_ns = 0,
            .failed = 0,
        };
        if (pthread_create(&threads[index], NULL, stat_worker, &args[index]) != 0) {
            pthread_barrier_destroy(&barrier);
            return -1;
        }
    }
    int barrier_result = pthread_barrier_wait(&barrier);
    if (barrier_result != 0 && barrier_result != PTHREAD_BARRIER_SERIAL_THREAD) {
        pthread_barrier_destroy(&barrier);
        return -1;
    }
    uint64_t start = monotonic_ns();
    barrier_result = pthread_barrier_wait(&barrier);
    uint64_t end = monotonic_ns();
    if (barrier_result != 0 && barrier_result != PTHREAD_BARRIER_SERIAL_THREAD) {
        pthread_barrier_destroy(&barrier);
        return -1;
    }
    int failed = 0;
    for (int index = 0; index < workers; ++index) {
        if (pthread_join(threads[index], NULL) != 0 || args[index].failed) {
            failed = 1;
        }
        if (start_cpus != NULL) {
            start_cpus[index] = args[index].start_cpu;
        }
        if (end_cpus != NULL) {
            end_cpus[index] = args[index].end_cpu;
        }
        if (worker_elapsed_ns != NULL) {
            worker_elapsed_ns[index] = args[index].elapsed_ns;
        }
    }
    pthread_barrier_destroy(&barrier);
    if (failed || start == 0 || end < start) {
        return -1;
    }
    *elapsed_ns = end - start;
    return 0;
}

static void *cold_stat_worker(void *opaque)
{
    struct cold_worker *worker = opaque;
    struct stat statbuf;
    cpu_set_t affinity;
    CPU_ZERO(&affinity);
    CPU_SET(worker->target_cpu, &affinity);
    if (sched_setaffinity(0, sizeof(affinity), &affinity) != 0) {
        worker->failed = 1;
    }
    int barrier = pthread_barrier_wait(worker->barrier);
    if (barrier != 0 && barrier != PTHREAD_BARRIER_SERIAL_THREAD) {
        worker->failed = 1;
        return NULL;
    }
    worker->start_cpu = sched_getcpu();
    uint64_t start = monotonic_ns();
    for (int index = 0; !worker->failed && index < COLD_FILES_PER_WORKER; ++index) {
        char name[32];
        snprintf(name, sizeof(name), "entry-%04d", index);
        if (fstatat(worker->dirfd, name, &statbuf, 0) != 0 || !S_ISREG(statbuf.st_mode)) {
            worker->failed = 1;
        }
    }
    uint64_t end = monotonic_ns();
    if (start == 0 || end < start) {
        worker->failed = 1;
    } else {
        worker->elapsed_ns = end - start;
    }
    worker->end_cpu = sched_getcpu();
    barrier = pthread_barrier_wait(worker->barrier);
    if (barrier != 0 && barrier != PTHREAD_BARRIER_SERIAL_THREAD) {
        worker->failed = 1;
    }
    return NULL;
}

static int phase_cold_shared_read_scaling(void)
{
    const int cells[] = {1, 2, 4, 8};
    for (size_t cell = 0; cell < sizeof(cells) / sizeof(cells[0]); ++cell) {
        int workers = cells[cell];
        unsigned long long lookup_before = 0, lookup_after = 0;
        unsigned long long lookup_contended_before = 0, lookup_contended_after = 0;
        unsigned long long lookup_wait_before = 0, lookup_wait_after = 0;
        unsigned long long stat_basic_before = 0, stat_basic_after = 0;
        unsigned long long stat_full_before = 0, stat_full_after = 0;
        unsigned long long block_reads_before = 0, block_reads_after = 0;
        unsigned long long block_read_blocks_before = 0, block_read_blocks_after = 0;
        unsigned long long sync_reads_before = 0, sync_reads_after = 0;
        unsigned long long nb_reads_before = 0, nb_reads_after = 0;
        unsigned long long inflight_before = 0, inflight_after = 0;
        unsigned long long inflight_hwm_before = 0, inflight_hwm_after = 0;
        unsigned long long index_locks_before = 0, index_locks_after = 0;
        unsigned long long index_contended_before = 0, index_contended_after = 0;
        unsigned long long lba_locks_before = 0, lba_locks_after = 0;
        unsigned long long lba_contended_before = 0, lba_contended_after = 0;
        int counters = read_counter("backend_op_lookup_calls", &lookup_before) == 0
            && read_counter("backend_op_lookup_contended", &lookup_contended_before) == 0
            && read_counter("backend_op_lookup_wait_us", &lookup_wait_before) == 0
            && read_counter("backend_op_stat_basic_calls", &stat_basic_before) == 0
            && read_counter("backend_op_stat_full_calls", &stat_full_before) == 0
            && read_counter("ext4_block_read_calls", &block_reads_before) == 0
            && read_counter("ext4_block_read_blocks", &block_read_blocks_before) == 0
            && read_counter("block_io_sync_read_submits", &sync_reads_before) == 0
            && read_counter("block_io_nb_read_submits", &nb_reads_before) == 0
            && read_counter("block_io_device_inflight", &inflight_before) == 0
            && read_counter("block_io_device_inflight_high_watermark", &inflight_hwm_before) == 0;
        int bcache_lock_counters =
            read_counter("ext4_bcache_index_lock_calls", &index_locks_before) == 0
            && read_counter("ext4_bcache_index_lock_contended", &index_contended_before) == 0
            && read_counter("ext4_bcache_lba_lock_calls", &lba_locks_before) == 0
            && read_counter("ext4_bcache_lba_lock_contended", &lba_contended_before) == 0;
        pthread_t threads[MAX_WORKERS];
        struct cold_worker args[MAX_WORKERS];
        pthread_barrier_t barrier;
        if (pthread_barrier_init(&barrier, NULL, (unsigned)workers + 1) != 0) {
            return -1;
        }
        for (int worker = 0; worker < workers; ++worker) {
            char path[128];
            snprintf(path, sizeof(path), "/x1/fs5-cold/cell-%d/worker-%d", workers, worker);
            int dirfd = open(path, O_RDONLY | O_DIRECTORY);
            if (dirfd < 0) {
                return -1;
            }
            args[worker] = (struct cold_worker){
                .dirfd = dirfd,
                .barrier = &barrier,
                .target_cpu = worker,
                .start_cpu = -1,
                .end_cpu = -1,
                .elapsed_ns = 0,
                .failed = 0,
            };
            if (pthread_create(&threads[worker], NULL, cold_stat_worker, &args[worker]) != 0) {
                return -1;
            }
        }
        int barrier_result = pthread_barrier_wait(&barrier);
        if (barrier_result != 0 && barrier_result != PTHREAD_BARRIER_SERIAL_THREAD) {
            return -1;
        }
        barrier_result = pthread_barrier_wait(&barrier);
        if (barrier_result != 0 && barrier_result != PTHREAD_BARRIER_SERIAL_THREAD) {
            return -1;
        }
        int failed = 0;
        uint64_t elapsed = 0;
        for (int worker = 0; worker < workers; ++worker) {
            if (pthread_join(threads[worker], NULL) != 0 || args[worker].failed
                || close(args[worker].dirfd) != 0) {
                failed = 1;
            }
            if (args[worker].elapsed_ns > elapsed) {
                elapsed = args[worker].elapsed_ns;
            }
        }
        pthread_barrier_destroy(&barrier);
        if (failed || elapsed == 0) {
            return -1;
        }
        unsigned long long operations =
            (unsigned long long)workers * COLD_FILES_PER_WORKER;
        unsigned long long throughput = operations * 1000000000ULL / elapsed;
        printf("FS5_SHARED_READ_SCALE workers=%d operations=%llu elapsed_ns=%llu ops_per_second=%llu\n",
            workers, operations, (unsigned long long)elapsed, throughput);
        printf("FS5_SHARED_READ_CPUS workers=%d", workers);
        for (int worker = 0; worker < workers; ++worker) {
            printf(" slot%d=%d->%d:%llu", worker, args[worker].start_cpu,
                args[worker].end_cpu, (unsigned long long)args[worker].elapsed_ns);
        }
        putchar('\n');
        counters = counters && read_counter("backend_op_lookup_calls", &lookup_after) == 0
            && read_counter("backend_op_lookup_contended", &lookup_contended_after) == 0
            && read_counter("backend_op_lookup_wait_us", &lookup_wait_after) == 0
            && read_counter("backend_op_stat_basic_calls", &stat_basic_after) == 0
            && read_counter("backend_op_stat_full_calls", &stat_full_after) == 0
            && read_counter("ext4_block_read_calls", &block_reads_after) == 0
            && read_counter("ext4_block_read_blocks", &block_read_blocks_after) == 0
            && read_counter("block_io_sync_read_submits", &sync_reads_after) == 0
            && read_counter("block_io_nb_read_submits", &nb_reads_after) == 0
            && read_counter("block_io_device_inflight", &inflight_after) == 0
            && read_counter("block_io_device_inflight_high_watermark", &inflight_hwm_after) == 0;
        bcache_lock_counters = bcache_lock_counters
            && read_counter("ext4_bcache_index_lock_calls", &index_locks_after) == 0
            && read_counter("ext4_bcache_index_lock_contended", &index_contended_after) == 0
            && read_counter("ext4_bcache_lba_lock_calls", &lba_locks_after) == 0
            && read_counter("ext4_bcache_lba_lock_contended", &lba_contended_after) == 0;
        if (counters) {
            printf("FS5_SHARED_READ_COUNTERS workers=%d lookup_calls=%llu lookup_contended=%llu lookup_wait_us=%llu stat_basic_calls=%llu stat_full_calls=%llu block_read_calls=%llu block_read_blocks=%llu sync_read_submits=%llu nb_read_submits=%llu inflight_before=%llu inflight_after=%llu inflight_hwm_before=%llu inflight_hwm_after=%llu bcache_lock_counters=%d index_lock_calls=%llu index_lock_contended=%llu lba_lock_calls=%llu lba_lock_contended=%llu\n",
                workers, lookup_after - lookup_before,
                lookup_contended_after - lookup_contended_before,
                lookup_wait_after - lookup_wait_before, stat_basic_after - stat_basic_before,
                stat_full_after - stat_full_before, block_reads_after - block_reads_before,
                block_read_blocks_after - block_read_blocks_before,
                sync_reads_after - sync_reads_before, nb_reads_after - nb_reads_before,
                inflight_before, inflight_after, inflight_hwm_before, inflight_hwm_after,
                bcache_lock_counters,
                bcache_lock_counters ? index_locks_after - index_locks_before : 0,
                bcache_lock_counters ? index_contended_after - index_contended_before : 0,
                bcache_lock_counters ? lba_locks_after - lba_locks_before : 0,
                bcache_lock_counters ? lba_contended_after - lba_contended_before : 0);
        }
        fflush(stdout);
    }
    return 0;
}

static int phase_cache_hit(const char *path, int expect_fast)
{
    struct stat statbuf;
    if (stat(path, &statbuf) != 0) {
        return -1;
    }
    unsigned long long lookup_before, lookup_after;
    unsigned long long stat_basic_before, stat_basic_after, stat_full_before, stat_full_after;
    unsigned long long hit_before, hit_after, miss_before, miss_after;
    unsigned long long revalidate_before, revalidate_after, insert_before, insert_after;
    if (read_counter("backend_op_lookup_calls", &lookup_before) != 0
        || read_counter("backend_op_stat_basic_calls", &stat_basic_before) != 0
        || read_counter("backend_op_stat_full_calls", &stat_full_before) != 0
        || read_counter("dentry_cache_positive_hit", &hit_before) != 0
        || read_counter("dentry_cache_miss", &miss_before) != 0
        || read_counter("dentry_cache_revalidate_fail", &revalidate_before) != 0
        || read_counter("dentry_cache_insert_positive", &insert_before) != 0) {
        return -1;
    }
    const char *paths[MAX_WORKERS];
    for (int index = 0; index < MAX_WORKERS; ++index) {
        paths[index] = path;
    }
    uint64_t elapsed;
    if (run_workers(-1, paths, MAX_WORKERS, HIT_ITERATIONS, &elapsed, NULL, NULL, NULL) != 0
        || read_counter("backend_op_lookup_calls", &lookup_after) != 0
        || read_counter("backend_op_stat_basic_calls", &stat_basic_after) != 0
        || read_counter("backend_op_stat_full_calls", &stat_full_after) != 0
        || read_counter("dentry_cache_positive_hit", &hit_after) != 0
        || read_counter("dentry_cache_miss", &miss_after) != 0
        || read_counter("dentry_cache_revalidate_fail", &revalidate_after) != 0
        || read_counter("dentry_cache_insert_positive", &insert_after) != 0) {
        return -1;
    }
    unsigned long long lookup_delta = lookup_after - lookup_before;
    unsigned long long stat_basic_delta = stat_basic_after - stat_basic_before;
    unsigned long long stat_full_delta = stat_full_after - stat_full_before;
    unsigned long long stat_delta = stat_basic_delta + stat_full_delta;
    printf("FS4E_CACHE_HIT_RESULT workers=%d operations=%d elapsed_ns=%llu lookup_delta=%llu stat_delta=%llu stat_basic_delta=%llu stat_full_delta=%llu dentry_hit_delta=%llu dentry_miss_delta=%llu dentry_revalidate_delta=%llu dentry_insert_delta=%llu\n",
        MAX_WORKERS, MAX_WORKERS * HIT_ITERATIONS, (unsigned long long)elapsed,
        lookup_delta, stat_delta, stat_basic_delta, stat_full_delta, hit_after - hit_before,
        miss_after - miss_before, revalidate_after - revalidate_before, insert_after - insert_before);
    if (expect_fast && (lookup_delta != 0 || stat_delta != 0)) {
        printf("FS4E_CACHE_HIT_FAIL lookup_delta=%llu stat_delta=%llu\n", lookup_delta,
            stat_delta);
        return -1;
    }
    if (expect_fast) {
        puts("FS4E_CACHE_HIT_PASS lookup_delta=0 stat_delta=0");
    }
    return 0;
}

static int phase_single_flight(const char *path, int expect_fast)
{
    unsigned long long lookup_before, lookup_after;
    unsigned long long stat_basic_before, stat_basic_after, stat_full_before, stat_full_after;
    if (read_counter("backend_op_lookup_calls", &lookup_before) != 0
        || read_counter("backend_op_stat_basic_calls", &stat_basic_before) != 0
        || read_counter("backend_op_stat_full_calls", &stat_full_before) != 0) {
        return -1;
    }
    const char *paths[MAX_WORKERS];
    for (int index = 0; index < MAX_WORKERS; ++index) {
        paths[index] = path;
    }
    uint64_t elapsed;
    if (run_workers(-1, paths, MAX_WORKERS, 1, &elapsed, NULL, NULL, NULL) != 0
        || read_counter("backend_op_lookup_calls", &lookup_after) != 0
        || read_counter("backend_op_stat_basic_calls", &stat_basic_after) != 0
        || read_counter("backend_op_stat_full_calls", &stat_full_after) != 0) {
        return -1;
    }
    unsigned long long lookup_delta = lookup_after - lookup_before;
    unsigned long long stat_basic_delta = stat_basic_after - stat_basic_before;
    unsigned long long stat_full_delta = stat_full_after - stat_full_before;
    unsigned long long stat_delta = stat_basic_delta + stat_full_delta;
    printf("FS4E_SINGLE_FLIGHT_RESULT workers=%d elapsed_ns=%llu lookup_delta=%llu stat_delta=%llu stat_basic_delta=%llu stat_full_delta=%llu\n",
        MAX_WORKERS, (unsigned long long)elapsed, lookup_delta, stat_delta, stat_basic_delta,
        stat_full_delta);
    /* File creation may already seed the inode metadata cache. The cold name
     * is still guaranteed to exercise one dentry miss; if metadata is cold,
     * its per-inode single-flight permits at most one backend stat call. */
    if (expect_fast && (lookup_delta != 1 || stat_delta > 1)) {
        printf("FS4E_SINGLE_FLIGHT_FAIL lookup_delta=%llu stat_delta=%llu\n", lookup_delta,
            stat_delta);
        return -1;
    }
    if (expect_fast) {
        printf("FS4E_SINGLE_FLIGHT_PASS lookup_delta=1 stat_delta=%llu\n", stat_delta);
    }
    return 0;
}

static int phase_scaling(int dirfd, const char *const *paths)
{
    const int cells[] = {1, 2, 4, 8};
    struct stat statbuf;
    for (int index = 0; index < MAX_WORKERS; ++index) {
        if (fstatat(dirfd, paths[index], &statbuf, 0) != 0) {
            return -1;
        }
        printf("FS4E_SCALE_INODE slot=%d ino=%llu shard=%llu\n", index,
            (unsigned long long)statbuf.st_ino, (unsigned long long)(statbuf.st_ino % 32));
    }
    for (size_t cell = 0; cell < sizeof(cells) / sizeof(cells[0]); ++cell) {
        int workers = cells[cell];
        uint64_t elapsed;
        int start_cpus[MAX_WORKERS], end_cpus[MAX_WORKERS];
        uint64_t worker_elapsed_ns[MAX_WORKERS];
        if (run_workers(dirfd, paths, workers, SCALE_ITERATIONS, &elapsed, start_cpus, end_cpus,
                worker_elapsed_ns) != 0
            || elapsed == 0) {
            return -1;
        }
        unsigned long long operations = (unsigned long long)workers * SCALE_ITERATIONS;
        unsigned long long ops_per_second = operations * 1000000000ULL / elapsed;
        printf("FS4E_SCALE workers=%d operations=%llu elapsed_ns=%llu ops_per_second=%llu\n",
            workers, operations, (unsigned long long)elapsed, ops_per_second);
        printf("FS4E_SCALE_CPUS workers=%d", workers);
        for (int index = 0; index < workers; ++index) {
            printf(" slot%d=%d->%d:%llu", index, start_cpus[index], end_cpus[index],
                (unsigned long long)worker_elapsed_ns[index]);
        }
        putchar('\n');
    }
    return 0;
}

static int phase_syscall_control(void)
{
    const int cells[] = {1, 2, 4, 8};
    for (size_t cell = 0; cell < sizeof(cells) / sizeof(cells[0]); ++cell) {
        int workers = cells[cell];
        uint64_t elapsed;
        int start_cpus[MAX_WORKERS], end_cpus[MAX_WORKERS];
        uint64_t worker_elapsed_ns[MAX_WORKERS];
        if (run_workers(-2, NULL, workers, SCALE_ITERATIONS, &elapsed, start_cpus, end_cpus,
                worker_elapsed_ns) != 0
            || elapsed == 0) {
            return -1;
        }
        unsigned long long operations = (unsigned long long)workers * SCALE_ITERATIONS;
        unsigned long long ops_per_second = operations * 1000000000ULL / elapsed;
        printf("FS4E_SYSCALL_CONTROL workers=%d operations=%llu elapsed_ns=%llu ops_per_second=%llu\n",
            workers, operations, (unsigned long long)elapsed, ops_per_second);
        printf("FS4E_SYSCALL_CONTROL_CPUS workers=%d", workers);
        for (int index = 0; index < workers; ++index) {
            printf(" slot%d=%d->%d:%llu", index, start_cpus[index], end_cpus[index],
                (unsigned long long)worker_elapsed_ns[index]);
        }
        putchar('\n');
    }
    return 0;
}

static int phase_mutation(const char *old_path, const char *new_path)
{
    struct stat before, after;
    if (stat(old_path, &before) != 0 || truncate(old_path, 17) != 0 || chmod(old_path, 0640) != 0
        || rename(old_path, new_path) != 0 || stat(new_path, &after) != 0) {
        return -1;
    }
    errno = 0;
    if (stat(old_path, &before) == 0 || errno != ENOENT || after.st_size != 17
        || (after.st_mode & 0777) != 0640) {
        return -1;
    }
    puts("FS4E_MUTATION_PASS size=17 mode=0640 old_name=negative new_name=positive");
    return 0;
}

int main(int argc, char **argv)
{
    if (argc != 2
        || (strcmp(argv[1], "candidate") != 0 && strcmp(argv[1], "baseline") != 0
            && strcmp(argv[1], "performance") != 0)) {
        return 2;
    }
    int expect_fast = strcmp(argv[1], "candidate") == 0;
    int measure_counters = strcmp(argv[1], "performance") != 0;
    alarm(60);
    char base[128];
    snprintf(base, sizeof(base), "/x1/fs4e-work-%ld", (long)getpid());
    if (mkdir(base, 0700) != 0) {
        return 2;
    }

    char paths[MAX_WORKERS][384];
    char names[MAX_WORKERS][32];
    const char *name_refs[MAX_WORKERS];
    for (int index = 0; index < MAX_WORKERS; ++index) {
        snprintf(names[index], sizeof(names[index]), "scale-%d", index);
        snprintf(paths[index], sizeof(paths[index]), "%s/%s", base, names[index]);
        name_refs[index] = names[index];
        if (create_file(paths[index], "scale") != 0) {
            return 2;
        }
    }
    char hit_path[384], single_path[384], mutation_old[384], mutation_new[384];
    snprintf(hit_path, sizeof(hit_path), "%s/hit", base);
    snprintf(single_path, sizeof(single_path), "%s/single", base);
    snprintf(mutation_old, sizeof(mutation_old), "%s/mutation-old", base);
    snprintf(mutation_new, sizeof(mutation_new), "%s/mutation-new", base);
    if (create_file(hit_path, "hit") != 0 || create_file(single_path, "") != 0
        || create_file(mutation_old, "old") != 0) {
        return 2;
    }

    if (measure_counters) {
        if (phase_cache_hit(hit_path, expect_fast) != 0) {
            puts("FS4E_METADATA_PROBE_FAIL stage=cache_hit");
            return 1;
        }
        if (phase_single_flight(single_path, expect_fast) != 0) {
            puts("FS4E_METADATA_PROBE_FAIL stage=single_flight");
            return 1;
        }
    }
    int dirfd = open(base, O_RDONLY | O_DIRECTORY);
    if (dirfd < 0 || phase_syscall_control() != 0 || phase_scaling(dirfd, name_refs) != 0
        || phase_cold_shared_read_scaling() != 0) {
        puts("FS4E_METADATA_PROBE_FAIL stage=scaling");
        return 1;
    }
    close(dirfd);
    if (phase_mutation(mutation_old, mutation_new) != 0) {
        puts("FS4E_METADATA_PROBE_FAIL stage=mutation");
        return 1;
    }
    puts("FS4E_METADATA_PROBE_PASS");

    unlink(hit_path);
    unlink(single_path);
    unlink(mutation_new);
    for (int index = 0; index < MAX_WORKERS; ++index) {
        unlink(paths[index]);
    }
    rmdir(base);
    return 0;
}
