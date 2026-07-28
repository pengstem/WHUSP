#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define FS4_RWLOCK_SYSCALL 0x5f54
#define CMD_RESET 0
#define CMD_READ 1
#define CMD_WRITE 2
#define CMD_WRITE_INTERRUPTIBLE 3
#define CMD_STAT 4
#define CMD_TRY_READ 5
#define CMD_TRY_WRITE 6

#define TAG_NONE 0
#define TAG_WRITER 1
#define TAG_LATE_READER 2

#define STAT_ACTIVE_READERS 0
#define STAT_ACTIVE_WRITERS 1
#define STAT_MAX_ACTIVE_READERS 2
#define STAT_VIOLATIONS 3
#define STAT_COMPLETIONS 4
#define STAT_WRITER_SEQUENCE 5
#define STAT_LATE_READER_SEQUENCE 6
#define STAT_WAITING_READERS 7
#define STAT_WAITING_WRITERS 8
#define STAT_MAX_WAITERS 9

static long probe_call(long command, long arg, long tag)
{
    return syscall(FS4_RWLOCK_SYSCALL, command, arg, tag);
}

static uint64_t monotonic_ms(void)
{
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) {
        return 0;
    }
    return (uint64_t)ts.tv_sec * 1000u + (uint64_t)ts.tv_nsec / 1000000u;
}

static int wait_stat(int metric, long expected_min, unsigned timeout_ms)
{
    uint64_t deadline = monotonic_ms() + timeout_ms;
    while (monotonic_ms() <= deadline) {
        long value = probe_call(CMD_STAT, metric, 0);
        if (value >= expected_min) {
            return 0;
        }
        usleep(1000);
    }
    return -1;
}

static int wait_one(pid_t pid, unsigned timeout_ms)
{
    uint64_t deadline = monotonic_ms() + timeout_ms;
    int status = 0;
    while (monotonic_ms() <= deadline) {
        pid_t result = waitpid(pid, &status, WNOHANG);
        if (result == pid) {
            return WIFEXITED(status) && WEXITSTATUS(status) == 0 ? 0 : -1;
        }
        if (result < 0) {
            return -1;
        }
        usleep(1000);
    }
    kill(pid, SIGKILL);
    waitpid(pid, &status, 0);
    return -1;
}

static void noop_handler(int signal_number)
{
    (void)signal_number;
}

static pid_t spawn_worker(int command, int duration_us, int tag, int start_fd, int expect_eintr)
{
    pid_t pid = fork();
    if (pid != 0) {
        return pid;
    }
    if (expect_eintr) {
        struct sigaction action;
        action.sa_handler = noop_handler;
        sigemptyset(&action.sa_mask);
        action.sa_flags = 0;
        if (sigaction(SIGUSR1, &action, NULL) != 0) {
            _exit(31);
        }
    }
    if (start_fd >= 0) {
        char token;
        if (read(start_fd, &token, 1) != 1) {
            _exit(32);
        }
        close(start_fd);
    }
    errno = 0;
    long result = probe_call(command, duration_us, tag);
    if (expect_eintr) {
        _exit(result == -1 && errno == EINTR ? 0 : 33);
    }
    _exit(result == 0 ? 0 : 34);
}

static int reset_probe(void)
{
    return probe_call(CMD_RESET, 0, 0) == 0 ? 0 : -1;
}

static int phase_parallel_readers(void)
{
    enum { READERS = 8 };
    int pipefd[2];
    pid_t pids[READERS];
    if (reset_probe() != 0 || pipe(pipefd) != 0) {
        return -1;
    }
    for (int i = 0; i < READERS; ++i) {
        pids[i] = spawn_worker(CMD_READ, 30000, TAG_NONE, pipefd[0], 0);
        if (pids[i] <= 0) {
            return -1;
        }
    }
    close(pipefd[0]);
    for (int i = 0; i < READERS; ++i) {
        if (write(pipefd[1], "x", 1) != 1) {
            return -1;
        }
    }
    close(pipefd[1]);
    for (int i = 0; i < READERS; ++i) {
        if (wait_one(pids[i], 3000) != 0) {
            return -1;
        }
    }
    return probe_call(CMD_STAT, STAT_MAX_ACTIVE_READERS, 0) >= 2
                   && probe_call(CMD_STAT, STAT_COMPLETIONS, 0) == READERS
                   && probe_call(CMD_STAT, STAT_VIOLATIONS, 0) == 0
               ? 0
               : -1;
}

