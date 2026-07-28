use core::{mem, slice};

use alloc::vec::Vec;

use crate::{Ext4Result, SystemHal, error::Context, ffi::*, util::revision_tuple};

use super::{Ext4MappedReadPlan, Ext4MappedReadRun, InodeRef, InodeType};

#[derive(Debug)]
pub struct Ext4DirectoryReadPlan {
    pub mapped: Ext4MappedReadPlan,
    pub start_offset: u64,
    pub has_file_type: bool,
}

impl<Hal: SystemHal> InodeRef<Hal> {
    pub fn plan_directory_read(
        &mut self,
        offset: u64,
    ) -> Ext4Result<Option<Ext4DirectoryReadPlan>> {
        let file_size = self.size();
        let block_size = crate::util::get_block_size(self.superblock()) as usize;
        if self.inode_type() != InodeType::Directory || block_size == 0 {
            return Ok(None);
        }
        let has_file_type = revision_tuple(self.superblock()) >= (0, 5);
        if offset >= file_size {
            return Ok(Some(Ext4DirectoryReadPlan {
                mapped: Ext4MappedReadPlan {
                    block_size,
                    buffer_len: 0,
                    read_offset: 0,
                    read_len: 0,
                    runs: Vec::new(),
                },
                start_offset: offset,
                has_file_type,
            }));
        }

        let block_size_u64 = block_size as u64;
        let aligned_start = offset / block_size_u64 * block_size_u64;
        let Some(aligned_end) = file_size
            .checked_add(block_size_u64 - 1)
            .map(|value| value / block_size_u64 * block_size_u64)
        else {
            return Ok(None);
        };
        let logical_start_u64 = aligned_start / block_size_u64;
        let logical_blocks_u64 = (aligned_end - aligned_start) / block_size_u64;
        let Ok(logical_start) = u32::try_from(logical_start_u64) else {
            return Ok(None);
        };
        let Ok(logical_blocks) = usize::try_from(logical_blocks_u64) else {
            return Ok(None);
        };
        let Some(buffer_len) = logical_blocks.checked_mul(block_size) else {
            return Ok(None);
        };
        let Ok(read_offset) = usize::try_from(offset - aligned_start) else {
            return Ok(None);
        };
        let Ok(read_len) = usize::try_from(file_size - offset) else {
            return Ok(None);
        };
        let mut runs: Vec<Ext4MappedReadRun> = Vec::new();
        for buffer_block in 0..logical_blocks {
            let Ok(block_delta) = u32::try_from(buffer_block) else {
                return Ok(None);
            };
            let Some(logical_block) = logical_start.checked_add(block_delta) else {
                return Ok(None);
            };
            let fs_block = self.get_inode_fblock(logical_block)?;
            let fs_block = (fs_block != 0).then_some(fs_block);
            let can_merge = runs.last().is_some_and(|last| {
                last.buffer_block + last.block_count == buffer_block
                    && match (last.fs_block, fs_block) {
                        (None, None) => true,
                        (Some(last_block), Some(next_block)) => {
                            last_block + last.block_count as u64 == next_block
                        }
                        _ => false,
                    }
            });
            if can_merge {
                runs.last_mut().unwrap().block_count += 1;
            } else {
                runs.push(Ext4MappedReadRun {
                    buffer_block,
                    block_count: 1,
                    fs_block,
                });
            }
        }
        Ok(Some(Ext4DirectoryReadPlan {
            mapped: Ext4MappedReadPlan {
                block_size,
                buffer_len,
                read_offset,
                read_len,
                runs,
            },
            start_offset: offset,
            has_file_type,
        }))
    }

    pub fn read_dir(mut self, offset: u64) -> Ext4Result<DirReader<Hal>> {
        unsafe {
            let mut iter = mem::zeroed();
            ext4_dir_iterator_init(&mut iter, self.inner.as_mut(), offset)
                .context("ext4_dir_iterator_init")?;

            Ok(DirReader {
                parent: self,
                inner: iter,
            })
        }
    }

    pub fn lookup(mut self, name: &str) -> Ext4Result<DirLookupResult<Hal>> {
        unsafe {
            let mut result = mem::zeroed();
            ext4_dir_find_entry(
                &mut result,
                self.inner.as_mut(),
                name.as_ptr() as *const _,
                name.len() as _,
            )
            .context("ext4_dir_find_entry")?;

            Ok(DirLookupResult {
                parent: self,
                inner: result,
            })
        }
    }

    pub fn has_children(self) -> Ext4Result<bool> {
        if self.inode_type() != InodeType::Directory {
            return Ok(false);
        }
        let mut reader = self.read_dir(0)?;
        while let Some(curr) = reader.current() {
            let name = curr.name();
            if name != b"." && name != b".." {
                return Ok(true);
            }
            reader.step()?;
        }
        Ok(false)
    }

    pub(crate) fn add_entry(&mut self, name: &str, entry: &mut InodeRef<Hal>) -> Ext4Result {
        unsafe {
            ext4_dir_add_entry(
                self.inner.as_mut(),
                name.as_ptr() as *const _,
                name.len() as _,
                entry.inner.as_mut(),
            )
            .context("ext4_dir_add_entry")?;
        }
        entry.inc_nlink();
        Ok(())
    }
    pub(crate) fn remove_entry(&mut self, name: &str, entry: &mut InodeRef<Hal>) -> Ext4Result {
        unsafe {
            ext4_dir_remove_entry(
                self.inner.as_mut(),
                name.as_ptr() as *const _,
                name.len() as _,
            )
            .context("ext4_dir_remove_entry")?;
        }
        entry.dec_nlink();
        Ok(())
    }
}

