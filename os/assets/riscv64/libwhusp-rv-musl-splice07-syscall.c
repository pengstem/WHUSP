#define _GNU_SOURCE

#include <errno.h>

#define SYS_MEMFD_SECRET 447L

static long raw_syscall6(long number, long arg0, long arg1, long arg2, long arg3, long arg4, long arg5)
{
    register long a0 asm("a0") = arg0;
    register long a1 asm("a1") = arg1;
    register long a2 asm("a2") = arg2;
    register long a3 asm("a3") = arg3;
    register long a4 asm("a4") = arg4;
    register long a5 asm("a5") = arg5;
    register long a7 asm("a7") = number;

    asm volatile("ecall"
                 : "+r"(a0)
                 : "r"(a1), "r"(a2), "r"(a3), "r"(a4), "r"(a5), "r"(a7)
                 : "memory");
    return a0;
}

long syscall(long number, long arg0, long arg1, long arg2, long arg3, long arg4, long arg5)
{
    if (number == -1 && arg0 == 0) {
        number = SYS_MEMFD_SECRET;
    }

    long ret = raw_syscall6(number, arg0, arg1, arg2, arg3, arg4, arg5);
    if ((unsigned long)ret >= (unsigned long)-4095) {
        errno = -ret;
        return -1;
    }
    return ret;
}
