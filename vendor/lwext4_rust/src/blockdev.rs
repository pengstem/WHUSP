use core::{
    ffi::{c_int, c_void},
    mem, ptr, slice,
};

use crate::{Ext4Result, error::Context, ffi::*};
use alloc::boxed::Box;

/// Device block size.
pub const EXT4_DEV_BSIZE: usize = 512;

pub trait BlockDevice: Send + Sync {
    /// Whether this device's owning lwext4 core may have concurrent cache
    /// callers. Serialized cores leave the C callbacks disabled and retain the
    /// legacy uncontended fast path.
    fn concurrent_bcache(&self) -> bool {
        false
    }

    /// Writes blocks to the device, starting from the given block ID.
    fn write_blocks(&self, block_id: u64, buf: &[u8]) -> Ext4Result<usize>;

    /// Reads blocks from the device, starting from the given block ID.
    fn read_blocks(&self, block_id: u64, buf: &mut [u8]) -> Ext4Result<usize>;

    /// Gets the number of blocks on the device.
    fn num_blocks(&self) -> Ext4Result<u64>;

    /// Acquires the short bookkeeping lock for this core's metadata cache.
    fn lock_bcache_index(&self);

    /// Releases one matching metadata-cache bookkeeping acquisition.
    ///
    /// # Safety
    ///
    /// The caller must own the cache-index lock exactly once.
    unsafe fn unlock_bcache_index(&self);

    /// Acquires the ownership shard for one logical filesystem block.
    fn lock_bcache_lba(&self, lba: u64);

    /// Releases one matching logical-block ownership acquisition.
    ///
    /// # Safety
    ///
    /// The caller must own the shard selected by `lba` exactly once.
    unsafe fn unlock_bcache_lba(&self, lba: u64);
}

/// Holds necessary resources for the ext4 block device, and automatically frees
/// them when the instance is dropped.
#[allow(dead_code)]
struct ResourceGuard<Dev> {
    dev: Box<Dev>,
    block_buf: Box<[u8; EXT4_DEV_BSIZE]>,
    block_cache_buf: Box<ext4_bcache>,
    block_dev_iface: Box<ext4_blockdev_iface>,
}

pub struct Ext4BlockDevice<Dev: BlockDevice> {
    pub(crate) inner: Box<ext4_blockdev>,
    _guard: ResourceGuard<Dev>,
}

impl<Dev: BlockDevice> Ext4BlockDevice<Dev> {
    pub fn new(dev: Dev) -> Ext4Result<Self> {
        let mut dev = Box::new(dev);
        let concurrent_bcache = dev.concurrent_bcache();

        // Block size buffer
        let mut block_buf = Box::new([0u8; EXT4_DEV_BSIZE]);
        let mut block_dev_iface = Box::new(ext4_blockdev_iface {
            open: Some(Self::dev_open),
            bread: Some(Self::dev_bread),
            bwrite: Some(Self::dev_bwrite),
            close: Some(Self::dev_close),
            lock: None,
            unlock: None,
            bcache_index_lock: concurrent_bcache.then_some(Self::dev_bcache_index_lock),
            bcache_index_unlock: concurrent_bcache.then_some(Self::dev_bcache_index_unlock),
            bcache_lba_lock: concurrent_bcache.then_some(Self::dev_bcache_lba_lock),
            bcache_lba_unlock: concurrent_bcache.then_some(Self::dev_bcache_lba_unlock),
            ph_bsize: EXT4_DEV_BSIZE as u32,
            ph_bcnt: 0,
            ph_bbuf: block_buf.as_mut_ptr(),
            ph_refctr: 0,
            bread_ctr: 0,
            bwrite_ctr: 0,
            p_user: dev.as_mut() as *mut _ as *mut c_void,
        });

        let mut block_cache_buf: Box<ext4_bcache> = Box::new(unsafe { mem::zeroed() });
        let mut blockdev = Box::new(ext4_blockdev {
            bdif: block_dev_iface.as_mut(),
            part_offset: 0,
            part_size: 0,
            bc: block_cache_buf.as_mut(),
            lg_bsize: 0,
            lg_bcnt: 0,
            cache_write_back: 0,
            fs: ptr::null_mut(),
            journal: ptr::null_mut(),
        });

        unsafe {
            ext4_block_init(blockdev.as_mut()).context("ext4_block_init")?;
            ext4_block_cache_write_back(blockdev.as_mut(), 1)
                .context("ext4_block_cache_write_back")
                .inspect_err(|_| {
                    ext4_block_fini(blockdev.as_mut());
                })?;
        }
        Ok(Self {
            inner: blockdev,
            _guard: ResourceGuard {
                dev,
                block_buf,
                block_cache_buf,
                block_dev_iface,
            },
        })
    }

