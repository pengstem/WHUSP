use core::{marker::PhantomData, mem, time::Duration};

use alloc::{boxed::Box, vec::Vec};

use crate::{
    DirLookupResult, DirReader, Ext4DirectoryReadPlan, Ext4Error, Ext4MappedReadPlan,
    Ext4MappedWritePlan, Ext4Result, Ext4SymlinkReadPlan, FileAttr, InodeRef, InodeType,
    blockdev::{BlockDevice, Ext4BlockDevice},
    error::Context,
    ffi::*,
    util::get_block_size,
};

pub trait SystemHal {
    fn now() -> Option<Duration>;
}

pub struct DummyHal;
impl SystemHal for DummyHal {
    fn now() -> Option<Duration> {
        None
    }
}

#[derive(Debug, Clone)]
pub struct FsConfig {
    pub bcache_size: u32,
}
impl Default for FsConfig {
    fn default() -> Self {
        Self {
            bcache_size: CONFIG_BLOCK_DEV_CACHE_SIZE,
        }
    }
}

#[derive(Debug, Clone)]
pub struct StatFs {
    pub inodes_count: u32,
    pub free_inodes_count: u32,

    pub blocks_count: u64,
    pub free_blocks_count: u64,
    pub block_size: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct Ext4FlushProgress {
    pub complete: bool,
    pub pending_lba: u64,
    pub pending_refs: u32,
}

/// Fields from one in-core inode shadow that must be merged into its raw
/// ext4 inode. `None` preserves the value currently stored in the canonical
/// inode-table buffer.
#[derive(Clone, Copy, Debug, Default)]
pub struct InodeMetadataUpdate {
    pub mode: Option<u32>,
    pub uid: Option<u16>,
    pub gid: Option<u16>,
    pub atime: Option<Duration>,
    pub mtime: Option<Duration>,
    pub ctime: Option<Duration>,
    pub flags: Option<u32>,
}

pub struct Ext4Filesystem<Hal: SystemHal, Dev: BlockDevice> {
    inner: Box<ext4_fs>,
    bdev: Ext4BlockDevice<Dev>,
    finalized: bool,
    owns_superblock_state: bool,
    _phantom: PhantomData<Hal>,
}

impl<Hal: SystemHal, Dev: BlockDevice> Ext4Filesystem<Hal, Dev> {
    #[inline]
    fn inner_ptr(&self) -> *mut ext4_fs {
        self.inner.as_ref() as *const ext4_fs as *mut ext4_fs
    }

    pub fn new(dev: Dev, config: FsConfig) -> Ext4Result<Self> {
        Self::new_with_mode(dev, config, false)
    }

    /// Opens an independent metadata core that never modifies the filesystem.
    ///
    /// This is used by the kernel's sharded read replicas. Each replica owns a
    /// separate lwext4 block cache, while the writable core remains the only
    /// instance allowed to dirty metadata or the superblock.
    pub fn new_read_only(dev: Dev, config: FsConfig) -> Ext4Result<Self> {
        Self::new_with_mode(dev, config, true)
    }

    fn new_with_mode(dev: Dev, config: FsConfig, read_only: bool) -> Ext4Result<Self> {
        let mut bdev = Ext4BlockDevice::new(dev)?;
        let mut fs = Box::new(unsafe { mem::zeroed() });
        unsafe {
            let bd = bdev.inner.as_mut();
            ext4_fs_init(&mut *fs, bd, read_only).context("ext4_fs_init")?;

            let bs = get_block_size(&fs.sb);
            ext4_block_set_lb_size(bd, bs);
            ext4_bcache_init_dynamic(bd.bc, config.bcache_size, bs)
                .context("ext4_bcache_init_dynamic")?;
            if bs != (*bd.bc).itemsize {
                return Err(Ext4Error::new(ENOTSUP as _, "block size mismatch"));
            }

            bd.fs = &mut *fs;

            let mut result = Self {
                inner: fs,
                bdev,
                finalized: false,
                owns_superblock_state: !read_only,
                _phantom: PhantomData,
            };
            let bd = result.bdev.inner.as_mut();
            ext4_block_bind_bcache(bd, bd.bc).context("ext4_block_bind_bcache")?;
            Ok(result)
        }
    }

    /// Drops every unreferenced clean metadata buffer owned by this core.
    ///
    /// Callers must exclude operations on this exact core. Read-only replicas
    /// never own dirty buffers, so cleanup cannot write stale metadata back.
    pub fn invalidate_clean_cache(&mut self) {
        unsafe {
            ext4_bcache_cleanup(self.bdev.inner.as_mut().bc);
        }
    }

    /// Invalidates cached aliases only when every matching buffer was clean
    /// and unreferenced before this call acquired its temporary reference.
    ///
    /// `Some(n)` reports the number of aliases removed. `None` leaves every
    /// acquired buffer valid and tells the caller to keep the overwrite on the
    /// serialized lwext4 path. The owning core lock must exclude all other
    /// operations on this exact bcache.
    pub fn invalidate_clean_unreferenced_blocks(&mut self, blocks: &[u64]) -> Option<usize> {
        unsafe {
            let bcache = self.bdev.inner.as_mut().bc;
            let mut held: Vec<ext4_block> = Vec::with_capacity(blocks.len());
            let mut eligible = true;
            for &lba in blocks {
                let mut block: ext4_block = mem::zeroed();
                let buf = ext4_bcache_find_get(bcache, &mut block, lba);
                if buf.is_null() {
                    continue;
                }
                eligible &= ext4_bcache_block_is_clean_exclusive(bcache, &mut block);
                held.push(block);
                if !eligible {
                    break;
                }
            }
            if eligible {
                for block in &held {
                    ext4_bcache_invalidate_buf(bcache, block.buf);
                }
            }
            let found = held.len();
            for block in &mut held {
                let rc = ext4_bcache_free(bcache, block);
                debug_assert_eq!(rc, EOK as i32);
            }
            eligible.then_some(found)
        }
    }

    /// Returns whether direct device reads can observe the current payload of
    /// every listed filesystem block.
    ///
    /// A missing cache alias is already device-backed. A present alias must be
    /// clean, up to date, and owned only by this temporary probe; otherwise a
    /// caller may be modifying it or the newest bytes may exist only in the
    /// write-back cache. The caller must separately prevent a new mutation of
    /// the same logical object between this check and device-plan execution.
    pub fn device_snapshot_blocks_are_clean(&self, blocks: &[u64]) -> bool {
        unsafe {
            let bcache = self.bdev.inner.as_ref().bc;
            for &lba in blocks {
                let mut block: ext4_block = mem::zeroed();
                let buf = ext4_bcache_find_get(bcache, &mut block, lba);
                if buf.is_null() {
                    continue;
                }
                let clean = ext4_bcache_block_is_clean_exclusive(bcache, &mut block);
                let rc = ext4_bcache_free(bcache, &mut block);
                debug_assert_eq!(rc, EOK as i32);
                if rc != EOK as i32 || !clean {
                    return false;
                }
            }
            true
        }
    }

    fn inode_ref(&self, ino: u32) -> Ext4Result<InodeRef<Hal>> {
        unsafe {
            let mut result = mem::zeroed();
            ext4_fs_get_inode_ref(self.inner_ptr(), ino, &mut result)
                .context("ext4_fs_get_inode_ref")?;
            Ok(InodeRef::new(result))
        }
    }
    fn clone_ref(&self, inode: &InodeRef<Hal>) -> InodeRef<Hal> {
        self.inode_ref(inode.ino()).expect("inode ref clone failed")
    }

    pub fn with_inode_ref<R>(
        &mut self,
        ino: u32,
        f: impl FnOnce(&mut InodeRef<Hal>) -> Ext4Result<R>,
    ) -> Ext4Result<R> {
        let mut inode = self.inode_ref(ino)?;
        f(&mut inode)
    }

    pub(crate) fn alloc_inode(&self, ty: InodeType) -> Ext4Result<InodeRef<Hal>> {
        unsafe {
            let ty = match ty {
                InodeType::Fifo => EXT4_DE_FIFO,
                InodeType::CharacterDevice => EXT4_DE_CHRDEV,
                InodeType::Directory => EXT4_DE_DIR,
                InodeType::BlockDevice => EXT4_DE_BLKDEV,
                InodeType::RegularFile => EXT4_DE_REG_FILE,
                InodeType::Symlink => EXT4_DE_SYMLINK,
                InodeType::Socket => EXT4_DE_SOCK,
                InodeType::Unknown => EXT4_DE_UNKNOWN,
            };
            let mut result = mem::zeroed();
            ext4_fs_alloc_inode(self.inner_ptr(), &mut result, ty as _)
                .context("ext4_fs_get_inode_ref")?;
            let mut result = InodeRef::new(result);
            ext4_fs_inode_blocks_init(self.inner_ptr(), result.inner.as_mut());
            Ok(result)
        }
    }

    pub fn get_attr(&self, ino: u32, attr: &mut FileAttr) -> Ext4Result<()> {
        self.inode_ref(ino)?.get_attr(attr);
        Ok(())
    }

    pub fn read_at(&self, ino: u32, buf: &mut [u8], offset: u64) -> Ext4Result<usize> {
        self.inode_ref(ino)?.read_at(buf, offset)
    }
    pub fn plan_read(
        &self,
        ino: u32,
        len: usize,
        offset: u64,
    ) -> Ext4Result<Option<Ext4MappedReadPlan>> {
        self.inode_ref(ino)?.plan_read(len, offset)
    }
    pub fn plan_symlink_read(
        &self,
        ino: u32,
        len: usize,
    ) -> Ext4Result<Option<Ext4SymlinkReadPlan>> {
        self.inode_ref(ino)?.plan_symlink_read(len)
    }
    pub fn write_at(&mut self, ino: u32, buf: &[u8], offset: u64) -> Ext4Result<usize> {
        self.inode_ref(ino)?.write_at(buf, offset)
    }
    pub fn set_len(&mut self, ino: u32, len: u64) -> Ext4Result<()> {
        self.inode_ref(ino)?.set_len(len)
    }
    pub fn set_mode(&self, ino: u32, mode: u32) -> Ext4Result<()> {
        let mut inode = self.inode_ref(ino)?;
        let preserved_type = inode.mode() & !0o7777;
        inode.set_mode(preserved_type | (mode & 0o7777));
        Ok(())
    }
    pub fn inode_flags(&self, ino: u32) -> Ext4Result<u32> {
        Ok(self.inode_ref(ino)?.flags())
    }
    pub fn set_inode_flags(&self, ino: u32, flags: u32) -> Ext4Result<()> {
        self.inode_ref(ino)?.set_flags(flags);
        Ok(())
    }
    pub fn set_owner(&self, ino: u32, uid: u16, gid: u16) -> Ext4Result<()> {
        self.inode_ref(ino)?.set_owner(uid, gid);
        Ok(())
    }
    pub fn set_times(
        &self,
        ino: u32,
        atime: Option<Duration>,
        mtime: Option<Duration>,
        ctime: Option<Duration>,
    ) -> Ext4Result<()> {
        let mut inode = self.inode_ref(ino)?;
        if let Some(atime) = atime {
            inode.set_atime(&atime);
        }
        if let Some(mtime) = mtime {
            inode.set_mtime(&mtime);
        }
        if let Some(ctime) = ctime {
            inode.set_ctime(&ctime);
        }
        Ok(())
    }

