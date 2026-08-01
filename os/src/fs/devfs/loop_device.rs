use super::super::File;
use crate::sync::SpinNoIrqLock;
use alloc::string::String;
use alloc::sync::Arc;
use lazy_static::lazy_static;

pub(super) const LOOP_DEVICE_COUNT: usize = 2;
const LOOP_DEVICE_SIZE_FALLBACK: u64 = 300 * 1024 * 1024;
const LOOP_DEVICE_BLOCK_SIZE_DEFAULT: usize = 512;
const LOOP_DEVICE_DEFAULT_READ_AHEAD: usize = 128;
pub(super) const LOOP_FLAG_READ_ONLY: u32 = 1;
pub(super) const LOOP_FLAG_AUTOCLEAR: u32 = 4;
pub(super) const LOOP_FLAG_PARTSCAN: u32 = 8;
pub(super) const LOOP_FLAG_DIRECT_IO: u32 = 16;

lazy_static! {
    pub(super) static ref LOOP0_STATE: SpinNoIrqLock<LoopDeviceState> =
        SpinNoIrqLock::new(LoopDeviceState::new());
    pub(super) static ref LOOP1_STATE: SpinNoIrqLock<LoopDeviceState> =
        SpinNoIrqLock::new(LoopDeviceState::new());
}

pub(super) struct LoopDeviceState {
    pub(super) backend: Option<Arc<dyn File + Send + Sync>>,
    pub(super) backing_path: Option<String>,
    pub(super) flags: u32,
    pub(super) read_ahead: usize,
    pub(super) block_size: usize,
    pub(super) size: u64,
    pub(super) size_limit: u64,
    pub(super) synthetic_write_sectors: u64,
    pub(super) synthetic_io_ticks: u64,
}

impl LoopDeviceState {
    fn new() -> Self {
        Self {
            backend: None,
            backing_path: None,
            flags: 0,
            read_ahead: LOOP_DEVICE_DEFAULT_READ_AHEAD,
            block_size: LOOP_DEVICE_BLOCK_SIZE_DEFAULT,
            size: LOOP_DEVICE_SIZE_FALLBACK,
            size_limit: 0,
            synthetic_write_sectors: 0,
            synthetic_io_ticks: 0,
        }
    }

    pub(super) fn reset(&mut self) {
        *self = Self::new();
    }

    pub(super) fn read_only(&self) -> bool {
        self.flags & LOOP_FLAG_READ_ONLY != 0
    }

    pub(super) fn visible_size(&self) -> u64 {
        if self.size_limit == 0 {
            self.size
        } else {
            self.size.min(self.size_limit)
        }
    }

    pub(super) fn set_flag(&mut self, flag: u32, enabled: bool) {
        if enabled {
            self.flags |= flag;
        } else {
            self.flags &= !flag;
        }
    }
}