static int phase_writer_exclusion(void)
{
    if (reset_probe() != 0) {
        return -1;
    }
    pid_t writer = spawn_worker(CMD_WRITE, 50000, TAG_NONE, -1, 0);
    if (writer <= 0 || wait_stat(STAT_ACTIVE_WRITERS, 1, 1000) != 0) {
        return -1;
    }
    if (probe_call(CMD_TRY_READ, 0, 0) != 0 || probe_call(CMD_TRY_WRITE, 0, 0) != 0) {
        return -1;
    }
    if (wait_one(writer, 3000) != 0) {
        return -1;
    }
    return probe_call(CMD_TRY_READ, 0, 0) == 1 && probe_call(CMD_TRY_WRITE, 0, 0) == 1
               && probe_call(CMD_STAT, STAT_VIOLATIONS, 0) == 0
               ? 0
               : -1;
}

static int phase_writer_fairness(void)
{
    enum { LATE_READERS = 4 };
    pid_t late[LATE_READERS];
    if (reset_probe() != 0) {
        return -1;
    }
    pid_t first_reader = spawn_worker(CMD_READ, 120000, TAG_NONE, -1, 0);
    if (first_reader <= 0 || wait_stat(STAT_ACTIVE_READERS, 1, 1000) != 0) {
        return -1;
    }
    pid_t writer = spawn_worker(CMD_WRITE, 5000, TAG_WRITER, -1, 0);
    if (writer <= 0 || wait_stat(STAT_WAITING_WRITERS, 1, 1000) != 0) {
        return -1;
    }
    for (int i = 0; i < LATE_READERS; ++i) {
        late[i] = spawn_worker(CMD_READ, 5000, TAG_LATE_READER, -1, 0);
        if (late[i] <= 0) {
            return -1;
        }
    }
    if (wait_one(first_reader, 3000) != 0 || wait_one(writer, 3000) != 0) {
        return -1;
    }
    for (int i = 0; i < LATE_READERS; ++i) {
        if (wait_one(late[i], 3000) != 0) {
            return -1;
        }
    }
    long writer_sequence = probe_call(CMD_STAT, STAT_WRITER_SEQUENCE, 0);
    long late_sequence = probe_call(CMD_STAT, STAT_LATE_READER_SEQUENCE, 0);
    return writer_sequence > 0 && late_sequence > writer_sequence
                   && probe_call(CMD_STAT, STAT_MAX_WAITERS, 0) >= LATE_READERS + 1
                   && probe_call(CMD_STAT, STAT_VIOLATIONS, 0) == 0
               ? 0
               : -1;
}

static int phase_interruption_cleanup(void)
{
    if (reset_probe() != 0) {
        return -1;
    }
    pid_t reader = spawn_worker(CMD_READ, 160000, TAG_NONE, -1, 0);
    if (reader <= 0 || wait_stat(STAT_ACTIVE_READERS, 1, 1000) != 0) {
        return -1;
    }
    pid_t writer = spawn_worker(CMD_WRITE_INTERRUPTIBLE, 0, TAG_NONE, -1, 1);
    if (writer <= 0 || wait_stat(STAT_WAITING_WRITERS, 1, 1000) != 0) {
        return -1;
    }
    if (kill(writer, SIGUSR1) != 0 || wait_one(writer, 3000) != 0) {
        return -1;
    }
    if (probe_call(CMD_STAT, STAT_WAITING_WRITERS, 0) != 0
        || probe_call(CMD_STAT, STAT_WAITING_READERS, 0) != 0) {
        return -1;
    }
    if (wait_one(reader, 3000) != 0 || probe_call(CMD_TRY_WRITE, 0, 0) != 1) {
        return -1;
    }
    return probe_call(CMD_STAT, STAT_VIOLATIONS, 0) == 0 ? 0 : -1;
}

int main(void)
{
    if (phase_parallel_readers() != 0) {
        puts("FS4_SLEEP_RWLOCK_CASE_FAIL case=parallel_readers");
        return 1;
    }
    puts("FS4_SLEEP_RWLOCK_CASE_PASS case=parallel_readers");
    if (phase_writer_exclusion() != 0) {
        puts("FS4_SLEEP_RWLOCK_CASE_FAIL case=writer_exclusion");
        return 1;
    }
    puts("FS4_SLEEP_RWLOCK_CASE_PASS case=writer_exclusion");
    if (phase_writer_fairness() != 0) {
        puts("FS4_SLEEP_RWLOCK_CASE_FAIL case=writer_fairness");
        return 1;
    }
    puts("FS4_SLEEP_RWLOCK_CASE_PASS case=writer_fairness");
    if (phase_interruption_cleanup() != 0) {
        puts("FS4_SLEEP_RWLOCK_CASE_FAIL case=interruption_cleanup");
        return 1;
    }
    puts("FS4_SLEEP_RWLOCK_CASE_PASS case=interruption_cleanup");
    puts("FS4_SLEEP_RWLOCK_PROBE_PASS cases=4");
    return 0;
}
