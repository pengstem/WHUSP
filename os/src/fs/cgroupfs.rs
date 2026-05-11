use super::dirent::{DT_DIR, DT_REG, RawDirEntry, write_dir_entries};
use super::vfs::{FileSystemBackend, FileSystemStat, FsError, FsNodeKind, FsResult};
use super::{FileStat, FileTimestamp, S_IFDIR, S_IFREG};
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

const ROOT_INO: u32 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CgroupFileKind {
    Controllers,
    SubtreeControl,
    Procs,
}

struct CgroupNode {
    parent_ino: u32,
    kind: CgroupNodeKind,
    mode: u32,
    uid: u32,
    gid: u32,
    nlink: u32,
    atime: FileTimestamp,
    ctime: FileTimestamp,
    mtime: FileTimestamp,
}

enum CgroupNodeKind {
    Directory {
        children: BTreeMap<String, u32>,
        pids: Vec<usize>,
    },
    File {
        kind: CgroupFileKind,
        owner_dir: u32,
    },
}

pub(super) struct CgroupFs {
    inodes: BTreeMap<u32, CgroupNode>,
    next_ino: u32,
}

impl CgroupNode {
    fn new_dir(parent_ino: u32, mode: u32) -> Self {
        let now = FileTimestamp::now();
        Self {
            parent_ino,
            kind: CgroupNodeKind::Directory {
                children: BTreeMap::new(),
                pids: Vec::new(),
            },
            mode: S_IFDIR | (mode & 0o7777),
            uid: 0,
            gid: 0,
            nlink: 2,
            atime: now,
            ctime: now,
            mtime: now,
        }
    }

    fn new_file(parent_ino: u32, kind: CgroupFileKind, mode: u32) -> Self {
        let now = FileTimestamp::now();
        Self {
            parent_ino,
            kind: CgroupNodeKind::File {
                kind,
                owner_dir: parent_ino,
            },
            mode: S_IFREG | (mode & 0o7777),
            uid: 0,
            gid: 0,
            nlink: 1,
            atime: now,
            ctime: now,
            mtime: now,
        }
    }

    fn touch(&mut self) {
        let now = FileTimestamp::now();
        self.ctime = now;
        self.mtime = now;
    }
}

impl CgroupFs {
    pub(super) fn new() -> Self {
        let mut fs = Self {
            inodes: BTreeMap::new(),
            next_ino: ROOT_INO + 1,
        };
        fs.inodes
            .insert(ROOT_INO, CgroupNode::new_dir(ROOT_INO, 0o755));
        fs.add_control_files(ROOT_INO);
        fs
    }

    fn alloc_ino(&mut self) -> u32 {
        let ino = self.next_ino;
        self.next_ino += 1;
        ino
    }

    fn inode(&self, ino: u32) -> FsResult<&CgroupNode> {
        self.inodes.get(&ino).ok_or(FsError::NotFound)
    }

    fn inode_mut(&mut self, ino: u32) -> FsResult<&mut CgroupNode> {
        self.inodes.get_mut(&ino).ok_or(FsError::NotFound)
    }

    fn dir_children(&self, ino: u32) -> FsResult<&BTreeMap<String, u32>> {
        match &self.inode(ino)?.kind {
            CgroupNodeKind::Directory { children, .. } => Ok(children),
            CgroupNodeKind::File { .. } => Err(FsError::NotDir),
        }
    }

    fn dir_children_mut(&mut self, ino: u32) -> FsResult<&mut BTreeMap<String, u32>> {
        match &mut self.inode_mut(ino)?.kind {
            CgroupNodeKind::Directory { children, .. } => Ok(children),
            CgroupNodeKind::File { .. } => Err(FsError::NotDir),
        }
    }

    fn dir_pids_mut(&mut self, ino: u32) -> FsResult<&mut Vec<usize>> {
        match &mut self.inode_mut(ino)?.kind {
            CgroupNodeKind::Directory { pids, .. } => Ok(pids),
            CgroupNodeKind::File { .. } => Err(FsError::NotDir),
        }
    }

    fn add_control_files(&mut self, dir_ino: u32) {
        self.add_control_file(dir_ino, "cgroup.controllers", CgroupFileKind::Controllers);
        self.add_control_file(
            dir_ino,
            "cgroup.subtree_control",
            CgroupFileKind::SubtreeControl,
        );
        self.add_control_file(dir_ino, "cgroup.procs", CgroupFileKind::Procs);
    }

    fn add_control_file(&mut self, dir_ino: u32, name: &str, kind: CgroupFileKind) {
        let ino = self.alloc_ino();
        self.inodes
            .insert(ino, CgroupNode::new_file(dir_ino, kind, 0o644));
        if let Ok(children) = self.dir_children_mut(dir_ino) {
            children.insert(name.into(), ino);
        }
    }

