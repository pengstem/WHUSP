#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

enum { TIMER_INTERNAL_ERROR = 125 };

static void report_errno(const char *program, const char *operation)
{
    int saved_errno = errno;

    fprintf(stderr, "%s: %s: %s\n", program, operation,
            strerror(saved_errno));
}

static int write_all(int fd, const char *buffer, size_t length)
{
    while (length > 0) {
        ssize_t written = write(fd, buffer, length);

        if (written < 0) {
            if (errno == EINTR)
                continue;
            return -1;
        }
        if (written == 0) {
            errno = EIO;
            return -1;
        }

        buffer += written;
        length -= (size_t)written;
    }

    return 0;
}

static int monotonic_elapsed_ns(const struct timespec *start,
                                const struct timespec *end,
                                uint64_t *elapsed_ns)
{
    uint64_t seconds;
    long nanoseconds;

    if (end->tv_sec < start->tv_sec ||
        (end->tv_sec == start->tv_sec && end->tv_nsec < start->tv_nsec)) {
        errno = ERANGE;
        return -1;
    }

    seconds = (uint64_t)(end->tv_sec - start->tv_sec);
    nanoseconds = end->tv_nsec - start->tv_nsec;
    if (nanoseconds < 0) {
        --seconds;
        nanoseconds += 1000000000L;
    }

    if (seconds > (UINT64_MAX - (uint64_t)nanoseconds) / 1000000000ULL) {
        errno = ERANGE;
        return -1;
    }

    *elapsed_ns = seconds * 1000000000ULL + (uint64_t)nanoseconds;
    return 0;
}

static int propagated_status(int status)
{
    if (WIFEXITED(status))
        return WEXITSTATUS(status);
    if (WIFSIGNALED(status))
        return 128 + WTERMSIG(status);
    return TIMER_INTERNAL_ERROR;
}

int main(int argc, char **argv)
{
    const char *program = argv[0];
    const char *result_path;
    struct timespec start;
    struct timespec end;
    uint64_t elapsed_ns;
    pid_t child;
    pid_t waited;
    int result_fd;
    int status;
    int exited;
    int exit_code;
    int signaled;
    int signal_number;
    char result[192];
    int result_length;

    if (argc < 3) {
        fprintf(stderr, "usage: %s RESULT_FILE COMMAND [ARG ...]\n", program);
        return TIMER_INTERNAL_ERROR;
    }

    result_path = argv[1];
    result_fd = open(result_path, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC,
                     0600);
    if (result_fd < 0) {
        report_errno(program, "open result file");
        return TIMER_INTERNAL_ERROR;
    }

    if (clock_gettime(CLOCK_MONOTONIC, &start) < 0) {
        report_errno(program, "clock_gettime start");
        close(result_fd);
        return TIMER_INTERNAL_ERROR;
    }

    child = fork();
    if (child < 0) {
        report_errno(program, "fork");
        close(result_fd);
        return TIMER_INTERNAL_ERROR;
    }

    if (child == 0) {
        execvp(argv[2], &argv[2]);
        report_errno(program, "exec");
        _exit(127);
    }

    do {
        waited = waitpid(child, &status, 0);
    } while (waited < 0 && errno == EINTR);

    if (waited < 0) {
        report_errno(program, "waitpid");
        close(result_fd);
        return TIMER_INTERNAL_ERROR;
    }

    if (clock_gettime(CLOCK_MONOTONIC, &end) < 0) {
        report_errno(program, "clock_gettime end");
        close(result_fd);
        return TIMER_INTERNAL_ERROR;
    }

    if (monotonic_elapsed_ns(&start, &end, &elapsed_ns) < 0) {
        report_errno(program, "compute elapsed time");
        close(result_fd);
        return TIMER_INTERNAL_ERROR;
    }

    exited = WIFEXITED(status) ? 1 : 0;
    exit_code = exited ? WEXITSTATUS(status) : -1;
    signaled = WIFSIGNALED(status) ? 1 : 0;
    signal_number = signaled ? WTERMSIG(status) : 0;

    result_length = snprintf(result, sizeof(result),
                             "elapsed_ns=%" PRIu64
                             " exited=%d exit_code=%d signaled=%d signal=%d\n",
                             elapsed_ns, exited, exit_code, signaled,
                             signal_number);
    if (result_length < 0 || (size_t)result_length >= sizeof(result)) {
        errno = EOVERFLOW;
        report_errno(program, "format result");
        close(result_fd);
        return TIMER_INTERNAL_ERROR;
    }

    if (write_all(result_fd, result, (size_t)result_length) < 0) {
        report_errno(program, "write result file");
        close(result_fd);
        return TIMER_INTERNAL_ERROR;
    }

    if (close(result_fd) < 0) {
        report_errno(program, "close result file");
        return TIMER_INTERNAL_ERROR;
    }

    return propagated_status(status);
}
