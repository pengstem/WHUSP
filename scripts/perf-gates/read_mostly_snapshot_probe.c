#define _GNU_SOURCE
#include <errno.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define READ_MOSTLY_SYSCALL 0x5f55
#define CMD_RESET 0
#define CMD_READ 1
#define CMD_PUBLISH 2
#define CMD_STAT 3
#define CMD_NESTED_READ 4
#define CMD_GATED_READ 5
#define CMD_GATED_NESTED_READ 6
#define CMD_RELEASE_READERS 7

#define STAT_ACTIVE_READERS 0
#define STAT_MAX_ACTIVE_READERS 1
#define STAT_READ_COMPLETIONS 2
#define STAT_PUBLISH_COMPLETIONS 3
#define STAT_CURRENT_VALUE 4
#define STAT_NESTED_INNER_DONE 5
#define STAT_VIOLATIONS 6
#define STAT_PUBLISH_ATTEMPTS 7

static long probe_call(long command, long arg)
{
    return syscall(READ_MOSTLY_SYSCALL, command, arg);
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
        long value = probe_call(CMD_STAT, metric);
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

static int pin_to_cpu(int cpu)
{
    cpu_set_t set;
    CPU_ZERO(&set);
    CPU_SET(cpu, &set);
    return sched_setaffinity(0, sizeof(set), &set);
}

static pid_t spawn_worker(int command, int arg, int start_fd, int cpu)
{
    pid_t pid = fork();
    if (pid != 0) {
        return pid;
    }
    if (pin_to_cpu(cpu) != 0) {
        _exit(30);
    }
    if (start_fd >= 0) {
        char token;
        if (read(start_fd, &token, 1) != 1) {
            _exit(31);
        }
        close(start_fd);
    }
    errno = 0;
    _exit(probe_call(command, arg) >= 0 ? 0 : 32);
}

static int reset_probe(void)
{
    return probe_call(CMD_RESET, 0) == 0 ? 0 : -1;
}

static int phase_parallel_readers(void)
{
    enum { READERS = 8 };
    int pipefd[2];
    pid_t pids[READERS];
    if (reset_probe() != 0 || pipe(pipefd) != 0) {
        return -1;
    }
    for (int index = 0; index < READERS; ++index) {
        pids[index] = spawn_worker(CMD_READ, 30000, pipefd[0], index % 7 + 1);
        if (pids[index] <= 0) {
            return -1;
        }
    }
    close(pipefd[0]);
    for (int index = 0; index < READERS; ++index) {
        if (write(pipefd[1], "x", 1) != 1) {
            return -1;
        }
    }
    close(pipefd[1]);
    for (int index = 0; index < READERS; ++index) {
        if (wait_one(pids[index], 3000) != 0) {
            return -1;
        }
    }
    return probe_call(CMD_STAT, STAT_MAX_ACTIVE_READERS) >= 2
                   && probe_call(CMD_STAT, STAT_READ_COMPLETIONS) == READERS
                   && probe_call(CMD_STAT, STAT_VIOLATIONS) == 0
               ? 0
               : -1;
}

static int phase_publish_grace_period(void)
{
    if (reset_probe() != 0) {
        return -1;
    }
    pid_t reader = spawn_worker(CMD_GATED_READ, 0, -1, 1);
    if (reader <= 0 || wait_stat(STAT_ACTIVE_READERS, 1, 1000) != 0) {
        return -1;
    }
    pid_t publisher = spawn_worker(CMD_PUBLISH, 17, -1, 2);
    if (publisher <= 0 || wait_stat(STAT_PUBLISH_ATTEMPTS, 1, 1000) != 0) {
        return -1;
    }
    if (probe_call(CMD_STAT, STAT_PUBLISH_COMPLETIONS) != 0
        || probe_call(CMD_STAT, STAT_ACTIVE_READERS) < 1) {
        return -1;
    }
    if (probe_call(CMD_RELEASE_READERS, 0) != 0) {
        return -1;
    }
    if (wait_one(reader, 3000) != 0 || wait_one(publisher, 3000) != 0) {
        return -1;
    }
    return probe_call(CMD_STAT, STAT_CURRENT_VALUE) == 17
                   && probe_call(CMD_STAT, STAT_PUBLISH_COMPLETIONS) == 1
                   && probe_call(CMD_STAT, STAT_VIOLATIONS) == 0
               ? 0
               : -1;
}

static int phase_nested_reader(void)
{
    if (reset_probe() != 0) {
        return -1;
    }
    pid_t reader = spawn_worker(CMD_GATED_NESTED_READ, 0, -1, 1);
    if (reader <= 0 || wait_stat(STAT_NESTED_INNER_DONE, 1, 1000) != 0) {
        return -1;
    }
    pid_t publisher = spawn_worker(CMD_PUBLISH, 23, -1, 2);
    if (publisher <= 0 || wait_stat(STAT_PUBLISH_ATTEMPTS, 1, 1000) != 0) {
        return -1;
    }
    if (probe_call(CMD_STAT, STAT_PUBLISH_COMPLETIONS) != 0
        || probe_call(CMD_STAT, STAT_ACTIVE_READERS) != 1) {
        return -1;
    }
    if (probe_call(CMD_RELEASE_READERS, 0) != 0) {
        return -1;
    }
    if (wait_one(reader, 3000) != 0 || wait_one(publisher, 3000) != 0) {
        return -1;
    }
    return probe_call(CMD_STAT, STAT_CURRENT_VALUE) == 23
                   && probe_call(CMD_STAT, STAT_READ_COMPLETIONS) == 1
                   && probe_call(CMD_STAT, STAT_PUBLISH_COMPLETIONS) == 1
                   && probe_call(CMD_STAT, STAT_VIOLATIONS) == 0
               ? 0
               : -1;
}

int main(void)
{
    if (pin_to_cpu(0) != 0) {
        puts("READ_MOSTLY_CASE_FAIL case=affinity_setup");
        return 1;
    }
    if (phase_parallel_readers() != 0) {
        puts("READ_MOSTLY_CASE_FAIL case=parallel_readers");
        return 1;
    }
    puts("READ_MOSTLY_CASE_PASS case=parallel_readers");
    if (phase_publish_grace_period() != 0) {
        puts("READ_MOSTLY_CASE_FAIL case=publish_grace_period");
        return 1;
    }
    puts("READ_MOSTLY_CASE_PASS case=publish_grace_period");
    if (phase_nested_reader() != 0) {
        puts("READ_MOSTLY_CASE_FAIL case=nested_reader");
        return 1;
    }
    puts("READ_MOSTLY_CASE_PASS case=nested_reader");
    puts("READ_MOSTLY_PROBE_PASS cases=3");
    return 0;
}
