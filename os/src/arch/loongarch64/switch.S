    .section .text
    .globl __switch
__switch:
    st.d      $sp, $a0,  0*8
    st.d      $tp, $a0,  1*8
    st.d      $s9, $a0,  2*8
    st.d      $s0, $a0,  3*8
    st.d      $s1, $a0,  4*8
    st.d      $s2, $a0,  5*8
    st.d      $s3, $a0,  6*8
    st.d      $s4, $a0,  7*8
    st.d      $s5, $a0,  8*8
    st.d      $s6, $a0,  9*8
    st.d      $s7, $a0, 10*8
    st.d      $s8, $a0, 11*8
    st.d      $ra, $a0, 12*8

    ld.d      $sp, $a1,  0*8
    ld.d      $tp, $a1,  1*8
    ld.d      $s9, $a1,  2*8
    ld.d      $s0, $a1,  3*8
    ld.d      $s1, $a1,  4*8
    ld.d      $s2, $a1,  5*8
    ld.d      $s3, $a1,  6*8
    ld.d      $s4, $a1,  7*8
    ld.d      $s5, $a1,  8*8
    ld.d      $s6, $a1,  9*8
    ld.d      $s7, $a1, 10*8
    ld.d      $s8, $a1, 11*8
    ld.d      $ra, $a1, 12*8
    ret
