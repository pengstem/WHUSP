use super::dirent::{DT_DIR, DT_LNK, DT_REG, RawDirEntry, write_dir_entries};
use super::vfs::{FileSystemBackend, FsError, FsNodeKind, FsResult};
use super::{FileStat, S_IFDIR, S_IFLNK, S_IFREG};
use crate::timer::get_time_us;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

const ROOT_INO: u32 = 2;

struct TmpfsInode {
    kind: FsNodeKind,
    mode: u32,
    nlink: u32,
    data: Vec<u8>,
    children: BTreeMap<String, u32>,
    parent_ino: u32,
    ctime_us: u64,
    mtime_us: u64,
}

pub(super) struct TmpFs {
    inodes: BTreeMap<u32, TmpfsInode>,
    next_ino: u32,
}

impl TmpfsInode {
    fn new(kind: FsNodeKind, mode: u32, parent_ino: u32) -> Self {
        let now = get_time_us() as u64;
        let nlink = if kind == FsNodeKind::Directory { 2 } else { 1 };
        Self {
            kind,
            mode,
            nlink,
            data: Vec::new(),
            children: BTreeMap::new(),
            parent_ino,
            ctime_us: now,
            mtime_us: now,
        }
    }

    fn touch(&mut self) {
        let now = get_time_us() as u64;
        self.ctime_us = now;
        self.mtime_us = now;
    }
}

impl TmpFs {
    pub(super) fn new() -> Self {
        let mut inodes = BTreeMap::new();
        inodes.insert(
            ROOT_INO,
            TmpfsInode::new(FsNodeKind::Directory, S_IFDIR | 0o1777, ROOT_INO),
        );
        Self {
            inodes,
            next_ino: ROOT_INO + 1,
        }
    }

    fn alloc_ino(&mut self) -> u32 {
        let ino = self.next_ino;
        self.next_ino += 1;
        ino
    }

    fn inode(&self, ino: u32) -> FsResult<&TmpfsInode> {
        self.inodes.get(&ino).ok_or(FsError::NotFound)
    }

    fn inode_mut(&mut self, ino: u32) -> FsResult<&mut TmpfsInode> {
        self.inodes.get_mut(&ino).ok_or(FsError::NotFound)
    }

    fn ensure_dir(&self, ino: u32) -> FsResult<&TmpfsInode> {
        let inode = self.inode(ino)?;
        if inode.kind != FsNodeKind::Directory {
            return Err(FsError::NotDir);
        }
        Ok(inode)
    }

    fn create_node(
        &mut self,
        parent_ino: u32,
        name: &str,
        kind: FsNodeKind,
        mode: u32,
    ) -> FsResult<u32> {
        if name.is_empty() || name == "." || name == ".." {
            return Err(FsError::InvalidInput);
        }
        {
            let parent = self.ensure_dir(parent_ino)?;
            if parent.children.contains_key(name) {
                return Err(FsError::AlreadyExists);
            }
        }

        let ino = self.alloc_ino();
        let mut inode = TmpfsInode::new(kind, mode, parent_ino);
        if kind == FsNodeKind::Directory {
            self.inode_mut(parent_ino)?.nlink += 1;
        }
        inode.touch();
        self.inodes.insert(ino, inode);
        let parent = self.inode_mut(parent_ino)?;
        parent.children.insert(name.into(), ino);
        parent.touch();
        Ok(ino)
    }

    fn remove_child(&mut self, parent_ino: u32, name: &str) -> FsResult<u32> {
        let child_ino = {
            let parent = self.ensure_dir(parent_ino)?;
            *parent.children.get(name).ok_or(FsError::NotFound)?
        };
        if child_ino == ROOT_INO {
            return Err(FsError::Busy);
        }
        {
            let child = self.inode(child_ino)?;
            if child.kind == FsNodeKind::Directory && !child.children.is_empty() {
                return Err(FsError::NotEmpty);
            }
        }
        self.inode_mut(parent_ino)?.children.remove(name);
        self.inode_mut(parent_ino)?.touch();
        Ok(child_ino)
    }

    fn drop_inode_link(&mut self, ino: u32) {
        let Some((kind, parent_ino)) = self
            .inodes
            .get(&ino)
            .map(|inode| (inode.kind, inode.parent_ino))
        else {
            return;
        };
        if kind == FsNodeKind::Directory {
            if parent_ino != ino {
                if let Some(parent) = self.inodes.get_mut(&parent_ino) {
                    parent.nlink = parent.nlink.saturating_sub(1);
                    parent.touch();
                }
            }
            self.inodes.remove(&ino);
            return;
        }
        if let Some(inode) = self.inodes.get_mut(&ino) {
            inode.nlink = inode.nlink.saturating_sub(1);
            inode.touch();
            if inode.nlink == 0 {
                self.inodes.remove(&ino);
            }
        }
    }

    fn dir_entries(&self, ino: u32) -> FsResult<Vec<RawDirEntry>> {
        let dir = self.ensure_dir(ino)?;
        let mut entries = Vec::new();
        entries.push(RawDirEntry {
            ino,
            name: ".".into(),
            dtype: DT_DIR,
        });
        entries.push(RawDirEntry {
            ino: dir.parent_ino,
            name: "..".into(),
            dtype: DT_DIR,
        });
        for (name, child_ino) in dir.children.iter() {
            let child = self.inode(*child_ino)?;
            let dtype = match child.kind {
                FsNodeKind::Directory => DT_DIR,
                FsNodeKind::RegularFile => DT_REG,
                FsNodeKind::Symlink => DT_LNK,
                FsNodeKind::Other => 0,
            };
            entries.push(RawDirEntry {
                ino: *child_ino,
                name: name.clone(),
                dtype,
            });
        }
        Ok(entries)
    }
}

impl FileSystemBackend for TmpFs {
    fn root_ino(&self) -> u32 {
        ROOT_INO
    }

    fn statfs(&mut self) -> super::vfs::FileSystemStat {
        super::vfs::FileSystemStat {
            magic: 0x0102_1994,
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
    ) -> FsResult<(u32, FsNodeKind)> {
        let parent = self.ensure_dir(parent_ino)?;
        match component {
            "." => Ok((parent_ino, FsNodeKind::Directory)),
            ".." => Ok((parent.parent_ino, FsNodeKind::Directory)),
            _ => {
                let ino = *parent.children.get(component).ok_or(FsError::NotFound)?;
                Ok((ino, self.inode(ino)?.kind))
            }
        }
    }

    fn create_file(&mut self, parent_ino: u32, leaf_name: &str) -> FsResult<u32> {
        self.create_node(
            parent_ino,
            leaf_name,
            FsNodeKind::RegularFile,
            S_IFREG | 0o666,
        )
    }

    fn create_dir(&mut self, parent_ino: u32, leaf_name: &str, mode: u32) -> FsResult<u32> {
        self.create_node(
            parent_ino,
            leaf_name,
            FsNodeKind::Directory,
            S_IFDIR | (mode & 0o7777),
        )
    }

    fn link(&mut self, parent_ino: u32, leaf_name: &str, child_ino: u32) -> FsResult {
        if leaf_name.is_empty() || leaf_name == "." || leaf_name == ".." {
            return Err(FsError::InvalidInput);
        }
        {
            let parent = self.ensure_dir(parent_ino)?;
            if parent.children.contains_key(leaf_name) {
                return Err(FsError::AlreadyExists);
            }
        }
        if self.inode(child_ino)?.kind == FsNodeKind::Directory {
            return Err(FsError::PermissionDenied);
        }
        self.inode_mut(child_ino)?.nlink += 1;
        self.inode_mut(child_ino)?.touch();
        let parent = self.inode_mut(parent_ino)?;
        parent.children.insert(leaf_name.into(), child_ino);
        parent.touch();
        Ok(())
    }

    fn symlink(&mut self, parent_ino: u32, leaf_name: &str, target: &[u8]) -> FsResult {
        let ino = self.create_node(parent_ino, leaf_name, FsNodeKind::Symlink, S_IFLNK | 0o777)?;
        let inode = self.inode_mut(ino)?;
        inode.data.extend_from_slice(target);
        inode.touch();
        Ok(())
    }

    fn unlink(&mut self, parent_ino: u32, leaf_name: &str) -> FsResult {
        if leaf_name.is_empty() || leaf_name == "." || leaf_name == ".." {
            return Err(FsError::InvalidInput);
        }
        let child_ino = self.remove_child(parent_ino, leaf_name)?;
        self.drop_inode_link(child_ino);
        Ok(())
    }

    fn rename(&mut self, src_dir: u32, src_name: &str, dst_dir: u32, dst_name: &str) -> FsResult {
        if src_name.is_empty() || dst_name.is_empty() || src_name == "." || src_name == ".." {
            return Err(FsError::InvalidInput);
        }
        if dst_name == "." || dst_name == ".." {
            return Err(FsError::InvalidInput);
        }

        let src_ino = {
            let src_parent = self.ensure_dir(src_dir)?;
            *src_parent.children.get(src_name).ok_or(FsError::NotFound)?
        };
        {
            let dst_parent = self.ensure_dir(dst_dir)?;
            if let Some(existing_ino) = dst_parent.children.get(dst_name).copied() {
                if existing_ino != src_ino {
                    let existing = self.inode(existing_ino)?;
                    if existing.kind == FsNodeKind::Directory && !existing.children.is_empty() {
                        return Err(FsError::NotEmpty);
                    }
                }
            }
        }

        let replaced = self.inode_mut(dst_dir)?.children.remove(dst_name);
        if let Some(replaced_ino) = replaced {
            if replaced_ino != src_ino {
                self.drop_inode_link(replaced_ino);
            }
        }
        self.inode_mut(src_dir)?.children.remove(src_name);
        self.inode_mut(dst_dir)?
            .children
            .insert(dst_name.into(), src_ino);
        if self.inode(src_ino)?.kind == FsNodeKind::Directory {
            self.inode_mut(src_ino)?.parent_ino = dst_dir;
        }
        self.inode_mut(src_ino)?.touch();
        self.inode_mut(src_dir)?.touch();
        self.inode_mut(dst_dir)?.touch();
        Ok(())
    }

    fn set_len(&mut self, ino: u32, len: u64) -> FsResult {
        const TMPFS_MAX_FILE_SIZE: u64 = 64 * 1024 * 1024;
        let inode = self.inode_mut(ino)?;
        if inode.kind == FsNodeKind::Directory {
            return Err(FsError::IsDir);
        }
        if len > TMPFS_MAX_FILE_SIZE {
            return Err(FsError::NoSpace);
        }
        inode.data.resize(len as usize, 0);
        inode.touch();
        Ok(())
    }

    fn stat(&mut self, ino: u32) -> FsResult<FileStat> {
        let inode = self.inode(ino)?;
        let size = match inode.kind {
            FsNodeKind::Directory => inode.children.len() as u64,
            _ => inode.data.len() as u64,
        };
        let sec = inode.mtime_us / 1_000_000;
        let nsec = ((inode.mtime_us % 1_000_000) * 1000) as u32;
        Ok(FileStat {
            mode: inode.mode,
            nlink: inode.nlink,
            size,
            blocks: size.div_ceil(512),
            blksize: super::DEFAULT_BLOCK_SIZE,
            atime_sec: sec,
            atime_nsec: nsec,
            mtime_sec: sec,
            mtime_nsec: nsec,
            ctime_sec: inode.ctime_us / 1_000_000,
            ctime_nsec: ((inode.ctime_us % 1_000_000) * 1000) as u32,
            ..FileStat::default()
        })
    }

    fn readlink(&mut self, ino: u32, buf: &mut [u8]) -> FsResult<usize> {
        let inode = self.inode(ino)?;
        if inode.kind != FsNodeKind::Symlink {
            return Err(FsError::InvalidInput);
        }
        let len = buf.len().min(inode.data.len());
        buf[..len].copy_from_slice(&inode.data[..len]);
        Ok(len)
    }

    fn read_at(&mut self, ino: u32, buf: &mut [u8], offset: u64) -> usize {
        let Ok(inode) = self.inode(ino) else {
            return 0;
        };
        if inode.kind == FsNodeKind::Directory {
            return 0;
        }
        let start = (offset as usize).min(inode.data.len());
        let len = buf.len().min(inode.data.len() - start);
        buf[..len].copy_from_slice(&inode.data[start..start + len]);
        len
    }

    fn write_at(&mut self, ino: u32, buf: &[u8], offset: u64) -> usize {
        let Ok(inode) = self.inode_mut(ino) else {
            return 0;
        };
        if inode.kind != FsNodeKind::RegularFile {
            return 0;
        }
        let start = offset as usize;
        let end = start.saturating_add(buf.len());
        if end > inode.data.len() {
            inode.data.resize(end, 0);
        }
        inode.data[start..end].copy_from_slice(buf);
        inode.touch();
        buf.len()
    }

    fn read_dirent64(&mut self, ino: u32, offset: u64, buf: &mut [u8]) -> FsResult<(usize, u64)> {
        write_dir_entries(&self.dir_entries(ino)?, offset, buf)
    }

    fn list_root_names(&mut self) -> Vec<String> {
        self.inodes
            .get(&ROOT_INO)
            .map(|root| root.children.keys().cloned().collect())
            .unwrap_or_default()
    }
}
