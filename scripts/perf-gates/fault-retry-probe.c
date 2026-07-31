#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <stdbool.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

enum {
    FILE_BYTES = 256 * 1024,
    READER_ITERATIONS = 2000,
    MUTATION_ITERATIONS = 256,
    MUTATION_STEPS = 8,
    CROSS_PAGE_BYTES = 128,
};

static unsigned char writer_buffer[FILE_BYTES];
static atomic_bool writer_ready;
static atomic_bool writer_request;
static atomic_bool writer_active;
static atomic_bool reader_ack;
static atomic_bool stop_writer;
static atomic_uint writer_errors;
static atomic_uint writer_calls;
static int backing_fd = -1;

static int pin_to_cpu(int cpu)
{
    cpu_set_t affinity;
    CPU_ZERO(&affinity);
    CPU_SET(cpu, &affinity);
    return sched_setaffinity(0, sizeof(affinity), &affinity);
}

static void *writer_main(void *unused)
{
    (void)unused;
    if (pin_to_cpu(1) != 0) {
        atomic_fetch_add_explicit(&writer_errors, 1, memory_order_relaxed);
        atomic_store_explicit(&writer_ready, true, memory_order_release);
        return NULL;
    }
    atomic_store_explicit(&writer_ready, true, memory_order_release);
    while (!atomic_load_explicit(&stop_writer, memory_order_acquire)) {
        if (!atomic_load_explicit(&writer_request, memory_order_acquire)) {
            sched_yield();
            continue;
        }
        atomic_store_explicit(&writer_active, true, memory_order_release);
        while (!atomic_load_explicit(&reader_ack, memory_order_acquire)) {
            if (atomic_load_explicit(&stop_writer, memory_order_acquire)) {
                return NULL;
            }
            sched_yield();
        }
        for (unsigned int step = 0; step < MUTATION_STEPS; ++step) {
            off_t length = FILE_BYTES + ((step & 1U) != 0 ? 4096 : 0);
            if (ftruncate(backing_fd, length) != 0) {
                atomic_fetch_add_explicit(&writer_errors, 1, memory_order_relaxed);
                break;
            }
            atomic_fetch_add_explicit(&writer_calls, 1, memory_order_relaxed);
        }
        atomic_store_explicit(&writer_active, false, memory_order_release);
        atomic_store_explicit(&reader_ack, false, memory_order_release);
        atomic_store_explicit(&writer_request, false, memory_order_release);
    }
    return NULL;
}

static int initialize_backing_file(int fd)
{
    memset(writer_buffer, 0x5a, sizeof(writer_buffer));
    ssize_t written = pwrite(fd, writer_buffer, sizeof(writer_buffer), 0);
    return written == (ssize_t)sizeof(writer_buffer) ? 0 : -1;
}

static int run_reader(int sink_fd, long page_size)
{
    const size_t map_len = (size_t)page_size * 2;
    const size_t start_offset = (size_t)page_size - CROSS_PAGE_BYTES / 2;

    while (!atomic_load_explicit(&writer_ready, memory_order_acquire)) {
        sched_yield();
    }

    for (unsigned int iteration = 0; iteration < READER_ITERATIONS; ++iteration) {
        unsigned char *mapping = mmap(
            NULL,
            map_len,
            PROT_READ,
            MAP_PRIVATE,
            backing_fd,
            0
        );
        if (mapping == MAP_FAILED) {
            fprintf(stderr, "FAULT_RETRY_PROBE_FAIL stage=mmap errno=%d\n", errno);
            return -1;
        }

        volatile unsigned char first_page = mapping[start_offset];
        (void)first_page;
        if (madvise(mapping + page_size, (size_t)page_size, MADV_DONTNEED) != 0) {
            fprintf(
                stderr,
                "FAULT_RETRY_PROBE_FAIL stage=madvise iteration=%u errno=%d\n",
                iteration,
                errno
            );
            munmap(mapping, map_len);
            return -1;
        }
        if (iteration < MUTATION_ITERATIONS) {
            atomic_store_explicit(&writer_request, true, memory_order_release);
            while (!atomic_load_explicit(&writer_active, memory_order_acquire)) {
                sched_yield();
            }
            atomic_store_explicit(&reader_ack, true, memory_order_release);
            for (volatile unsigned int spin = 0; spin < 1000; ++spin) {
            }
        }
        ssize_t written = write(sink_fd, mapping + start_offset, CROSS_PAGE_BYTES);
        int saved_errno = errno;
        if (iteration < MUTATION_ITERATIONS) {
            while (atomic_load_explicit(&writer_request, memory_order_acquire)) {
                sched_yield();
            }
        }
        if (munmap(mapping, map_len) != 0) {
            fprintf(stderr, "FAULT_RETRY_PROBE_FAIL stage=munmap errno=%d\n", errno);
            return -1;
        }
        if (written != CROSS_PAGE_BYTES) {
            fprintf(
                stderr,
                "FAULT_RETRY_PROBE_FAIL stage=cross_page_write iteration=%u rc=%zd errno=%d\n",
                iteration,
                written,
                saved_errno
            );
            return -1;
        }
    }
    return 0;
}

