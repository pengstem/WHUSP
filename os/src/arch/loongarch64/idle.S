    .section .text
    .align 4
    .globl __whusp_arch_cpu_idle
    .globl __whusp_idle_enter
    .globl __whusp_idle_exit
    .type __whusp_arch_cpu_idle, @function
__whusp_arch_cpu_idle:
__whusp_idle_enter:
    # LoongArch IDLE requires CRMD.IE=1. Keep these instructions in one
    # rollback region: the kernel trap handler redirects ERA to idle_exit when
    # an interrupt lands after IE is enabled but before IDLE executes.
    ori     $t0, $zero, 0x4
    csrxchg $t0, $t0, 0x0
    idle    0
__whusp_idle_exit:
    jirl    $zero, $ra, 0
    .size __whusp_arch_cpu_idle, .-__whusp_arch_cpu_idle