    /// Merge the dirty fields of one in-core inode shadow with a single
    /// canonical inode-table reference. Unlisted fields, including nlink,
    /// size, block count, and the extent root, remain untouched.
    pub fn apply_inode_metadata(&self, ino: u32, update: InodeMetadataUpdate) -> Ext4Result<()> {
        let mut inode = self.inode_ref(ino)?;
        if let Some(mode) = update.mode {
            inode.set_mode(mode);
        }
        if update.uid.is_some() || update.gid.is_some() {
            inode.set_owner(
                update.uid.unwrap_or_else(|| inode.uid()),
                update.gid.unwrap_or_else(|| inode.gid()),
            );
        }
        if let Some(atime) = update.atime {
            inode.set_atime(&atime);
        }
        if let Some(mtime) = update.mtime {
            inode.set_mtime(&mtime);
        }
        if let Some(ctime) = update.ctime {
            inode.set_ctime(&ctime);
        }
        if let Some(flags) = update.flags {
            inode.set_flags(flags);
        }
        Ok(())
    }
    pub fn set_symlink(&mut self, ino: u32, buf: &[u8]) -> Ext4Result<()> {
        self.inode_ref(ino)?.set_symlink(buf)
    }
    pub fn lookup(&self, parent: u32, name: &str) -> Ext4Result<DirLookupResult<Hal>> {
        self.inode_ref(parent)?.lookup(name)
    }
    pub fn read_dir(&self, parent: u32, offset: u64) -> Ext4Result<DirReader<Hal>> {
        self.inode_ref(parent)?.read_dir(offset)
    }
    pub fn plan_directory_read(
        &self,
        parent: u32,
        offset: u64,
    ) -> Ext4Result<Option<Ext4DirectoryReadPlan>> {
        self.inode_ref(parent)?.plan_directory_read(offset)
    }

    pub fn plan_mapped_overwrite(
        &mut self,
        ino: u32,
        len: usize,
        offset: u64,
    ) -> Ext4Result<Option<Ext4MappedWritePlan>> {
        self.inode_ref(ino)?.plan_mapped_overwrite(len, offset)
    }

    pub fn create(&self, parent: u32, name: &str, ty: InodeType, mode: u32) -> Ext4Result<u32> {
        let mut child = self.alloc_inode(ty)?;
        let mut parent = self.inode_ref(parent)?;
        parent.add_entry(name, &mut child)?;
        if ty == InodeType::Directory {
            child.add_entry(".", &mut self.clone_ref(&child))?;
            child.add_entry("..", &mut parent)?;
            assert_eq!(child.nlink(), 2);
        }
        child.set_mode((child.mode() & !0o777) | (mode & 0o777));

        Ok(child.ino())
    }

    pub fn rename(
        &mut self,
        src_dir: u32,
        src_name: &str,
        dst_dir: u32,
        dst_name: &str,
    ) -> Ext4Result {
        let mut src_dir_ref = self.inode_ref(src_dir)?;
        let mut dst_dir_ref = self.inode_ref(dst_dir)?;

        // TODO: optimize
        match self.unlink(dst_dir, dst_name) {
            Ok(_) => {}
            Err(err) if err.code == ENOENT as i32 => {}
            Err(err) => return Err(err),
        }

        let src = self.lookup(src_dir, src_name)?.entry().ino();

        let mut src_ref = self.inode_ref(src)?;
        if src_ref.is_dir() {
            let mut result = self.clone_ref(&src_ref).lookup("..")?;
            result.entry().raw_entry_mut().set_ino(dst_dir);
            src_dir_ref.dec_nlink();
            dst_dir_ref.inc_nlink();
        }
        src_dir_ref.remove_entry(src_name, &mut src_ref)?;
        dst_dir_ref.add_entry(dst_name, &mut src_ref)?;

        Ok(())
    }

    pub fn exchange(
        &mut self,
        src_dir: u32,
        src_name: &str,
        dst_dir: u32,
        dst_name: &str,
    ) -> Ext4Result {
        let mut src_dir_ref = self.inode_ref(src_dir)?;
        let mut dst_dir_ref = self.inode_ref(dst_dir)?;
        let mut src_lookup = self.clone_ref(&src_dir_ref).lookup(src_name)?;
        let mut dst_lookup = self.clone_ref(&dst_dir_ref).lookup(dst_name)?;
        let mut src_entry = src_lookup.entry();
        let mut dst_entry = dst_lookup.entry();
        let src_ino = src_entry.ino();
        let dst_ino = dst_entry.ino();
        if src_ino == dst_ino {
            return Ok(());
        }

        let src_ref = self.inode_ref(src_ino)?;
        let dst_ref = self.inode_ref(dst_ino)?;
        let src_type = src_ref.inode_type();
        let dst_type = dst_ref.inode_type();
        let src_is_dir = src_ref.is_dir();
        let dst_is_dir = dst_ref.is_dir();
        let mut src_parent_lookup = if src_is_dir {
            Some(self.clone_ref(&src_ref).lookup("..")?)
        } else {
            None
        };
        let mut dst_parent_lookup = if dst_is_dir {
            Some(self.clone_ref(&dst_ref).lookup("..")?)
        } else {
            None
        };

        src_entry.set_ino_and_type(dst_ino, dst_type);
        dst_entry.set_ino_and_type(src_ino, src_type);
        if let Some(parent_lookup) = src_parent_lookup.as_mut() {
            parent_lookup.entry().raw_entry_mut().set_ino(dst_dir);
        }
        if let Some(parent_lookup) = dst_parent_lookup.as_mut() {
            parent_lookup.entry().raw_entry_mut().set_ino(src_dir);
        }
        if src_dir != dst_dir {
            match (src_is_dir, dst_is_dir) {
                (true, false) => {
                    src_dir_ref.dec_nlink();
                    dst_dir_ref.inc_nlink();
                }
                (false, true) => {
                    src_dir_ref.inc_nlink();
                    dst_dir_ref.dec_nlink();
                }
                _ => {}
            }
        }

        Ok(())
    }

    pub fn link(&mut self, dir: u32, name: &str, child: u32) -> Ext4Result {
        let mut child_ref = self.inode_ref(child)?;
        if child_ref.is_dir() {
            return Err(Ext4Error::new(EISDIR as _, "cannot link to directory"));
        }
        self.inode_ref(dir)?.add_entry(name, &mut child_ref)?;
        Ok(())
    }

    fn free_unlinked_inode_ref(inode_ref: &mut InodeRef<Hal>) -> Ext4Result {
        inode_ref.truncate(0)?;
        unsafe {
            ext4_inode_set_del_time(inode_ref.inner.inode, u32::MAX);
            inode_ref.mark_dirty();
            ext4_fs_free_inode(inode_ref.inner.as_mut());
        }
        Ok(())
    }

    pub fn free_unlinked_inode(&mut self, ino: u32) -> Ext4Result {
        let mut inode_ref = self.inode_ref(ino)?;
        if inode_ref.nlink() != 0 {
            return Ok(());
        }
        Self::free_unlinked_inode_ref(&mut inode_ref)
    }

    pub fn unlink(&mut self, dir: u32, name: &str) -> Ext4Result {
        self.unlink_maybe_defer_free(dir, name, false).map(|_| ())
    }

    pub fn unlink_defer_free(&mut self, dir: u32, name: &str) -> Ext4Result<Option<u32>> {
        self.unlink_maybe_defer_free(dir, name, true)
    }