static int verify_bad_address(int sink_fd)
{
    volatile uintptr_t invalid_address = 1;
    errno = 0;
    ssize_t rc = write(sink_fd, (const void *)invalid_address, 16);
    if (rc != -1 || errno != EFAULT) {
        fprintf(
            stderr,
            "FAULT_RETRY_PROBE_FAIL stage=bad_address rc=%zd errno=%d\n",
            rc,
            errno
        );
        return -1;
    }
    return 0;
}

int main(int argc, char **argv)
{
    if (argc != 2) {
        fprintf(stderr, "usage: %s BASE_PATH\n", argv[0]);
        return 2;
    }

    char backing_path[512];
    char sink_path[512];
    if (snprintf(backing_path, sizeof(backing_path), "%s-backing", argv[1])
            >= (int)sizeof(backing_path)
        || snprintf(sink_path, sizeof(sink_path), "%s-sink", argv[1])
            >= (int)sizeof(sink_path)) {
        fprintf(stderr, "FAULT_RETRY_PROBE_FAIL stage=path_too_long\n");
        return 1;
    }

    unlink(backing_path);
    unlink(sink_path);
    backing_fd = open(backing_path, O_CREAT | O_TRUNC | O_RDWR, 0600);
    int sink_fd = open(sink_path, O_CREAT | O_TRUNC | O_WRONLY, 0600);
    if (backing_fd < 0 || sink_fd < 0 || initialize_backing_file(backing_fd) != 0) {
        fprintf(stderr, "FAULT_RETRY_PROBE_FAIL stage=open_or_initialize errno=%d\n", errno);
        return 1;
    }

    long page_size = sysconf(_SC_PAGESIZE);
    if (page_size <= 0) {
        fprintf(stderr, "FAULT_RETRY_PROBE_FAIL stage=page_size value=%ld\n", page_size);
        return 1;
    }
    if (pin_to_cpu(0) != 0) {
        fprintf(stderr, "FAULT_RETRY_PROBE_FAIL stage=reader_affinity errno=%d\n", errno);
        return 1;
    }

    pthread_t writer;
    if (pthread_create(&writer, NULL, writer_main, NULL) != 0) {
        fprintf(stderr, "FAULT_RETRY_PROBE_FAIL stage=pthread_create\n");
        return 1;
    }

    int reader_result = run_reader(sink_fd, page_size);
    atomic_store_explicit(&stop_writer, true, memory_order_release);
    int join_result = pthread_join(writer, NULL);
    int bad_address_result = verify_bad_address(sink_fd);
    unsigned int errors = atomic_load_explicit(&writer_errors, memory_order_relaxed);
    unsigned int writes = atomic_load_explicit(&writer_calls, memory_order_relaxed);

    close(sink_fd);
    close(backing_fd);
    unlink(backing_path);
    unlink(sink_path);

    if (reader_result != 0 || join_result != 0 || bad_address_result != 0 || errors != 0
        || writes == 0) {
        fprintf(
            stderr,
            "FAULT_RETRY_PROBE_FAIL stage=summary reader=%d join=%d bad_address=%d "
            "writer_errors=%u writer_calls=%u\n",
            reader_result,
            join_result,
            bad_address_result,
            errors,
            writes
        );
        return 1;
    }

    printf(
        "FAULT_RETRY_PROBE_PASS iterations=%u writer_calls=%u bad_address=EFAULT\n",
        READER_ITERATIONS,
        writes
    );
    return 0;
}
