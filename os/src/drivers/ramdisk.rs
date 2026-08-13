use crate::arch::mm::boot_ramdisk_phys_to_virt;
use crate::drivers::block_cache;
use core::ptr;

pub struct RamDiskBlock {
    base_addr: usize,
    mapped_addr: usize,
    size: usize,
    cache_key: usize,
}

impl RamDiskBlock {
    pub fn new(base_addr: usize, size: usize) -> Self {
        assert_ne!(size, 0, "boot ramdisk is empty");
        assert_eq!(
            base_addr % 512,
            0,
            "boot ramdisk base is not sector aligned"
        );
        assert_eq!(size % 512, 0, "boot ramdisk size is not sector aligned");
        Self {
            base_addr,
            mapped_addr: boot_ramdisk_phys_to_virt(base_addr, base_addr),
            size,
            cache_key: base_addr ^ usize::MAX.rotate_left(17),
        }
    }

    pub fn read_blocks(&self, block_id: usize, buf: &mut [u8]) {
        block_cache::read_with_cache(self.cache_key, block_id, buf, |block_id, buf| {
            self.read_blocks_uncached(block_id, buf)
        });
    }

    pub fn write_blocks(&self, block_id: usize, buf: &[u8]) {
        block_cache::write_with_cache(self.cache_key, block_id, buf, |block_id, buf| {
            self.write_blocks_uncached(block_id, buf)
        });
    }

    pub fn read_blocks_versioned_fill_for_file_plan(
        &self,
        block_id: usize,
        buf: &mut [u8],
    ) -> block_cache::VersionedReadStats {
        block_cache::read_with_cache_versioned_fill(
            self.cache_key,
            block_id,
            buf,
            |block_id, buf| self.read_blocks_uncached(block_id, buf),
        )
    }

    pub fn write_blocks_for_file_plan(
        &self,
        block_id: usize,
        buf: &[u8],
    ) -> block_cache::WriteStats {
        block_cache::write_with_cache(self.cache_key, block_id, buf, |block_id, buf| {
            self.write_blocks_uncached(block_id, buf)
        })
    }

    fn byte_offset(&self, block_id: usize, len: usize) -> usize {
        assert_eq!(len % 512, 0, "ramdisk I/O must use whole sectors");
        let start = block_id
            .checked_mul(512)
            .expect("ramdisk block offset overflow");
        let end = start.checked_add(len).expect("ramdisk I/O range overflow");
        assert!(end <= self.size, "ramdisk I/O exceeds image capacity");
        start
    }

    fn read_blocks_uncached(&self, block_id: usize, buf: &mut [u8]) {
        let offset = self.byte_offset(block_id, buf.len());
        unsafe {
            ptr::copy_nonoverlapping(
                (self.mapped_addr + offset) as *const u8,
                buf.as_mut_ptr(),
                buf.len(),
            )
        };
    }

    fn write_blocks_uncached(&self, block_id: usize, buf: &[u8]) {
        let offset = self.byte_offset(block_id, buf.len());
        unsafe {
            ptr::copy_nonoverlapping(
                buf.as_ptr(),
                (self.mapped_addr + offset) as *mut u8,
                buf.len(),
            )
        };
    }

    pub fn num_blocks(&self) -> u64 {
        (self.size / 512) as u64
    }

    pub fn base_addr(&self) -> usize {
        self.base_addr
    }
}
