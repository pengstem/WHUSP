#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

enum {
    DIRTY_FILES = 32,
    PAGE_BYTES = 4096,
    PAGES_PER_FILE = 128,
    FILE_BYTES = PAGE_BYTES * PAGES_PER_FILE,
    WRITE_BYTES = 64 * 1024,
    TRIGGER_BYTES = PAGE_BYTES,
};

static void fill_pattern(unsigned char *buffer, size_t length, int file_index, size_t offset)
{
    for (size_t index = 0; index < length; ++index) {
        buffer[index] = (unsigned char)(file_index * 37 + (offset + index) * 13 + 0x5b);
    }
}

static int write_exact(int fd, const void *buffer, size_t length)
{
    const unsigned char *bytes = buffer;
    size_t written = 0;
    while (written < length) {
        ssize_t amount = write(fd, bytes + written, length - written);
        if (amount <= 0) {
            return -1;
        }
        written += (size_t)amount;
    }
    return 0;
}

static int write_file(const char *path, int file_index, size_t length, unsigned char *buffer)
{
    int fd = open(path, O_CREAT | O_EXCL | O_WRONLY, 0600);
    if (fd < 0) {
        return -1;
    }
    for (size_t offset = 0; offset < length; offset += WRITE_BYTES) {
        size_t amount = length - offset;
        if (amount > WRITE_BYTES) {
            amount = WRITE_BYTES;
        }
        fill_pattern(buffer, amount, file_index, offset);
        if (write_exact(fd, buffer, amount) != 0) {
            close(fd);
            return -1;
        }
    }
    return close(fd);
}

static int verify_file(const char *path, int file_index, size_t length, unsigned char *observed,
                       unsigned char *expected)
{
    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        return -1;
    }
    for (size_t offset = 0; offset < length; offset += WRITE_BYTES) {
        size_t amount = length - offset;
        if (amount > WRITE_BYTES) {
            amount = WRITE_BYTES;
        }
        size_t received = 0;
        while (received < amount) {
            ssize_t result = pread(fd, observed + received, amount - received,
                                   (off_t)(offset + received));
            if (result <= 0) {
                close(fd);
                return -1;
            }
            received += (size_t)result;
        }
        fill_pattern(expected, amount, file_index, offset);
        if (memcmp(observed, expected, amount) != 0) {
            close(fd);
            errno = EIO;
            return -1;
        }
    }
    unsigned char extra = 0;
    if (pread(fd, &extra, 1, (off_t)length) != 0) {
        close(fd);
        errno = EIO;
        return -1;
    }
    return close(fd);
}

static int verify_all(const char *base, unsigned char *observed, unsigned char *expected)
{
    char path[512];
    for (int file_index = 0; file_index < DIRTY_FILES; ++file_index) {
        snprintf(path, sizeof(path), "%s/file-%02d", base, file_index);
        if (verify_file(path, file_index, FILE_BYTES, observed, expected) != 0) {
            return -1;
        }
    }
    snprintf(path, sizeof(path), "%s/trigger", base);
    return verify_file(path, DIRTY_FILES, TRIGGER_BYTES, observed, expected);
}

static int cleanup(const char *base)
{
    char path[512];
    for (int file_index = 0; file_index < DIRTY_FILES; ++file_index) {
        snprintf(path, sizeof(path), "%s/file-%02d", base, file_index);
        if (unlink(path) != 0) {
            return -1;
        }
    }
    snprintf(path, sizeof(path), "%s/trigger", base);
    if (unlink(path) != 0) {
        return -1;
    }
    return rmdir(base);
}

static int fail(const char *stage)
{
    printf("S0_2_DIRTY_PRESSURE_PROBE_FAIL stage=%s errno=%d\n", stage, errno);
    return 1;
}

int main(int argc, char **argv)
{
    if (argc != 2) {
        errno = EINVAL;
        return fail("arguments");
    }
    const char *base = argv[1];
    unsigned char *write_buffer = malloc(WRITE_BYTES);
    unsigned char *observed = malloc(WRITE_BYTES);
    unsigned char *expected = malloc(WRITE_BYTES);
    if (write_buffer == NULL || observed == NULL || expected == NULL) {
        return fail("allocate");
    }
    if (mkdir(base, 0700) != 0) {
        return fail("mkdir");
    }

    char path[512];
    for (int file_index = 0; file_index < DIRTY_FILES; ++file_index) {
        snprintf(path, sizeof(path), "%s/file-%02d", base, file_index);
        if (write_file(path, file_index, FILE_BYTES, write_buffer) != 0) {
            return fail("fill");
        }
    }
    puts("S0_2_DIRTY_PRESSURE_FILLED files=32 pages_per_file=128 total_pages=4096");

    snprintf(path, sizeof(path), "%s/trigger", base);
    if (write_file(path, DIRTY_FILES, TRIGGER_BYTES, write_buffer) != 0) {
        return fail("trigger");
    }
    puts("S0_2_DIRTY_PRESSURE_TRIGGER pages=1");
    if (verify_all(base, observed, expected) != 0) {
        return fail("verify_dirty");
    }
    puts("S0_2_DIRTY_PRESSURE_VERIFY phase=dirty ok=1");

    sync();
    if (verify_all(base, observed, expected) != 0) {
        return fail("verify_synced");
    }
    puts("S0_2_DIRTY_PRESSURE_VERIFY phase=synced ok=1");
    if (cleanup(base) != 0) {
        return fail("cleanup");
    }
    sync();
    free(expected);
    free(observed);
    free(write_buffer);
    puts("S0_2_DIRTY_PRESSURE_PROBE_PASS files=33 initial_pages=4096 trigger_pages=1 phases=2");
    return 0;
}