    fn create_dir_node(&mut self, parent_ino: u32, name: &str, mode: u32) -> FsResult<u32> {
        if name.is_empty() || name == "." || name == ".." {
            return Err(FsError::InvalidInput);
        }
        if self.dir_children(parent_ino)?.contains_key(name) {
            return Err(FsError::AlreadyExists);
        }
        let ino = self.alloc_ino();
        self.inodes
            .insert(ino, CgroupNode::new_dir(parent_ino, mode));
        self.add_control_files(ino);
        let parent = self.inode_mut(parent_ino)?;
        if let CgroupNodeKind::Directory { children, .. } = &mut parent.kind {
            children.insert(name.into(), ino);
            parent.nlink += 1;
            parent.touch();
        }
        Ok(ino)
    }

    fn is_standard_file(name: &str) -> bool {
        matches!(
            name,
            "cgroup.controllers" | "cgroup.subtree_control" | "cgroup.procs"
        )
    }

    fn removable_cgroup_dir(&self, ino: u32) -> FsResult<bool> {
        let children = self.dir_children(ino)?;
        Ok(children.keys().all(|name| Self::is_standard_file(name)))
    }

    fn remove_dir_tree(&mut self, ino: u32) {
        let children = self
            .dir_children(ino)
            .map(|children| children.values().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        for child_ino in children {
            self.inodes.remove(&child_ino);
        }
        self.inodes.remove(&ino);
    }

    fn dir_entries(&self, ino: u32) -> FsResult<Vec<RawDirEntry>> {
        let node = self.inode(ino)?;
        let children = match &node.kind {
            CgroupNodeKind::Directory { children, .. } => children,
            CgroupNodeKind::File { .. } => return Err(FsError::NotDir),
        };
        let mut entries = Vec::new();
        entries.push(RawDirEntry {
            ino,
            name: ".".into(),
            dtype: DT_DIR,
        });
        entries.push(RawDirEntry {
            ino: node.parent_ino,
            name: "..".into(),
            dtype: DT_DIR,
        });
        for (name, child_ino) in children {
            let dtype = match self.inode(*child_ino)?.kind {
                CgroupNodeKind::Directory { .. } => DT_DIR,
                CgroupNodeKind::File { .. } => DT_REG,
            };
            entries.push(RawDirEntry {
                ino: *child_ino,
                name: name.clone(),
                dtype,
            });
        }
        Ok(entries)
    }

    fn file_content(&self, kind: CgroupFileKind, owner_dir: u32) -> Vec<u8> {
        match kind {
            CgroupFileKind::Controllers | CgroupFileKind::SubtreeControl => Vec::new(),
            CgroupFileKind::Procs => {
                let mut output = String::new();
                if let Ok(CgroupNode {
                    kind: CgroupNodeKind::Directory { pids, .. },
                    ..
                }) = self.inode(owner_dir)
                {
                    for pid in pids {
                        output.push_str(&format!("{pid}\n"));
                    }
                }
                output.into_bytes()
            }
        }
    }

    fn move_pid_to_dir(&mut self, dir_ino: u32, pid: usize) -> FsResult {
        self.dir_children(dir_ino)?;
        for node in self.inodes.values_mut() {
            if let CgroupNodeKind::Directory { pids, .. } = &mut node.kind {
                pids.retain(|existing| *existing != pid);
            }
        }
        let pids = self.dir_pids_mut(dir_ino)?;
        if !pids.contains(&pid) {
            pids.push(pid);
        }
        Ok(())
    }

    fn write_procs(&mut self, owner_dir: u32, buf: &[u8]) -> usize {
        let Ok(text) = core::str::from_utf8(buf) else {
            return 0;
        };
        for token in text.split_ascii_whitespace() {
            let Ok(pid) = token.parse::<usize>() else {
                return 0;
            };
            if self.move_pid_to_dir(owner_dir, pid).is_err() {
                return 0;
            }
        }
        buf.len()
    }
}

impl FileSystemBackend for CgroupFs {
    fn root_ino(&self) -> u32 {
        ROOT_INO
    }

