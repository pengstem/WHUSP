    .section .text.entry
    .globl _start
_start:
    ori         $t0, $zero, 0x1
    lu52i.d     $t0, $t0, -2048
    csrwr       $t0, 0x180
    ori         $t0, $zero, 0x11
    lu52i.d     $t0, $t0, -1792
    csrwr       $t0, 0x181

    li.w        $t0, 0xb0
    csrwr       $t0, 0x0
    li.w        $t0, 0x0
    csrwr       $t0, 0x1
    li.w        $t0, 0x3
    csrwr       $t0, 0x2

    la.global   $sp, boot_stack_top
    csrrd       $a0, 0x20
    li.d        $a1, 0x9000000000100000
    la.global   $t0, rust_main
    jirl        $zero, $t0, 0

    .section .bss.stack
    .globl boot_stack_lower_bound
boot_stack_lower_bound:
    .space 4096 * 16
    .globl boot_stack_top
boot_stack_top:
