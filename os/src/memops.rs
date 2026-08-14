//! Freestanding memory primitives used by Rust and the vendored C filesystem.
//!
//! `compiler_builtins` supplies a weak byte-at-a-time `memset` on the current
//! bare-metal targets. Large zero fills, especially fresh 4 KiB user frames,
//! dominate that implementation. Keep the generic unaligned edges, but use
//! aligned native-width stores for the bulk of every request.

use core::ptr;

const WORD_BYTES: usize = size_of::<usize>();
const UNROLL_WORDS: usize = 8;
const UNROLL_BYTES: usize = WORD_BYTES * UNROLL_WORDS;

#[unsafe(no_mangle)]
pub unsafe extern "C" fn memset(dest: *mut u8, value: i32, len: usize) -> *mut u8 {
    let fill = value as u8;
    let mut cursor = dest;
    let mut remaining = len;

    while remaining != 0 && !(cursor as usize).is_multiple_of(WORD_BYTES) {
        unsafe { ptr::write_volatile(cursor, fill) };
        cursor = unsafe { cursor.add(1) };
        remaining -= 1;
    }

    let repeated = usize::from_ne_bytes([fill; WORD_BYTES]);
    while remaining >= UNROLL_BYTES {
        let words = cursor.cast::<usize>();
        unsafe {
            ptr::write_volatile(words.add(0), repeated);
            ptr::write_volatile(words.add(1), repeated);
            ptr::write_volatile(words.add(2), repeated);
            ptr::write_volatile(words.add(3), repeated);
            ptr::write_volatile(words.add(4), repeated);
            ptr::write_volatile(words.add(5), repeated);
            ptr::write_volatile(words.add(6), repeated);
            ptr::write_volatile(words.add(7), repeated);
            cursor = cursor.add(UNROLL_BYTES);
        }
        remaining -= UNROLL_BYTES;
    }

    while remaining >= WORD_BYTES {
        unsafe { ptr::write_volatile(cursor.cast::<usize>(), repeated) };
        cursor = unsafe { cursor.add(WORD_BYTES) };
        remaining -= WORD_BYTES;
    }

    while remaining != 0 {
        unsafe { ptr::write_volatile(cursor, fill) };
        cursor = unsafe { cursor.add(1) };
        remaining -= 1;
    }

    dest
}

/// Covers the alignment prologue, unrolled word body, word tail, and byte
/// epilogue before allocator or filesystem state can depend on this routine.
pub fn self_test() {
    let mut bytes = [0xa5_u8; 97];
    let start = 3;
    let len = 89;
    let dest = unsafe { bytes.as_mut_ptr().add(start) };
    let returned = unsafe { memset(dest, 0x5a, len) };
    assert_eq!(returned, dest, "memset returned a different destination");
    assert!(bytes[..start].iter().all(|byte| *byte == 0xa5));
    assert!(bytes[start..start + len].iter().all(|byte| *byte == 0x5a));
    assert!(bytes[start + len..].iter().all(|byte| *byte == 0xa5));
}