    fn statfs(&mut self) -> FileSystemStat {
        FileSystemStat {
            magic: 0x6367_7270,
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
        let parent = self.inode(parent_ino)?;
        match component {
            "." => Ok((parent_ino, FsNodeKind::Directory)),
            ".." => Ok((parent.parent_ino, FsNodeKind::Directory)),
            _ => {
                let ino = *self
                    .dir_children(parent_ino)?
                    .get(component)
                    .ok_or(FsError::NotFound)?;
                let kind = match self.inode(ino)?.kind {
                    CgroupNodeKind::Directory { .. } => FsNodeKind::Directory,
                    CgroupNodeKind::File { .. } => FsNodeKind::RegularFile,
                };
                Ok((ino, kind))
            }
        }
    }

    fn create_file(&mut self, _parent_ino: u32, _leaf_name: &str) -> FsResult<u32> {
        Err(FsError::PermissionDenied)
    }

    fn create_dir(&mut self, parent_ino: u32, leaf_name: &str, mode: u32) -> FsResult<u32> {
        self.create_dir_node(parent_ino, leaf_name, mode)
    }

    fn link(&mut self, _parent_ino: u32, _leaf_name: &str, _child_ino: u32) -> FsResult {
        Err(FsError::PermissionDenied)
    }

    fn symlink(&mut self, _parent_ino: u32, _leaf_name: &str, _target: &[u8]) -> FsResult {
        Err(FsError::PermissionDenied)
    }

    fn unlink(&mut self, parent_ino: u32, leaf_name: &str) -> FsResult {
        if leaf_name.is_empty() || leaf_name == "." || leaf_name == ".." {
            return Err(FsError::InvalidInput);
        }
        let child_ino = *self
            .dir_children(parent_ino)?
            .get(leaf_name)
            .ok_or(FsError::NotFound)?;
        if child_ino == ROOT_INO {
            return Err(FsError::Busy);
        }
        match self.inode(child_ino)?.kind {
            CgroupNodeKind::File { .. } => return Err(FsError::PermissionDenied),
            CgroupNodeKind::Directory { .. } => {
                if !self.removable_cgroup_dir(child_ino)? {
                    return Err(FsError::NotEmpty);
                }
            }
        }
        self.dir_children_mut(parent_ino)?.remove(leaf_name);
        self.remove_dir_tree(child_ino);
        Ok(())
    }

    fn rename(
        &mut self,
        _src_dir: u32,
        _src_name: &str,
        _dst_dir: u32,
        _dst_name: &str,
    ) -> FsResult {
        Err(FsError::PermissionDenied)
    }

    fn set_len(&mut self, ino: u32, _len: u64) -> FsResult {
        match self.inode(ino)?.kind {
            CgroupNodeKind::File { .. } => Ok(()),
            CgroupNodeKind::Directory { .. } => Err(FsError::IsDir),
        }
    }

    fn set_mode(&mut self, ino: u32, mode: u32) -> FsResult {
        let inode = self.inode_mut(ino)?;
        inode.mode = (inode.mode & !0o7777) | (mode & 0o7777);
        inode.ctime = FileTimestamp::now();
        Ok(())
    }

    fn set_owner(&mut self, ino: u32, uid: Option<u32>, gid: Option<u32>) -> FsResult {
        let inode = self.inode_mut(ino)?;
        if let Some(uid) = uid {
            inode.uid = uid;
        }
        if let Some(gid) = gid {
            inode.gid = gid;
        }
        inode.ctime = FileTimestamp::now();
        Ok(())
    }

    fn stat(&mut self, ino: u32) -> FsResult<FileStat> {
        let inode = self.inode(ino)?;
        let size = match inode.kind {
            CgroupNodeKind::Directory { ref children, .. } => children.len() as u64,
            CgroupNodeKind::File { kind, owner_dir } => {
                self.file_content(kind, owner_dir).len() as u64
            }
        };
        Ok(FileStat {
            ino: ino as u64,
            mode: inode.mode,
            nlink: inode.nlink,
            uid: inode.uid,
            gid: inode.gid,
            size,
            blocks: size.div_ceil(512),
            blksize: super::DEFAULT_BLOCK_SIZE,
            atime_sec: inode.atime.sec,
            atime_nsec: inode.atime.nsec,
            mtime_sec: inode.mtime.sec,
            mtime_nsec: inode.mtime.nsec,
            ctime_sec: inode.ctime.sec,
            ctime_nsec: inode.ctime.nsec,
            ..FileStat::default()
        })
    }

    fn readlink(&mut self, _ino: u32, _buf: &mut [u8]) -> FsResult<usize> {
        Err(FsError::InvalidInput)
    }

    fn read_at(&mut self, ino: u32, buf: &mut [u8], offset: u64) -> usize {
        let Ok(inode) = self.inode(ino) else {
            return 0;
        };
        let CgroupNodeKind::File { kind, owner_dir } = inode.kind else {
            return 0;
        };
        let content = self.file_content(kind, owner_dir);
        let Ok(offset) = usize::try_from(offset) else {
            return 0;
        };
        if offset >= content.len() {
            return 0;
        }
        let len = buf.len().min(content.len() - offset);
        buf[..len].copy_from_slice(&content[offset..offset + len]);
        len
    }

    fn write_at(&mut self, ino: u32, buf: &[u8], _offset: u64) -> usize {
        let Ok(inode) = self.inode(ino) else {
            return 0;
        };
        let CgroupNodeKind::File { kind, owner_dir } = inode.kind else {
            return 0;
        };
        match kind {
            CgroupFileKind::Controllers => 0,
            CgroupFileKind::SubtreeControl => buf.len(),
            CgroupFileKind::Procs => self.write_procs(owner_dir, buf),
        }
    }

    fn read_dirent64(&mut self, ino: u32, offset: u64, buf: &mut [u8]) -> FsResult<(usize, u64)> {
        write_dir_entries(&self.dir_entries(ino)?, offset, buf)
    }

    fn list_root_names(&mut self) -> Vec<String> {
        self.dir_children(ROOT_INO)
            .map(|children| children.keys().cloned().collect())
            .unwrap_or_default()
    }

    fn assign_cgroup_pid(&mut self, dir_ino: u32, pid: usize) -> FsResult {
        self.move_pid_to_dir(dir_ino, pid)
    }
}
