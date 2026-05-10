#define _GNU_SOURCE

#include <sched.h>
#include <sys/syscall.h>
#include <unistd.h>

int sched_getparam(pid_t pid, struct sched_param *param)
{
    return syscall(SYS_sched_getparam, pid, param);
}

int sched_getscheduler(pid_t pid)
{
    return syscall(SYS_sched_getscheduler, pid);
}

int sched_setparam(pid_t pid, const struct sched_param *param)
{
    return syscall(SYS_sched_setparam, pid, param);
}

int sched_setscheduler(pid_t pid, int policy, const struct sched_param *param)
{
    return syscall(SYS_sched_setscheduler, pid, policy, param);
}
