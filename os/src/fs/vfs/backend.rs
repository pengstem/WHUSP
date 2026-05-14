use super::super::{FileStat, FileTimestamp};
use super::FsError;
use super::FsResult;
use super::VfsNodeId;
use alloc::string::String;
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FsNodeKind {
    Directory,
    RegularFile,
    Symlink,
    Fifo,
    CharacterDevice,
    Other,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FileSystemStat {
    pub(crate) magic: i64,
    pub(crate) block_size: u64,
    pub(crate) blocks: u64,
    pub(crate) free_blocks: u64,
    pub(crate) available_blocks: u64,
    pub(crate) files: u64,
    pub(crate) free_files: u64,
    pub(crate) max_name_len: u64,
    pub(crate) flags: u64,
}

pub(crate) trait FileSystemBackend: Send {
    fn root_ino(&self) -> u32 {
        2
    }

    fn overlay_real_node(&mut self, _ino: u32) -> Option<VfsNodeId> {
        None
    }

    fn statfs(&mut self) -> FileSystemStat {
        FileSystemStat {
            magic: 0,
            block_size: 4096,
            blocks: 0,
            free_blocks: 0,
            available_blocks: 0,
            files: 1024,
            free_files: 1024,
            max_name_len: 255,
            flags: 0,
        }
    }

    fn lookup_component_from(
        &mut self,
        parent_ino: u32,
        component: &str,
    ) -> FsResult<(u32, FsNodeKind)>;
    fn create_file(&mut self, parent_ino: u32, leaf_name: &str) -> FsResult<u32>;
    fn create_node(
        &mut self,
        parent_ino: u32,
        leaf_name: &str,
        kind: FsNodeKind,
        _mode: u32,
        _rdev: u64,
    ) -> FsResult<u32> {
        match kind {
            FsNodeKind::RegularFile => self.create_file(parent_ino, leaf_name),
            _ => Err(FsError::Unsupported),
        }
    }
    fn create_dir(&mut self, parent_ino: u32, leaf_name: &str, mode: u32) -> FsResult<u32>;
    fn link(&mut self, parent_ino: u32, leaf_name: &str, child_ino: u32) -> FsResult;
    fn symlink(&mut self, parent_ino: u32, leaf_name: &str, target: &[u8]) -> FsResult;
    fn unlink(&mut self, parent_ino: u32, leaf_name: &str) -> FsResult;
    fn rename(&mut self, src_dir: u32, src_name: &str, dst_dir: u32, dst_name: &str) -> FsResult;
    fn set_len(&mut self, ino: u32, len: u64) -> FsResult;
    fn sync(&mut self, _ino: u32, _data_only: bool) -> FsResult {
        Ok(())
    }
    fn shutdown(&mut self) -> FsResult {
        let root_ino = self.root_ino();
        self.sync(root_ino, false)
    }
    fn set_times(
        &mut self,
        _ino: u32,
        _atime: Option<FileTimestamp>,
        _mtime: Option<FileTimestamp>,
        _ctime: FileTimestamp,
    ) -> FsResult {
        Err(FsError::Unsupported)
    }
    fn set_mode(&mut self, _ino: u32, _mode: u32) -> FsResult {
        Err(FsError::Unsupported)
    }
    fn set_owner(&mut self, _ino: u32, _uid: Option<u32>, _gid: Option<u32>) -> FsResult {
        Err(FsError::Unsupported)
    }
    fn inode_flags(&mut self, _ino: u32) -> FsResult<u32> {
        Err(FsError::Unsupported)
    }
    fn set_inode_flags(&mut self, _ino: u32, _flags: u32) -> FsResult {
        Err(FsError::Unsupported)
    }
    fn retain_inode(&mut self, ino: u32) -> FsResult {
        self.stat(ino).map(|_| ())
    }
    fn release_inode(&mut self, _ino: u32) -> FsResult {
        Ok(())
    }
    fn assign_cgroup_pid(&mut self, _dir_ino: u32, _pid: usize) -> FsResult {
        Err(FsError::InvalidInput)
    }
    fn stat(&mut self, ino: u32) -> FsResult<FileStat>;
    fn readlink(&mut self, ino: u32, buf: &mut [u8]) -> FsResult<usize>;
    fn read_at(&mut self, ino: u32, buf: &mut [u8], offset: u64) -> usize;
    fn write_at(&mut self, ino: u32, buf: &[u8], offset: u64) -> usize;
    fn read_dirent64(&mut self, ino: u32, offset: u64, buf: &mut [u8]) -> FsResult<(usize, u64)>;
    fn list_root_names(&mut self) -> Vec<String>;
}
