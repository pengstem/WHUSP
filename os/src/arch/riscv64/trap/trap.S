.altmacro
.option arch, +d
.macro SAVE_GP n
    sd x\n, \n*8(sp)
.endm
.macro LOAD_GP n
    ld x\n, \n*8(sp)
.endm
.macro SAVE_FP n
    fsd f\n, (37+\n)*8(sp)
.endm
.macro LOAD_FP n
    fld f\n, (37+\n)*8(sp)
.endm
.equ SSTATUS_FS_DIRTY, 0x6000
.equ SSTATUS_FS_MASK, 0x6000
.equ FCSR_OFFSET, 69
    .section .text.trampoline
    .globl __alltraps
    .globl __restore
    .globl __alltraps_k
    .globl __restore_k
    .align 2
__alltraps:
    csrrw sp, sscratch, sp
    # now sp->*TrapContext in user space, sscratch->user stack
    # save other general purpose registers
    sd x1, 1*8(sp)
    # skip sp(x2), we will save it later
    sd x3, 3*8(sp)
    sd x4, 4*8(sp)
    # save x5~x31
    .set n, 5
    .rept 27
        SAVE_GP %n
        .set n, n+1
    .endr
    # we can use t0/t1/t2 freely, because they have been saved in TrapContext
    csrr t0, sstatus
    csrr t1, sepc
    sd t0, 32*8(sp)
    sd t1, 33*8(sp)
    # read user stack from sscratch and save it in TrapContext
    csrr t2, sscratch
    sd t2, 2*8(sp)
    # CONTEXT: User FP state lives in TrapContext in this kernel; switch.S only
    # carries kernel scheduling state. Temporarily enable FS while copying it.
    li t2, SSTATUS_FS_DIRTY
    csrs sstatus, t2
    .set n, 0
    .rept 32
        SAVE_FP %n
        .set n, n+1
    .endr
    frcsr t2
    sw t2, FCSR_OFFSET*8(sp)
    li t2, SSTATUS_FS_MASK
    csrc sstatus, t2
    # load kernel_satp into t0
    ld t0, 34*8(sp)
    # load trap_handler into t1
    ld t1, 36*8(sp)
    # move to kernel_sp
    ld sp, 35*8(sp)
    # switch to kernel space (satp: Supervisor Address Translation and Protection)
    csrw satp, t0
    sfence.vma
    # jump to trap_handler
    jr t1

__restore:
    # a0: *TrapContext in user space(Constant); a1: user space token
    # switch to user space
    csrw satp, a1
    sfence.vma
    csrw sscratch, a0
    mv sp, a0
    # now sp points to TrapContext in user space, start restoring based on it
    # restore sstatus/sepc
    ld t0, 32*8(sp)
    ld t1, 33*8(sp)
    li t2, SSTATUS_FS_DIRTY
    or t2, t0, t2
    csrw sstatus, t2
    csrw sepc, t1
    .set n, 0
    .rept 32
        LOAD_FP %n
        .set n, n+1
    .endr
    lw t2, FCSR_OFFSET*8(sp)
    fscsr t2
    csrw sstatus, t0
    # restore general purpose registers except x0/sp
    ld x1, 1*8(sp)
    ld x3, 3*8(sp)
    ld x4, 4*8(sp)
    .set n, 5
    .rept 27
        LOAD_GP %n
        .set n, n+1
    .endr
    # back to user stack
    ld sp, 2*8(sp)
    sret

    .align 2
__alltraps_k:
    addi sp, sp, -34*8 
    sd x1, 1*8(sp)
    sd x3, 3*8(sp)
    .set n, 5
    .rept 27
        SAVE_GP %n
        .set n, n+1
    .endr
    csrr t0, sstatus
    csrr t1, sepc
    sd t0, 32*8(sp)
    sd t1, 33*8(sp)
    mv a0, sp
    csrr t2, sscratch
    jalr t2

__restore_k:
    ld t0, 32*8(sp)
    ld t1, 33*8(sp)
    csrw sstatus, t0
    csrw sepc, t1
    ld x1, 1*8(sp)
    ld x3, 3*8(sp)
    .set n, 5
    .rept 27
        LOAD_GP %n
        .set n, n+1
    .endr
    addi sp, sp, 34*8
    sret
