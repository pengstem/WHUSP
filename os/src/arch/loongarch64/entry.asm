    .section .text.entry
    .globl _start
    .equ BOOT_STACK_SHIFT, 16
    .equ MAX_CPUS, 8
_start:
    ori         $t0, $zero, 0x1
    lu52i.d     $t0, $t0, -2048
    csrwr       $t0, 0x180
    ori         $t0, $zero, 0x11
    lu52i.d     $t0, $t0, -1792
    csrwr       $t0, 0x181
    # QEMU firmware enters the kernel at the low physical alias 0x90000000.
    # Keep that alias fetchable only long enough to jump to the high DMW1 alias.
    ori         $t0, $zero, 0x11
    csrwr       $t0, 0x182

    li.w        $t0, 0xb0
    csrwr       $t0, 0x0
    li.w        $t0, 0x0
    csrwr       $t0, 0x1
    li.w        $t0, 0x3
    csrwr       $t0, 0x2

    la.global   $t0, high_entry
    jirl        $zero, $t0, 0

    .globl high_entry
high_entry:
    li.w        $t0, 0x0
    csrwr       $t0, 0x182
    la.global   $sp, boot_stack_lower_bound
    li.d        $t0, 1
    slli.d      $t0, $t0, BOOT_STACK_SHIFT
    add.d       $sp, $sp, $t0
    csrrd       $a0, 0x20
    li.d        $a1, 0x9000000000100000
    la.global   $t0, rust_main
    jirl        $zero, $t0, 0

    # QEMU's parked application processors jump to this low physical alias
    # after mailbox 0 and boot IPI bit 0 are delivered.
    .globl secondary_entry
secondary_entry:
    ori         $t0, $zero, 0x1
    lu52i.d     $t0, $t0, -2048
    csrwr       $t0, 0x180
    ori         $t0, $zero, 0x11
    lu52i.d     $t0, $t0, -1792
    csrwr       $t0, 0x181
    ori         $t0, $zero, 0x11
    csrwr       $t0, 0x182

    li.w        $t0, 0xb0
    csrwr       $t0, 0x0
    li.w        $t0, 0x0
    csrwr       $t0, 0x1
    li.w        $t0, 0x3
    csrwr       $t0, 0x2

    la.global   $t0, secondary_high_entry
    jirl        $zero, $t0, 0

    .globl secondary_high_entry
secondary_high_entry:
    li.w        $t0, 0x0
    csrwr       $t0, 0x182
    dbar        0
    csrrd       $a0, 0x20
    la.global   $t0, CPU_EARLY_COUNT
    ld.d        $t1, $t0, 0
    la.global   $t2, CPU_EARLY_HW_IDS
    move        $t3, $zero

1:
    bgeu        $t3, $t1, secondary_entry_failed
    slli.d      $t4, $t3, 3
    add.d       $t5, $t2, $t4
    ld.d        $t6, $t5, 0
    beq         $t6, $a0, 2f
    addi.d      $t3, $t3, 1
    b           1b

2:
    move        $a1, $t3
    la.global   $sp, boot_stack_lower_bound
    slli.d      $t0, $a1, BOOT_STACK_SHIFT
    add.d       $sp, $sp, $t0
    li.d        $t0, 1
    slli.d      $t0, $t0, BOOT_STACK_SHIFT
    add.d       $sp, $sp, $t0
    la.global   $t0, rust_secondary_main
    jirl        $zero, $t0, 0

secondary_entry_failed:
    idle        0
    b           secondary_entry_failed

    .section .bss.stack
    .globl boot_stack_lower_bound
boot_stack_lower_bound:
    .space (1 << BOOT_STACK_SHIFT) * MAX_CPUS
    .globl boot_stack_top
boot_stack_top:
