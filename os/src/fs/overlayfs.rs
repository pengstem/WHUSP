use super::dirent::{DT_DIR, DT_FIFO, DT_LNK, DT_REG, RawDirEntry, write_dir_entries};
use super::mount::with_mount;
use super::path::WorkingDir;
use super::vfs::{FileSystemBackend, FileSystemStat, FsError, FsNodeKind, FsResult, VfsNodeId};
use super::{FileStat, FileTimestamp, S_IFDIR};
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;

const OVERLAY_ROOT_INO: u32 = 2;
const OVERLAY_SUPER_MAGIC: i64 = 0x794c_7630;

pub(super) struct OverlayFs {
    lower: VfsNodeId,
    upper: VfsNodeId,
    next_ino: u32,
    overlay_to_real: BTreeMap<u32, VfsNodeId>,
    real_to_overlay: BTreeMap<VfsNodeId, u32>,
}

impl OverlayFs {
    pub(super) fn new(lower: WorkingDir, upper: WorkingDir) -> Self {
        Self {
            lower: VfsNodeId::new(lower.mount_id(), lower.ino()),
            upper: VfsNodeId::new(upper.mount_id(), upper.ino()),
            next_ino: OVERLAY_ROOT_INO + 1,
            overlay_to_real: BTreeMap::new(),
            real_to_overlay: BTreeMap::new(),
        }
    }

    fn overlay_ino_for(&mut self, real: VfsNodeId) -> u32 {
        if real == self.upper || real == self.lower {
            return OVERLAY_ROOT_INO;
        }
        if let Some(ino) = self.real_to_overlay.get(&real) {
            return *ino;
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        self.real_to_overlay.insert(real, ino);
        self.overlay_to_real.insert(ino, real);
        ino
    }

    fn lookup_real(parent: VfsNodeId, component: &str) -> FsResult<(VfsNodeId, FsNodeKind)> {
        let (ino, kind) = with_mount(parent.mount_id, |mount| {
            mount.lookup_component_from(parent.ino, component)
        })
        .ok_or(FsError::Io)??;
        Ok((VfsNodeId::new(parent.mount_id, ino), kind))
    }

    fn lookup_child(&mut self, parent: VfsNodeId, component: &str) -> FsResult<(u32, FsNodeKind)> {
        let (real, kind) = Self::lookup_real(parent, component)?;
        Ok((self.overlay_ino_for(real), kind))
    }

    fn real_parent(&mut self, real: VfsNodeId) -> FsResult<u32> {
        let (parent, _) = Self::lookup_real(real, "..")?;
        Ok(self.overlay_ino_for(parent))
    }

    fn real_for_overlay(&self, ino: u32) -> FsResult<VfsNodeId> {
        if ino == OVERLAY_ROOT_INO {
            return Ok(self.upper);
        }
        self.overlay_to_real
            .get(&ino)
            .copied()
            .ok_or(FsError::NotFound)
    }

    fn upper_parent_for_create(&self, parent_ino: u32) -> FsResult<VfsNodeId> {
        if parent_ino == OVERLAY_ROOT_INO {
            return Ok(self.upper);
        }
        let parent = self.real_for_overlay(parent_ino)?;
        if parent.mount_id == self.upper.mount_id {
            Ok(parent)
        } else {
            Err(FsError::Unsupported)
        }
    }

    fn with_real<V>(
        ino: u32,
        real: VfsNodeId,
        f: impl FnOnce(&mut dyn FileSystemBackend, u32) -> V,
    ) -> FsResult<V> {
        with_mount(real.mount_id, |mount| f(mount, real.ino)).ok_or_else(|| {
            if ino == OVERLAY_ROOT_INO {
                FsError::Io
            } else {
                FsError::NotFound
            }
        })
    }
}

impl FileSystemBackend for OverlayFs {
    fn root_ino(&self) -> u32 {
        OVERLAY_ROOT_INO
    }

    fn statfs(&mut self) -> FileSystemStat {
        FileSystemStat {
            magic: OVERLAY_SUPER_MAGIC,
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

    fn overlay_real_node(&mut self, ino: u32) -> Option<VfsNodeId> {
        self.overlay_to_real.get(&ino).copied()
    }

    fn lookup_component_from(
        &mut self,
        parent_ino: u32,
        component: &str,
    ) -> FsResult<(u32, FsNodeKind)> {
        match (parent_ino, component) {
            (OVERLAY_ROOT_INO, "." | "..") => Ok((OVERLAY_ROOT_INO, FsNodeKind::Directory)),
            (_, ".") => {
                let kind = self.stat(parent_ino).map(|stat| {
                    if stat.mode & super::S_IFMT == super::S_IFDIR {
                        FsNodeKind::Directory
                    } else if stat.mode & super::S_IFMT == super::S_IFLNK {
                        FsNodeKind::Symlink
                    } else {
                        FsNodeKind::RegularFile
                    }
                })?;
                Ok((parent_ino, kind))
            }
            (_, "..") if parent_ino != OVERLAY_ROOT_INO => Ok((
                self.real_parent(self.real_for_overlay(parent_ino)?)?,
                FsNodeKind::Directory,
            )),
            (OVERLAY_ROOT_INO, name) => self
                .lookup_child(self.upper, name)
                .or_else(|_| self.lookup_child(self.lower, name)),
            (_, name) => self.lookup_child(self.real_for_overlay(parent_ino)?, name),
        }
    }

    fn create_file(&mut self, parent_ino: u32, leaf_name: &str) -> FsResult<u32> {
        let parent = self.upper_parent_for_create(parent_ino)?;
        let real_ino = with_mount(parent.mount_id, |mount| {
            mount.create_file(parent.ino, leaf_name)
        })
        .ok_or(FsError::Io)??;
        Ok(self.overlay_ino_for(VfsNodeId::new(parent.mount_id, real_ino)))
    }

    fn create_node(
        &mut self,
        parent_ino: u32,
        leaf_name: &str,
        kind: FsNodeKind,
        mode: u32,
        rdev: u64,
    ) -> FsResult<u32> {
        let parent = self.upper_parent_for_create(parent_ino)?;
        let real_ino = with_mount(parent.mount_id, |mount| {
            mount.create_node(parent.ino, leaf_name, kind, mode, rdev)
        })
        .ok_or(FsError::Io)??;
        Ok(self.overlay_ino_for(VfsNodeId::new(parent.mount_id, real_ino)))
    }

    fn create_dir(&mut self, parent_ino: u32, leaf_name: &str, mode: u32) -> FsResult<u32> {
        let parent = self.upper_parent_for_create(parent_ino)?;
        let real_ino = with_mount(parent.mount_id, |mount| {
            mount.create_dir(parent.ino, leaf_name, mode)
        })
        .ok_or(FsError::Io)??;
        Ok(self.overlay_ino_for(VfsNodeId::new(parent.mount_id, real_ino)))
    }

    fn link(&mut self, _parent_ino: u32, _leaf_name: &str, _child_ino: u32) -> FsResult {
        Err(FsError::Unsupported)
    }

    fn symlink(&mut self, parent_ino: u32, leaf_name: &str, target: &[u8]) -> FsResult {
        let parent = self.upper_parent_for_create(parent_ino)?;
        with_mount(parent.mount_id, |mount| {
            mount.symlink(parent.ino, leaf_name, target)
        })
        .ok_or(FsError::Io)?
    }

    fn unlink(&mut self, parent_ino: u32, leaf_name: &str) -> FsResult {
        let parent = self.upper_parent_for_create(parent_ino)?;
        with_mount(parent.mount_id, |mount| mount.unlink(parent.ino, leaf_name))
            .ok_or(FsError::Io)?
    }

    fn rename(
        &mut self,
        _src_dir: u32,
        _src_name: &str,
        _dst_dir: u32,
        _dst_name: &str,
    ) -> FsResult {
        Err(FsError::Unsupported)
    }

    fn set_len(&mut self, ino: u32, len: u64) -> FsResult {
        let real = self.real_for_overlay(ino)?;
        Self::with_real(ino, real, |mount, real_ino| mount.set_len(real_ino, len))?
    }

    fn sync(&mut self, ino: u32, data_only: bool) -> FsResult {
        let real = self.real_for_overlay(ino)?;
        Self::with_real(ino, real, |mount, real_ino| mount.sync(real_ino, data_only))?
    }

    fn set_times(
        &mut self,
        ino: u32,
        atime: Option<FileTimestamp>,
        mtime: Option<FileTimestamp>,
        ctime: FileTimestamp,
    ) -> FsResult {
        let real = self.real_for_overlay(ino)?;
        Self::with_real(ino, real, |mount, real_ino| {
            mount.set_times(real_ino, atime, mtime, ctime)
        })?
    }

    fn set_mode(&mut self, ino: u32, mode: u32) -> FsResult {
        let real = self.real_for_overlay(ino)?;
        Self::with_real(ino, real, |mount, real_ino| mount.set_mode(real_ino, mode))?
    }

    fn set_owner(&mut self, ino: u32, uid: Option<u32>, gid: Option<u32>) -> FsResult {
        let real = self.real_for_overlay(ino)?;
        Self::with_real(ino, real, |mount, real_ino| {
            mount.set_owner(real_ino, uid, gid)
        })?
    }

    fn inode_flags(&mut self, ino: u32) -> FsResult<u32> {
        let real = self.real_for_overlay(ino)?;
        Self::with_real(ino, real, |mount, real_ino| mount.inode_flags(real_ino))?
    }

    fn set_inode_flags(&mut self, ino: u32, flags: u32) -> FsResult {
        let real = self.real_for_overlay(ino)?;
        Self::with_real(ino, real, |mount, real_ino| {
            mount.set_inode_flags(real_ino, flags)
        })?
    }

    fn retain_inode(&mut self, ino: u32) -> FsResult {
        let real = self.real_for_overlay(ino)?;
        Self::with_real(ino, real, |mount, real_ino| mount.retain_inode(real_ino))?
    }

    fn release_inode(&mut self, ino: u32) -> FsResult {
        let real = self.real_for_overlay(ino)?;
        Self::with_real(ino, real, |mount, real_ino| mount.release_inode(real_ino))?
    }

    fn stat(&mut self, ino: u32) -> FsResult<FileStat> {
        if ino == OVERLAY_ROOT_INO {
            let mut stat = FileStat::with_mode(S_IFDIR | 0o755);
            stat.ino = OVERLAY_ROOT_INO as u64;
            return Ok(stat);
        }
        let real = self.real_for_overlay(ino)?;
        let mut stat = Self::with_real(ino, real, |mount, real_ino| mount.stat(real_ino))??;
        stat.ino = ino as u64;
        Ok(stat)
    }

    fn readlink(&mut self, ino: u32, buf: &mut [u8]) -> FsResult<usize> {
        let real = self.real_for_overlay(ino)?;
        Self::with_real(ino, real, |mount, real_ino| mount.readlink(real_ino, buf))?
    }

    fn read_at(&mut self, ino: u32, buf: &mut [u8], offset: u64) -> usize {
        let Ok(real) = self.real_for_overlay(ino) else {
            return 0;
        };
        with_mount(real.mount_id, |mount| mount.read_at(real.ino, buf, offset)).unwrap_or(0)
    }

    fn write_at(&mut self, ino: u32, buf: &[u8], offset: u64) -> usize {
        let Ok(real) = self.real_for_overlay(ino) else {
            return 0;
        };
        with_mount(real.mount_id, |mount| mount.write_at(real.ino, buf, offset)).unwrap_or(0)
    }

    fn read_dirent64(&mut self, ino: u32, offset: u64, buf: &mut [u8]) -> FsResult<(usize, u64)> {
        if ino != OVERLAY_ROOT_INO {
            let real = self.real_for_overlay(ino)?;
            return Self::with_real(ino, real, |mount, real_ino| {
                mount.read_dirent64(real_ino, offset, buf)
            })?;
        }

        let mut names = BTreeSet::new();
        for root in [self.upper, self.lower] {
            if let Some(layer_names) = with_mount(root.mount_id, |mount| mount.list_root_names()) {
                names.extend(layer_names);
            }
        }
        let mut entries = Vec::new();
        entries.push(RawDirEntry {
            ino: OVERLAY_ROOT_INO,
            name: String::from("."),
            dtype: DT_DIR,
        });
        entries.push(RawDirEntry {
            ino: OVERLAY_ROOT_INO,
            name: String::from(".."),
            dtype: DT_DIR,
        });
        for name in names {
            let (ino, kind) = self.lookup_component_from(OVERLAY_ROOT_INO, name.as_str())?;
            let dtype = match kind {
                FsNodeKind::Directory => DT_DIR,
                FsNodeKind::RegularFile => DT_REG,
                FsNodeKind::Symlink => DT_LNK,
                FsNodeKind::Fifo => DT_FIFO,
                _ => 0,
            };
            entries.push(RawDirEntry { ino, name, dtype });
        }
        write_dir_entries(&entries, offset, buf)
    }

    fn list_root_names(&mut self) -> Vec<String> {
        let mut names = BTreeSet::new();
        for root in [self.upper, self.lower] {
            if let Some(layer_names) = with_mount(root.mount_id, |mount| mount.list_root_names()) {
                names.extend(layer_names);
            }
        }
        names.into_iter().collect()
    }
}