    fn unlink_maybe_defer_free(
        &mut self,
        dir: u32,
        name: &str,
        defer_free: bool,
    ) -> Ext4Result<Option<u32>> {
        let mut dir_ref = self.inode_ref(dir)?;
        let child = self.clone_ref(&dir_ref).lookup(name)?.entry().ino();
        let mut child_ref = self.inode_ref(child)?;

        if self.clone_ref(&child_ref).has_children()? {
            return Err(Ext4Error::new(ENOTEMPTY as _, None));
        }
        if child_ref.inode_type() == InodeType::Directory {
            // According to `ext4_trunc_dir`
            let bs = get_block_size(&self.inner.as_mut().sb);
            child_ref.truncate(bs as _)?;
        }

        dir_ref.remove_entry(name, &mut child_ref)?;

        if child_ref.is_dir() {
            dir_ref.dec_nlink();
            child_ref.dec_nlink();
        }
        if child_ref.nlink() == 0 {
            if defer_free && !child_ref.is_dir() {
                return Ok(Some(child));
            }
            Self::free_unlinked_inode_ref(&mut child_ref)?;
        }
        Ok(None)
    }

    pub fn stat(&self) -> Ext4Result<StatFs> {
        let sb = &self.inner.as_ref().sb;
        Ok(StatFs {
            inodes_count: u32::from_le(sb.inodes_count),
            free_inodes_count: u32::from_le(sb.free_inodes_count),
            blocks_count: (u32::from_le(sb.blocks_count_hi) as u64) << 32
                | u32::from_le(sb.blocks_count_lo) as u64,
            free_blocks_count: (u32::from_le(sb.free_blocks_count_hi) as u64) << 32
                | u32::from_le(sb.free_blocks_count_lo) as u64,
            block_size: get_block_size(sb),
        })
    }

    pub fn flush(&self) -> Ext4Result<()> {
        unsafe {
            ext4_block_cache_flush(self.bdev.inner_ptr()).context("ext4_cache_flush")?;
        }
        Ok(())
    }

    /// Captures a durability boundary for every metadata buffer dirtied by
    /// this cache before the call.
    pub fn dirty_ticket(&self) -> u64 {
        unsafe { ext4_bcache_dirty_ticket(self.bdev.inner.as_ref().bc) }
    }

    /// Writes every zero-reference buffer covered by `ticket`. `false` means
    /// at least one covered buffer is still owned by a metadata caller; no
    /// referenced payload is submitted and the caller may yield before retry.
    pub fn flush_through(&self, ticket: u64) -> Ext4Result<Ext4FlushProgress> {
        let mut complete = false;
        let mut pending_lba = 0;
        let mut pending_refs = 0;
        unsafe {
            ext4_block_cache_flush_through(
                self.bdev.inner_ptr(),
                ticket,
                &mut complete,
                &mut pending_lba,
                &mut pending_refs,
            )
            .context("ext4_cache_flush_through")?;
        }
        Ok(Ext4FlushProgress {
            complete,
            pending_lba,
            pending_refs,
        })
    }

    pub fn shutdown_clean(&mut self) -> Ext4Result<()> {
        if self.finalized {
            return Ok(());
        }
        self.flush()?;
        unsafe {
            ext4_fs_fini(self.inner.as_mut()).context("ext4_fs_fini")?;
        }
        self.finalized = true;
        self.flush()
    }
}

impl<Hal: SystemHal, Dev: BlockDevice> Drop for Ext4Filesystem<Hal, Dev> {
    fn drop(&mut self) {
        unsafe {
            if !self.finalized && self.owns_superblock_state {
                let r = ext4_fs_fini(self.inner.as_mut());
                if r != 0 {
                    log::error!("ext4_fs_fini failed: {}", Ext4Error::new(r, None));
                }
            }
            let bdev = self.bdev.inner.as_mut();
            ext4_bcache_cleanup(bdev.bc);
            ext4_block_fini(bdev);
            ext4_bcache_fini_dynamic(bdev.bc);
        }
    }
}

pub(crate) struct WritebackGuard {
    bdev: *mut ext4_blockdev,
}
impl WritebackGuard {
    pub fn new(bdev: *mut ext4_blockdev) -> Self {
        unsafe { ext4_block_cache_write_back(bdev, 1) };
        Self { bdev }
    }
}
impl Drop for WritebackGuard {
    fn drop(&mut self) {
        unsafe { ext4_block_cache_write_back(self.bdev, 0) };
    }
}