    unsafe fn iface_and_dev<'a>(bdev: *mut ext4_blockdev) -> (*mut ext4_blockdev_iface, &'a Dev) {
        let bdif = unsafe { (*bdev).bdif };
        let dev = unsafe { &*((*bdif).p_user as *const Dev) };
        (bdif, dev)
    }
    unsafe extern "C" fn dev_open(bdev: *mut ext4_blockdev) -> c_int {
        debug!("open ext4 block device");
        let (bdif, dev) = unsafe { Self::iface_and_dev(bdev) };

        let blocks = match dev.num_blocks() {
            Ok(cur) => cur,
            Err(err) => {
                error!("num_blocks failed: {err:?}");
                return EIO as _;
            }
        };

        unsafe {
            (*bdif).ph_bcnt = blocks;
            (*bdev).part_offset = 0;
            (*bdev).part_size = blocks * (*bdif).ph_bsize as u64;
        }
        EOK as _
    }
    unsafe extern "C" fn dev_bread(
        bdev: *mut ext4_blockdev,
        buf: *mut c_void,
        blk_id: u64,
        blk_cnt: u32,
    ) -> c_int {
        trace!("read ext4 block id={blk_id} count={blk_cnt}");
        if blk_cnt == 0 {
            return EOK as _;
        }

        let (bdif, dev) = unsafe { Self::iface_and_dev(bdev) };
        let buf_len = unsafe { ((*bdif).ph_bsize * blk_cnt) as usize };
        let buffer = unsafe { slice::from_raw_parts_mut(buf as *mut u8, buf_len) };
        if let Err(err) = dev.read_blocks(blk_id, buffer) {
            error!("read_blocks failed: {err:?}");
            return EIO as _;
        }

        EOK as _
    }
    unsafe extern "C" fn dev_bwrite(
        bdev: *mut ext4_blockdev,
        buf: *const c_void,
        blk_id: u64,
        blk_cnt: u32,
    ) -> c_int {
        trace!("write ext4 block id={blk_id} count={blk_cnt}");
        if blk_cnt == 0 {
            return EOK as _;
        }

        let (bdif, dev) = unsafe { Self::iface_and_dev(bdev) };
        let buf_len = unsafe { ((*bdif).ph_bsize * blk_cnt) as usize };
        let buffer = unsafe { slice::from_raw_parts(buf as *const u8, buf_len) };
        if let Err(err) = dev.write_blocks(blk_id, buffer) {
            error!("read_blocks failed: {err:?}");
            return EIO as _;
        }

        // drop_cache();
        // sync

        EOK as _
    }
    unsafe extern "C" fn dev_close(_bdev: *mut ext4_blockdev) -> c_int {
        debug!("close ext4 block device");
        EOK as _
    }

    unsafe extern "C" fn dev_bcache_index_lock(bdev: *mut ext4_blockdev) -> c_int {
        let (_, dev) = unsafe { Self::iface_and_dev(bdev) };
        dev.lock_bcache_index();
        EOK as _
    }

    unsafe extern "C" fn dev_bcache_index_unlock(bdev: *mut ext4_blockdev) -> c_int {
        let (_, dev) = unsafe { Self::iface_and_dev(bdev) };
        unsafe { dev.unlock_bcache_index() };
        EOK as _
    }

    unsafe extern "C" fn dev_bcache_lba_lock(bdev: *mut ext4_blockdev, lba: u64) -> c_int {
        let (_, dev) = unsafe { Self::iface_and_dev(bdev) };
        dev.lock_bcache_lba(lba);
        EOK as _
    }

    unsafe extern "C" fn dev_bcache_lba_unlock(bdev: *mut ext4_blockdev, lba: u64) -> c_int {
        let (_, dev) = unsafe { Self::iface_and_dev(bdev) };
        unsafe { dev.unlock_bcache_lba(lba) };
        EOK as _
    }
}

impl<Dev: BlockDevice> Drop for Ext4BlockDevice<Dev> {
    fn drop(&mut self) {
        unsafe {
            let bdev = self.inner.as_mut();
            ext4_block_fini(bdev);
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::Ext4Error;
    use alloc::{sync::Arc, vec, vec::Vec};
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::{
        sync::{Barrier, Mutex},
        thread,
        time::Duration,
    };

    struct TestRawLock {
        held: AtomicBool,
    }

    impl TestRawLock {
        fn new() -> Self {
            Self {
                held: AtomicBool::new(false),
            }
        }

        fn lock(&self) {
            while self
                .held
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                thread::yield_now();
            }
        }

        unsafe fn unlock(&self) {
            assert!(
                self.held.swap(false, Ordering::Release),
                "test cache lock released without ownership"
            );
        }
    }

    struct TestShared {
        storage: Mutex<Vec<u8>>,
        index: TestRawLock,
        lba: Vec<TestRawLock>,
        read_calls: AtomicUsize,
        write_calls: AtomicUsize,
        active_reads: AtomicUsize,
        active_read_hwm: AtomicUsize,
        fail_next_read: AtomicBool,
    }

    impl TestShared {
        fn new(blocks: usize) -> Self {
            let mut storage = vec![0; blocks * EXT4_DEV_BSIZE];
            for (block, bytes) in storage.chunks_exact_mut(EXT4_DEV_BSIZE).enumerate() {
                bytes.fill(block as u8);
            }
            Self {
                storage: Mutex::new(storage),
                index: TestRawLock::new(),
                lba: (0..32).map(|_| TestRawLock::new()).collect(),
                read_calls: AtomicUsize::new(0),
                write_calls: AtomicUsize::new(0),
                active_reads: AtomicUsize::new(0),
                active_read_hwm: AtomicUsize::new(0),
                fail_next_read: AtomicBool::new(false),
            }
        }

        fn lba_lock(&self, lba: u64) -> &TestRawLock {
            &self.lba[lba as usize % self.lba.len()]
        }
    }

    struct TestDevice {
        shared: Arc<TestShared>,
    }

    impl BlockDevice for TestDevice {
        fn concurrent_bcache(&self) -> bool {
            true
        }

        fn write_blocks(&self, block_id: u64, buf: &[u8]) -> Ext4Result<usize> {
            assert!(
                !self.shared.index.held.load(Ordering::Acquire),
                "device write executed while the bcache index was locked"
            );
            self.shared.write_calls.fetch_add(1, Ordering::SeqCst);
            let start = block_id as usize * EXT4_DEV_BSIZE;
            self.shared.storage.lock().unwrap()[start..start + buf.len()].copy_from_slice(buf);
            Ok(buf.len())
        }

        fn read_blocks(&self, block_id: u64, buf: &mut [u8]) -> Ext4Result<usize> {
            assert!(
                !self.shared.index.held.load(Ordering::Acquire),
                "device read executed while the bcache index was locked"
            );
            self.shared.read_calls.fetch_add(1, Ordering::SeqCst);
            if self.shared.fail_next_read.swap(false, Ordering::SeqCst) {
                return Err(Ext4Error::new(EIO as _, "injected cache read failure"));
            }
            let active = self.shared.active_reads.fetch_add(1, Ordering::SeqCst) + 1;
            self.shared
                .active_read_hwm
                .fetch_max(active, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(5));
            let start = block_id as usize * EXT4_DEV_BSIZE;
            buf.copy_from_slice(&self.shared.storage.lock().unwrap()[start..start + buf.len()]);
            self.shared.active_reads.fetch_sub(1, Ordering::SeqCst);
            Ok(buf.len())
        }

        fn num_blocks(&self) -> Ext4Result<u64> {
            Ok((self.shared.storage.lock().unwrap().len() / EXT4_DEV_BSIZE) as u64)
        }

        fn lock_bcache_index(&self) {
            self.shared.index.lock();
        }

        unsafe fn unlock_bcache_index(&self) {
            unsafe { self.shared.index.unlock() };
        }

        fn lock_bcache_lba(&self, lba: u64) {
            self.shared.lba_lock(lba).lock();
        }

        unsafe fn unlock_bcache_lba(&self, lba: u64) {
            unsafe { self.shared.lba_lock(lba).unlock() };
        }
    }

    fn test_bcache(cache_blocks: u32) -> (Ext4BlockDevice<TestDevice>, Arc<TestShared>) {
        let shared = Arc::new(TestShared::new(64));
        let mut bdev = Ext4BlockDevice::new(TestDevice {
            shared: shared.clone(),
        })
        .unwrap();
        unsafe {
            let raw = bdev.inner.as_mut();
            ext4_block_set_lb_size(raw, EXT4_DEV_BSIZE as u32);
            assert_eq!(
                ext4_bcache_init_dynamic(raw.bc, cache_blocks, EXT4_DEV_BSIZE as u32),
                EOK as i32
            );
            assert_eq!(ext4_block_bind_bcache(raw, raw.bc), EOK as i32);
        }
        (bdev, shared)
    }

    unsafe fn finish_bcache(bdev: &mut Ext4BlockDevice<TestDevice>) {
        unsafe {
            ext4_bcache_cleanup(bdev.inner.as_mut().bc);
            assert_eq!(ext4_bcache_fini_dynamic(bdev.inner.as_mut().bc), EOK as i32);
        }
    }

    #[test]
    fn same_lba_cold_miss_is_single_flight() {
        let (mut bdev, shared) = test_bcache(16);
        let raw = bdev.inner.as_mut() as *mut ext4_blockdev as usize;
        let barrier = Arc::new(Barrier::new(8));
        thread::scope(|scope| {
            for _ in 0..8 {
                let barrier = barrier.clone();
                scope.spawn(move || unsafe {
                    barrier.wait();
                    let mut block: ext4_block = mem::zeroed();
                    assert_eq!(
                        ext4_block_get(raw as *mut ext4_blockdev, &mut block, 7),
                        EOK as i32
                    );
                    assert_eq!(*block.data, 7);
                    assert_eq!(
                        ext4_block_set(raw as *mut ext4_blockdev, &mut block),
                        EOK as i32
                    );
                });
            }
        });
        assert_eq!(shared.read_calls.load(Ordering::SeqCst), 1);
        unsafe { finish_bcache(&mut bdev) };
    }

    #[test]
    fn independent_misses_overlap_and_failed_fill_retries() {
        let (mut bdev, shared) = test_bcache(16);
        let raw = bdev.inner.as_mut() as *mut ext4_blockdev as usize;
        let barrier = Arc::new(Barrier::new(2));
        thread::scope(|scope| {
            for lba in [3, 5] {
                let barrier = barrier.clone();
                scope.spawn(move || unsafe {
                    barrier.wait();
                    let mut block: ext4_block = mem::zeroed();
                    assert_eq!(
                        ext4_block_get(raw as *mut ext4_blockdev, &mut block, lba),
                        EOK as i32
                    );
                    assert_eq!(*block.data, lba as u8);
                    assert_eq!(
                        ext4_block_set(raw as *mut ext4_blockdev, &mut block),
                        EOK as i32
                    );
                });
            }
        });
        assert!(shared.active_read_hwm.load(Ordering::SeqCst) >= 2);

        shared.fail_next_read.store(true, Ordering::SeqCst);
        unsafe {
            let mut failed: ext4_block = mem::zeroed();
            assert_eq!(
                ext4_block_get(raw as *mut ext4_blockdev, &mut failed, 11),
                EIO as i32
            );
            let mut retry: ext4_block = mem::zeroed();
            assert_eq!(
                ext4_block_get(raw as *mut ext4_blockdev, &mut retry, 11),
                EOK as i32
            );
            assert_eq!(*retry.data, 11);
            assert_eq!(
                ext4_block_set(raw as *mut ext4_blockdev, &mut retry),
                EOK as i32
            );
            finish_bcache(&mut bdev);
        }
    }

    #[test]
    fn full_cache_evicts_only_unreferenced_buffers() {
        let (mut bdev, _) = test_bcache(2);
        unsafe {
            for lba in 1..9 {
                let mut block: ext4_block = mem::zeroed();
                assert_eq!(
                    ext4_block_get(bdev.inner.as_mut(), &mut block, lba),
                    EOK as i32
                );
                assert_eq!(ext4_block_set(bdev.inner.as_mut(), &mut block), EOK as i32);
                assert!((*bdev.inner.as_mut().bc).ref_blocks <= 2);
            }
            finish_bcache(&mut bdev);
        }
    }

    #[test]
    fn dirty_eviction_flushes_without_holding_the_index() {
        let (mut bdev, shared) = test_bcache(1);
        unsafe {
            let mut dirty: ext4_block = mem::zeroed();
            assert_eq!(
                ext4_block_get(bdev.inner.as_mut(), &mut dirty, 1),
                EOK as i32
            );
            *dirty.data = 0xa5;
            (*dirty.buf).flags |= 1i32 << bcache_state_bits_BC_DIRTY;
            assert_eq!(ext4_block_set(bdev.inner.as_mut(), &mut dirty), EOK as i32);

            let mut replacement: ext4_block = mem::zeroed();
            assert_eq!(
                ext4_block_get(bdev.inner.as_mut(), &mut replacement, 2),
                EOK as i32
            );
            assert_eq!(
                ext4_block_set(bdev.inner.as_mut(), &mut replacement),
                EOK as i32
            );
            assert_eq!(shared.write_calls.load(Ordering::SeqCst), 1);
            assert_eq!(shared.storage.lock().unwrap()[EXT4_DEV_BSIZE], 0xa5);
            finish_bcache(&mut bdev);
        }
    }
}
