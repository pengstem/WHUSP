use crate::timer::get_time_clock_ticks;
use core::sync::atomic::{AtomicU64, Ordering};

const STREAM_INCREMENT: u64 = 0x9e37_79b9_7f4a_7c15;
static RANDOM_REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

fn mix64(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

/// Kernel-wide compatibility stream for `getrandom(2)` and the random devices.
///
/// UNFINISHED: This generator has no cryptographic entropy pool and must not be
/// used as a security boundary. Reserving a unique request sequence prevents
/// independent callers from receiving the identical fixed stream that used to
/// collide in compiler temporary-file names.
pub(crate) struct CompatibilityRandom {
    state: u64,
    word: [u8; 8],
    word_offset: usize,
}

impl CompatibilityRandom {
    pub(crate) fn new(context: u64) -> Self {
        let sequence = RANDOM_REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let first_word = mix64(sequence.wrapping_add(STREAM_INCREMENT));
        Self {
            state: first_word ^ mix64(context) ^ get_time_clock_ticks() as u64,
            word: first_word.to_ne_bytes(),
            word_offset: 0,
        }
    }

    fn advance_word(&mut self) {
        self.state = self.state.wrapping_add(STREAM_INCREMENT);
        self.word = mix64(self.state).to_ne_bytes();
        self.word_offset = 0;
    }

    /// Fills one segment while retaining stream position across later segments.
    /// Returns `(byte_writes, word_fill_bytes)` for the existing perf counters.
    pub(crate) fn fill(&mut self, output: &mut [u8]) -> (usize, usize) {
        let mut offset = 0usize;
        let mut byte_writes = 0usize;
        let mut word_fill_bytes = 0usize;

        while offset < output.len() {
            if self.word_offset == self.word.len() {
                self.advance_word();
            }
            if self.word_offset == 0 && output.len() - offset >= self.word.len() {
                output[offset..offset + self.word.len()].copy_from_slice(&self.word);
                offset += self.word.len();
                self.word_offset = self.word.len();
                word_fill_bytes += self.word.len();
                continue;
            }
            output[offset] = self.word[self.word_offset];
            offset += 1;
            self.word_offset += 1;
            byte_writes += 1;
        }

        (byte_writes, word_fill_bytes)
    }
}
