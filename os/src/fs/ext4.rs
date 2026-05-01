use super::dirent::{
    DT_DIR, DT_LNK, DT_REG, DT_UNKNOWN, LINUX_DIRENT64_ALIGN, LINUX_DIRENT64_HEADER_SIZE,
};
use super::vfs::{FileSystemBackend, FileSystemStat, FsError, FsNodeKind, FsResult};
use super::{FileStat, FileTimestamp};
use crate::drivers::block::VirtIOBlock;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::str;
use core::time::Duration;
use lwext4_rust::ffi::{
    EEXIST, EINVAL, EIO, EISDIR, ENOENT, ENOTDIR, ENOTEMPTY, ENOTSUP, EXT4_ROOT_INO,
};
use lwext4_rust::{
    BlockDevice as Ext4BlockDevice, EXT4_DEV_BSIZE, Ext4Error, Ext4Filesystem, Ext4Result,
    FsConfig, InodeType, SystemHal,
};

pub(super) struct KernelHal;

impl SystemHal for KernelHal {
    // UNFINISHED: Linux stat timestamps should reflect filesystem time updates;
    // this HAL currently exposes no wall-clock source to lwext4.
    fn now() -> Option<Duration> {
        None
    }
}

#[derive(Clone)]
pub(super) struct KernelDisk {
    dev: Arc<VirtIOBlock>,
}

impl Ext4BlockDevice for KernelDisk {
    fn write_blocks(&mut self, block_id: u64, buf: &[u8]) -> Ext4Result<usize> {
        for (index, block) in buf.chunks(EXT4_DEV_BSIZE).enumerate() {
            if block.len() != EXT4_DEV_BSIZE {
                return Err(Ext4Error::new(EIO as _, "unaligned block write"));
            }
            self.dev.write_block(block_id as usize + index, block);
        }
        Ok(buf.len())
    }

    fn read_blocks(&mut self, block_id: u64, buf: &mut [u8]) -> Ext4Result<usize> {
        for (index, block) in buf.chunks_mut(EXT4_DEV_BSIZE).enumerate() {
            if block.len() != EXT4_DEV_BSIZE {
                return Err(Ext4Error::new(EIO as _, "unaligned block read"));
            }
            self.dev.read_block(block_id as usize + index, block);
        }
        Ok(buf.len())
    }

    fn num_blocks(&self) -> Ext4Result<u64> {
        Ok(self.dev.num_blocks())
    }
}

type KernelExt4Fs = Ext4Filesystem<KernelHal, KernelDisk>;

const EXT4_CONFIG: FsConfig = FsConfig { bcache_size: 256 };
// lwext4_rust::ffi does not export ENAMETOOLONG; define it locally.
const ENAMETOOLONG: u32 = 36;

fn into_node_kind(kind: InodeType) -> FsNodeKind {
    match kind {
        InodeType::Directory => FsNodeKind::Directory,
        InodeType::RegularFile => FsNodeKind::RegularFile,
        InodeType::Symlink => FsNodeKind::Symlink,
        // UNFINISHED: Linux distinguishes block devices, character devices,
        // sockets, and FIFOs in stat/getdents; this VFS currently groups the
        // non-directory, non-regular, non-symlink EXT4 types as Other.
        _ => FsNodeKind::Other,
    }
}

fn into_linux_dtype(kind: InodeType) -> u8 {
    match kind {
        InodeType::Directory => DT_DIR,
        InodeType::RegularFile => DT_REG,
        InodeType::Symlink => DT_LNK,
        _ => DT_UNKNOWN,
    }
}

use super::align_up;

fn map_ext4_error(err: Ext4Error) -> FsError {
    // UNFINISHED: lwext4 exposes raw errno values that are not all mapped
    // into this kernel's VFS error model yet; unmapped codes fall back to Io.
    let code = err.code as u32;
    match code {
        ENOENT => FsError::NotFound,
        ENOTDIR => FsError::NotDir,
        EISDIR => FsError::IsDir,
        EEXIST => FsError::AlreadyExists,
        EINVAL => FsError::InvalidInput,
        ENOTEMPTY => FsError::NotEmpty,
        ENAMETOOLONG => FsError::NameTooLong,
        EIO => FsError::Io,
        ENOTSUP => FsError::Unsupported,
        _ => FsError::Io,
    }
}

