    .section .text.entry
    .globl _start
    .equ BOOT_STACK_SHIFT, 16
    .equ MAX_CPUS, 8
_start:
    la sp, boot_stack_lower_bound
    li t0, 1
    slli t0, t0, BOOT_STACK_SHIFT
    add sp, sp, t0
    # Phase 1 uses tp=logical_id+1 only while CPUs remain on boot stacks.
    # Phase 2 replaces this with the permanent CpuLocal pointer contract.
    li tp, 1
    call rust_main

    .globl secondary_entry
secondary_entry:
    li t0, MAX_CPUS
    bgeu a1, t0, secondary_entry_failed
    la sp, boot_stack_lower_bound
    slli t0, a1, BOOT_STACK_SHIFT
    add sp, sp, t0
    li t0, 1
    slli t0, t0, BOOT_STACK_SHIFT
    add sp, sp, t0
    addi tp, a1, 1
    tail rust_secondary_main

secondary_entry_failed:
    wfi
    j secondary_entry_failed

    .section .bss.stack
    .globl boot_stack_lower_bound
boot_stack_lower_bound:
    .space (1 << BOOT_STACK_SHIFT) * MAX_CPUS
    .globl boot_stack_top
boot_stack_top:
