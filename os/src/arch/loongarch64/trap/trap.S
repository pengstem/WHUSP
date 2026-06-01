    .altmacro
    .equ KSAVE_KSP,  0x30
    .equ KSAVE_CTX,  0x31
    .equ KSAVE_USP,  0x32
    .equ LA_CSR_PRMD, 0x1
    .equ LA_CSR_ESTAT, 0x5
    .equ LA_CSR_ERA, 0x6
    .equ LA_CSR_PGDL, 0x19
    .equ LA_CSR_PGDH, 0x1a
    .equ LA_CSR_PGD, 0x1b
    .equ LA_CSR_TLBRSAVE, 0x8b
    .equ FCC_OFFSET, 69
    .equ FCSR_OFFSET, 70

    .macro SAVE_REGS
        st.d    $ra, $sp,  1*8
        st.d    $tp, $sp,  2*8
        st.d    $a0, $sp,  4*8
        st.d    $a1, $sp,  5*8
        st.d    $a2, $sp,  6*8
        st.d    $a3, $sp,  7*8
        st.d    $a4, $sp,  8*8
        st.d    $a5, $sp,  9*8
        st.d    $a6, $sp, 10*8
        st.d    $a7, $sp, 11*8
        st.d    $t0, $sp, 12*8
        st.d    $t1, $sp, 13*8
        st.d    $t2, $sp, 14*8
        st.d    $t3, $sp, 15*8
        st.d    $t4, $sp, 16*8
        st.d    $t5, $sp, 17*8
        st.d    $t6, $sp, 18*8
        st.d    $t7, $sp, 19*8
        st.d    $t8, $sp, 20*8
        st.d    $r21,$sp, 21*8
        st.d    $fp, $sp, 22*8
        st.d    $s0, $sp, 23*8
        st.d    $s1, $sp, 24*8
        st.d    $s2, $sp, 25*8
        st.d    $s3, $sp, 26*8
        st.d    $s4, $sp, 27*8
        st.d    $s5, $sp, 28*8
        st.d    $s6, $sp, 29*8
        st.d    $s7, $sp, 30*8
        st.d    $s8, $sp, 31*8
        csrrd   $t0, KSAVE_USP
        st.d    $t0, $sp,  3*8
        csrrd   $t0, LA_CSR_PRMD
        st.d    $t0, $sp, 32*8
        csrrd   $t0, LA_CSR_ERA
        st.d    $t0, $sp, 33*8
    .endm

    .macro LOAD_REGS
        ld.d    $t0, $sp, 32*8
        csrwr   $t0, LA_CSR_PRMD
        ld.d    $t0, $sp, 33*8
        csrwr   $t0, LA_CSR_ERA
        ld.d    $ra, $sp,  1*8
        ld.d    $tp, $sp,  2*8
        ld.d    $a0, $sp,  4*8
        ld.d    $a1, $sp,  5*8
        ld.d    $a2, $sp,  6*8
        ld.d    $a3, $sp,  7*8
        ld.d    $a4, $sp,  8*8
        ld.d    $a5, $sp,  9*8
        ld.d    $a6, $sp, 10*8
        ld.d    $a7, $sp, 11*8
        ld.d    $t0, $sp, 12*8
        ld.d    $t1, $sp, 13*8
        ld.d    $t2, $sp, 14*8
        ld.d    $t3, $sp, 15*8
        ld.d    $t4, $sp, 16*8
        ld.d    $t5, $sp, 17*8
        ld.d    $t6, $sp, 18*8
        ld.d    $t7, $sp, 19*8
        ld.d    $t8, $sp, 20*8
        ld.d    $r21,$sp, 21*8
        ld.d    $fp, $sp, 22*8
        ld.d    $s0, $sp, 23*8
        ld.d    $s1, $sp, 24*8
        ld.d    $s2, $sp, 25*8
        ld.d    $s3, $sp, 26*8
        ld.d    $s4, $sp, 27*8
        ld.d    $s5, $sp, 28*8
        ld.d    $s6, $sp, 29*8
        ld.d    $s7, $sp, 30*8
        ld.d    $s8, $sp, 31*8
        ld.d    $sp, $sp,  3*8
    .endm

    .macro SAVE_FP n
        fst.d   $f\n, $sp, (37+\n)*8
    .endm

    .macro LOAD_FP n
        fld.d   $f\n, $sp, (37+\n)*8
    .endm

    .macro SAVE_FP_REGS
        .set n, 0
        .rept 32
            SAVE_FP %n
            .set n, n+1
        .endr
        movcf2gr    $t0, $fcc0
        move        $t1, $t0
        movcf2gr    $t0, $fcc1
        bstrins.d   $t1, $t0, 15, 8
        movcf2gr    $t0, $fcc2
        bstrins.d   $t1, $t0, 23, 16
        movcf2gr    $t0, $fcc3
        bstrins.d   $t1, $t0, 31, 24
        movcf2gr    $t0, $fcc4
        bstrins.d   $t1, $t0, 39, 32
        movcf2gr    $t0, $fcc5
        bstrins.d   $t1, $t0, 47, 40
        movcf2gr    $t0, $fcc6
        bstrins.d   $t1, $t0, 55, 48
        movcf2gr    $t0, $fcc7
        bstrins.d   $t1, $t0, 63, 56
        st.d        $t1, $sp, FCC_OFFSET*8
        movfcsr2gr  $t0, $fcsr0
        st.w        $t0, $sp, FCSR_OFFSET*8
    .endm

    .macro LOAD_FP_REGS
        .set n, 0
        .rept 32
            LOAD_FP %n
            .set n, n+1
        .endr
        ld.d        $t0, $sp, FCC_OFFSET*8
        bstrpick.d  $t1, $t0, 7, 0
        movgr2cf    $fcc0, $t1
        bstrpick.d  $t1, $t0, 15, 8
        movgr2cf    $fcc1, $t1
        bstrpick.d  $t1, $t0, 23, 16
        movgr2cf    $fcc2, $t1
        bstrpick.d  $t1, $t0, 31, 24
        movgr2cf    $fcc3, $t1
        bstrpick.d  $t1, $t0, 39, 32
        movgr2cf    $fcc4, $t1
        bstrpick.d  $t1, $t0, 47, 40
        movgr2cf    $fcc5, $t1
        bstrpick.d  $t1, $t0, 55, 48
        movgr2cf    $fcc6, $t1
        bstrpick.d  $t1, $t0, 63, 56
        movgr2cf    $fcc7, $t1
        ld.w        $t0, $sp, FCSR_OFFSET*8
        movgr2fcsr  $fcsr0, $t0
    .endm

    .section .text.trampoline
    .globl __trap_vector_base
    .globl __restore
    .globl __tlb_refill
    .align 12
__trap_vector_base:
    csrwr   $sp, KSAVE_USP
    csrrd   $sp, LA_CSR_PRMD
    andi    $sp, $sp, 0x3
    bnez    $sp, 1f

    csrrd   $sp, KSAVE_USP
    addi.d  $sp, $sp, -37*8
    SAVE_REGS
    move    $a0, $sp
    la.global $t0, trap_from_kernel
    jirl    $ra, $t0, 0
    LOAD_REGS
    ertn

1:
    csrrd   $sp, KSAVE_CTX
    SAVE_REGS
    SAVE_FP_REGS
    ld.d    $t0, $sp, 34*8
    # csrwr writes the old CSR value back to its source register.
    move    $t1, $t0
    csrwr   $t0, LA_CSR_PGDL
    csrwr   $t1, LA_CSR_PGDH
    dbar    0
    invtlb  0x00, $r0, $r0
    ld.d    $t1, $sp, 36*8
    ld.d    $sp, $sp, 35*8
    jirl    $zero, $t1, 0

__restore:
    ld.d    $t0, $a0, 34*8
    move    $t1, $a1
    move    $t2, $t0
    csrwr   $a0, KSAVE_CTX
    csrwr   $t1, LA_CSR_PGDL
    csrwr   $t2, LA_CSR_PGDH
    dbar    0
    invtlb  0x00, $r0, $r0
    csrrd   $sp, KSAVE_CTX
    LOAD_FP_REGS
    LOAD_REGS
    ertn

    .align 12
__tlb_refill:
    csrwr   $t0, LA_CSR_TLBRSAVE
    csrrd   $t0, LA_CSR_PGD
    lddir   $t0, $t0, 3
    lddir   $t0, $t0, 1
    ldpte   $t0, 0
    ldpte   $t0, 1
    tlbfill
    csrrd   $t0, LA_CSR_TLBRSAVE
    ertn