pub(super) struct Ext4Mount {
    fs: KernelExt4Fs,
}

unsafe impl Send for Ext4Mount {}
unsafe impl Sync for Ext4Mount {}

impl Ext4Mount {
    pub(super) fn open(device: Arc<VirtIOBlock>) -> Result<Self, Ext4Error> {
        Ok(Self {
            fs: KernelExt4Fs::new(KernelDisk { dev: device }, EXT4_CONFIG)?,
        })
    }
}

impl FileSystemBackend for Ext4Mount {
    fn statfs(&mut self) -> FileSystemStat {
        match self.fs.stat() {
            Ok(st) => FileSystemStat {
                magic: 0xEF53,
                block_size: st.block_size as u64,
                blocks: st.blocks_count,
                free_blocks: st.free_blocks_count,
                available_blocks: st.free_blocks_count,
                files: st.inodes_count as u64,
                free_files: st.free_inodes_count as u64,
                max_name_len: 255,
                flags: 0,
            },
            Err(_) => FileSystemStat {
                magic: 0xEF53,
                block_size: 4096,
                blocks: 0,
                free_blocks: 0,
                available_blocks: 0,
                files: 4096,
                free_files: 2048,
                max_name_len: 255,
                flags: 0,
            },
        }
    }

    fn lookup_component_from(
        &mut self,
        parent_ino: u32,
        component: &str,
    ) -> FsResult<(u32, FsNodeKind)> {
        let mut result = self
            .fs
            .lookup(parent_ino, component)
            .map_err(map_ext4_error)?;
        let entry = result.entry();
        Ok((entry.ino(), into_node_kind(entry.inode_type())))
    }

    fn create_file(&mut self, parent_ino: u32, leaf_name: &str) -> FsResult<u32> {
        self.fs
            .create(parent_ino, leaf_name, InodeType::RegularFile, 0o644)
            .map_err(map_ext4_error)
    }

    fn create_dir(&mut self, parent_ino: u32, leaf_name: &str, mode: u32) -> FsResult<u32> {
        self.fs
            .create(parent_ino, leaf_name, InodeType::Directory, mode)
            .map_err(map_ext4_error)
    }

    fn link(&mut self, parent_ino: u32, leaf_name: &str, child_ino: u32) -> FsResult {
        self.fs
            .link(parent_ino, leaf_name, child_ino)
            .map_err(map_ext4_error)
    }

    fn symlink(&mut self, parent_ino: u32, leaf_name: &str, target: &[u8]) -> FsResult {
        let ino = self
            .fs
            .create(parent_ino, leaf_name, InodeType::Symlink, 0o777)
            .map_err(map_ext4_error)?;
        match self.fs.set_symlink(ino, target).map_err(map_ext4_error) {
            Ok(()) => Ok(()),
            Err(err) => {
                let _ = self.fs.unlink(parent_ino, leaf_name);
                Err(err)
            }
        }
    }

    fn unlink(&mut self, parent_ino: u32, leaf_name: &str) -> FsResult {
        self.fs
            .unlink(parent_ino, leaf_name)
            .map_err(map_ext4_error)
    }

    fn rename(&mut self, src_dir: u32, src_name: &str, dst_dir: u32, dst_name: &str) -> FsResult {
        self.fs
            .rename(src_dir, src_name, dst_dir, dst_name)
            .map_err(map_ext4_error)
    }

    fn set_len(&mut self, ino: u32, len: u64) -> FsResult {
        self.fs.set_len(ino, len).map_err(map_ext4_error)
    }

    fn set_times(
        &mut self,
        ino: u32,
        atime: Option<FileTimestamp>,
        mtime: Option<FileTimestamp>,
        ctime: FileTimestamp,
    ) -> FsResult {
        self.fs
            .set_times(
                ino,
                atime.map(FileTimestamp::to_duration),
                mtime.map(FileTimestamp::to_duration),
                Some(ctime.to_duration()),
            )
            .map_err(map_ext4_error)
    }

    fn stat(&mut self, ino: u32) -> FsResult<FileStat> {
        let mut attr = lwext4_rust::FileAttr::default();
        self.fs.get_attr(ino, &mut attr).map_err(map_ext4_error)?;
        Ok(FileStat {
            dev: attr.device,
            ino: attr.ino as u64,
            mode: attr.mode,
            nlink: attr.nlink as u32,
            uid: attr.uid,
            gid: attr.gid,
            rdev: 0,
            size: attr.size,
            blksize: attr.block_size as u32,
            blocks: attr.blocks,
            atime_sec: attr.atime.as_secs(),
            atime_nsec: attr.atime.subsec_nanos(),
            mtime_sec: attr.mtime.as_secs(),
            mtime_nsec: attr.mtime.subsec_nanos(),
            ctime_sec: attr.ctime.as_secs(),
            ctime_nsec: attr.ctime.subsec_nanos(),
        })
    }

    fn readlink(&mut self, ino: u32, buf: &mut [u8]) -> FsResult<usize> {
        self.fs.read_at(ino, buf, 0).map_err(map_ext4_error)
    }

    fn read_at(&mut self, ino: u32, buf: &mut [u8], offset: u64) -> usize {
        self.fs.read_at(ino, buf, offset).expect("ext4 read failed")
    }

    fn write_at(&mut self, ino: u32, buf: &[u8], offset: u64) -> usize {
        self.fs
            .write_at(ino, buf, offset)
            .expect("ext4 write failed")
    }

    fn read_dirent64(&mut self, ino: u32, offset: u64, buf: &mut [u8]) -> FsResult<(usize, u64)> {
        let mut reader = self.fs.read_dir(ino, offset).map_err(map_ext4_error)?;
        let mut written = 0usize;
        let mut next_offset = offset;

        while let Some(entry) = reader.current() {
            let d_ino = entry.ino() as u64;
            let d_type = into_linux_dtype(entry.inode_type());
            let name = entry.name().to_vec();
            let d_reclen = align_up(
                LINUX_DIRENT64_HEADER_SIZE + name.len() + 1,
                LINUX_DIRENT64_ALIGN,
            );

            // TODO:a classic performance loss?
            if d_reclen > buf.len().saturating_sub(written) {
                if written == 0 {
                    return Err(FsError::InvalidInput);
                }
                break;
            }

            reader.step().map_err(map_ext4_error)?;
            next_offset = reader.offset();

            let entry_buf = &mut buf[written..written + d_reclen];
            entry_buf.fill(0);
            entry_buf[0..8].copy_from_slice(&d_ino.to_ne_bytes());
            entry_buf[8..16].copy_from_slice(&(next_offset as i64).to_ne_bytes());
            entry_buf[16..18].copy_from_slice(&(d_reclen as u16).to_ne_bytes());
            entry_buf[18] = d_type;
            entry_buf[LINUX_DIRENT64_HEADER_SIZE..LINUX_DIRENT64_HEADER_SIZE + name.len()]
                .copy_from_slice(&name);

            written += d_reclen;
        }

        Ok((written, next_offset))
    }

    fn list_root_names(&mut self) -> Vec<String> {
        let mut names = Vec::new();
        let mut reader = self
            .fs
            .read_dir(EXT4_ROOT_INO, 0)
            .expect("failed to iterate ext4 root directory");
        while let Some(entry) = reader.current() {
            let name = str::from_utf8(entry.name()).unwrap_or("<invalid>");
            if name != "." && name != ".." {
                names.push(name.to_string());
            }
            reader.step().expect("failed to advance ext4 dir iterator");
        }
        names
    }
}
