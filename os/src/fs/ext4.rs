use crate::drivers::block::VirtIOBlock;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::str;
use core::time::Duration;
use lwext4_rust::ffi::{EIO, EXT4_ROOT_INO};
use lwext4_rust::{
    BlockDevice as Ext4BlockDevice, EXT4_DEV_BSIZE, Ext4Error, Ext4Filesystem, Ext4Result,
    FsConfig, InodeType, SystemHal,
};

pub(super) struct KernelHal;

impl SystemHal for KernelHal {
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
        let mut block_buf = [0u8; EXT4_DEV_BSIZE];
        for (index, block) in buf.chunks(EXT4_DEV_BSIZE).enumerate() {
            if block.len() != EXT4_DEV_BSIZE {
                return Err(Ext4Error::new(EIO as _, "unaligned block write"));
            }
            block_buf.copy_from_slice(block);
            self.dev.write_block(block_id as usize + index, &block_buf);
        }
        Ok(buf.len())
    }

    fn read_blocks(&mut self, block_id: u64, buf: &mut [u8]) -> Ext4Result<usize> {
        let mut block_buf = [0u8; EXT4_DEV_BSIZE];
        for (index, block) in buf.chunks_mut(EXT4_DEV_BSIZE).enumerate() {
            if block.len() != EXT4_DEV_BSIZE {
                return Err(Ext4Error::new(EIO as _, "unaligned block read"));
            }
            self.dev.read_block(block_id as usize + index, &mut block_buf);
            block.copy_from_slice(&block_buf);
        }
        Ok(buf.len())
    }

    fn num_blocks(&self) -> Ext4Result<u64> {
        Ok(self.dev.num_blocks())
    }
}

type KernelExt4Fs = Ext4Filesystem<KernelHal, KernelDisk>;

const EXT4_CONFIG: FsConfig = FsConfig { bcache_size: 256 };

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FsNodeKind {
    Directory,
    RegularFile,
    Other,
}

fn into_node_kind(kind: InodeType) -> FsNodeKind {
    match kind {
        InodeType::Directory => FsNodeKind::Directory,
        InodeType::RegularFile => FsNodeKind::RegularFile,
        _ => FsNodeKind::Other,
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

    pub(super) fn lookup_path(&mut self, relpath: &str) -> Option<(u32, FsNodeKind)> {
        if relpath.is_empty() {
            return Some((EXT4_ROOT_INO, FsNodeKind::Directory));
        }

        let mut ino = EXT4_ROOT_INO;
        let mut kind = FsNodeKind::Directory;
        for component in relpath
            .split('/')
            .filter(|component| !component.is_empty() && *component != ".")
        {
            if component == ".." {
                return None;
            }
            let mut result = self.fs.lookup(ino, component).ok()?;
            let entry = result.entry();
            ino = entry.ino();
            kind = into_node_kind(entry.inode_type());
        }
        Some((ino, kind))
    }

    pub(super) fn resolve_parent<'a>(&mut self, relpath: &'a str) -> Option<(u32, &'a str)> {
        if relpath.is_empty() {
            return None;
        }
        let (parent_path, leaf_name) = match relpath.rsplit_once('/') {
            Some((parent_path, leaf_name)) => (parent_path, leaf_name),
            None => ("", relpath),
        };
        if leaf_name.is_empty() || leaf_name == "." || leaf_name == ".." {
            return None;
        }
        let (parent_ino, kind) = self.lookup_path(parent_path)?;
        if kind != FsNodeKind::Directory {
            return None;
        }
        Some((parent_ino, leaf_name))
    }

    pub(super) fn create_file(&mut self, parent_ino: u32, leaf_name: &str) -> Option<u32> {
        self.fs
            .create(parent_ino, leaf_name, InodeType::RegularFile, 0o644)
            .ok()
    }

    pub(super) fn set_len(&mut self, ino: u32, len: u64) -> Option<()> {
        self.fs.set_len(ino, len).ok()
    }

    pub(super) fn read_at(&mut self, ino: u32, buf: &mut [u8], offset: u64) -> usize {
        self.fs
            .read_at(ino, buf, offset)
            .expect("ext4 read failed")
    }

    pub(super) fn write_at(&mut self, ino: u32, buf: &[u8], offset: u64) -> usize {
        self.fs
            .write_at(ino, buf, offset)
            .expect("ext4 write failed")
    }

    pub(super) fn list_root_names(&mut self) -> Vec<String> {
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