pub struct DirLookupResult<Hal: SystemHal> {
    parent: InodeRef<Hal>,
    inner: ext4_dir_search_result,
}
impl<Hal: SystemHal> DirLookupResult<Hal> {
    pub fn entry(&mut self) -> DirEntry {
        DirEntry {
            inner: unsafe { &mut *(self.inner.dentry as *mut _) },
            sb: self.parent.superblock(),
        }
    }
}
impl<Hal: SystemHal> Drop for DirLookupResult<Hal> {
    fn drop(&mut self) {
        unsafe {
            ext4_dir_destroy_result(self.parent.inner.as_mut(), &mut self.inner);
        }
    }
}

#[repr(transparent)]
pub struct RawDirEntry {
    inner: ext4_dir_en,
}
impl RawDirEntry {
    pub fn ino(&self) -> u32 {
        u32::from_le(self.inner.inode)
    }
    pub fn set_ino(&mut self, ino: u32) {
        self.inner.inode = u32::to_le(ino);
    }
    pub fn set_inode_type(&mut self, inode_type: InodeType) {
        self.inner.in_.inode_type = match inode_type {
            InodeType::Fifo => EXT4_DE_FIFO,
            InodeType::CharacterDevice => EXT4_DE_CHRDEV,
            InodeType::Directory => EXT4_DE_DIR,
            InodeType::BlockDevice => EXT4_DE_BLKDEV,
            InodeType::RegularFile => EXT4_DE_REG_FILE,
            InodeType::Symlink => EXT4_DE_SYMLINK,
            InodeType::Socket => EXT4_DE_SOCK,
            InodeType::Unknown => EXT4_DE_UNKNOWN,
        } as _;
    }

    pub fn len(&self) -> u16 {
        u16::from_le(self.inner.entry_len)
    }

    pub fn name<'a>(&'a self, sb: &ext4_sblock) -> &'a [u8] {
        let mut name_len = self.inner.name_len as u16;
        if revision_tuple(sb) < (0, 5) {
            let high = unsafe { self.inner.in_.name_length_high };
            name_len |= (high as u16) << 8;
        }
        unsafe { slice::from_raw_parts(self.inner.name.as_ptr(), name_len as usize) }
    }

    pub fn inode_type(&self, sb: &ext4_sblock) -> InodeType {
        if revision_tuple(sb) < (0, 5) {
            InodeType::Unknown
        } else {
            match unsafe { self.inner.in_.inode_type } as u32 {
                EXT4_DE_DIR => InodeType::Directory,
                EXT4_DE_REG_FILE => InodeType::RegularFile,
                EXT4_DE_SYMLINK => InodeType::Symlink,
                EXT4_DE_CHRDEV => InodeType::CharacterDevice,
                EXT4_DE_BLKDEV => InodeType::BlockDevice,
                EXT4_DE_FIFO => InodeType::Fifo,
                EXT4_DE_SOCK => InodeType::Socket,
                _ => InodeType::Unknown,
            }
        }
    }
}

pub struct DirEntry<'a> {
    inner: &'a mut RawDirEntry,
    sb: &'a ext4_sblock,
}
impl DirEntry<'_> {
    pub fn ino(&self) -> u32 {
        self.inner.ino()
    }

    pub fn name(&self) -> &[u8] {
        self.inner.name(self.sb)
    }

    pub fn inode_type(&self) -> InodeType {
        self.inner.inode_type(self.sb)
    }

    pub fn len(&self) -> u16 {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.len() == 0
    }

    pub fn raw_entry(&self) -> &RawDirEntry {
        self.inner
    }
    pub fn raw_entry_mut(&mut self) -> &mut RawDirEntry {
        self.inner
    }
    pub fn set_ino_and_type(&mut self, ino: u32, inode_type: InodeType) {
        self.inner.set_ino(ino);
        if revision_tuple(self.sb) >= (0, 5) {
            self.inner.set_inode_type(inode_type);
        }
    }
}

/// Reader returned by [`InodeRef::read_dir`].
pub struct DirReader<Hal: SystemHal> {
    parent: InodeRef<Hal>,
    inner: ext4_dir_iter,
}
impl<Hal: SystemHal> DirReader<Hal> {
    pub fn current(&self) -> Option<DirEntry> {
        if self.inner.curr.is_null() {
            return None;
        }
        let curr = unsafe { &mut *(self.inner.curr as *mut _) };
        let sb = self.parent.superblock();

        Some(DirEntry { inner: curr, sb })
    }

    pub fn step(&mut self) -> Ext4Result {
        if !self.inner.curr.is_null() {
            unsafe {
                ext4_dir_iterator_next(&mut self.inner).context("ext4_dir_iterator_next")?;
            }
        }
        Ok(())
    }

    pub fn offset(&self) -> u64 {
        self.inner.curr_off
    }
}
impl<Hal: SystemHal> Drop for DirReader<Hal> {
    fn drop(&mut self) {
        unsafe {
            ext4_dir_iterator_fini(&mut self.inner);
        }
    }
}
