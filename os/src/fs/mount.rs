use super::ext4::Ext4Mount;
use super::fat::FatMount;
use super::path::WorkingDir;
use super::procfs::ProcFs;
use super::tmpfs::TmpFs;
use super::vfs::{FileSystemBackend, FileSystemStat, FsError, FsNodeKind, VfsNodeId};
use crate::drivers::block::BLOCK_DEVICES;
use crate::sync::{SleepMutex, UPIntrFreeCell};
use crate::task::any_process_references_mount;
use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use alloc::{format, string::String};
use core::sync::atomic::{AtomicUsize, Ordering};
use lazy_static::*;
use log::{info, warn};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MountId(pub(crate) usize);

#[derive(Clone, Debug, Eq, PartialEq)]
struct DynamicMount {
    target: VfsNodeId,
    covered_parent: VfsNodeId,
    source_mount_id: MountId,
    target_path: String,
}

struct MountedFs {
    source: String,
    fs_type: &'static str,
    options: &'static str,
    backend: SleepMutex<Box<dyn FileSystemBackend>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BackendKind {
    Ext4,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MountError {
    SourceMissing,
    InvalidFilesystem,
    InvalidTarget,
    TargetBusy,
    TargetNotMounted,
    StaticRoot,
}

#[derive(Clone, Debug)]
pub(crate) struct MountInfo {
    pub(crate) source: String,
    pub(crate) target: String,
    pub(crate) fs_type: &'static str,
    pub(crate) options: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BlockPartition {
    pub(crate) start_block: u64,
    pub(crate) block_count: u64,
}

lazy_static! {
    static ref MOUNTS: SleepMutex<Vec<Option<Arc<MountedFs>>>> = SleepMutex::new(Vec::new());
    static ref MOUNTS_INITIALIZED: UPIntrFreeCell<bool> = unsafe { UPIntrFreeCell::new(false) };
    static ref DYNAMIC_MOUNTS: UPIntrFreeCell<Vec<DynamicMount>> =
        unsafe { UPIntrFreeCell::new(Vec::new()) };
    static ref PENDING_INODE_RELEASES: UPIntrFreeCell<Vec<(MountId, u32)>> =
        unsafe { UPIntrFreeCell::new(Vec::new()) };
}

static NEXT_MOUNT_ID: AtomicUsize = AtomicUsize::new(0);

pub fn init_mounts() {
    let already_initialized = MOUNTS_INITIALIZED.exclusive_session(|initialized| {
        if *initialized {
            true
        } else {
            *initialized = true;
            false
        }
    });
    if already_initialized {
        return;
    }

    let primary_device = BLOCK_DEVICES
        .first()
        .expect("DTB is missing a block device")
        .clone();
    let primary_mount = open_backend(BackendKind::Ext4, primary_device, 0)
        .expect("failed to mount primary ext4 filesystem");
    let block_mount_count = BLOCK_DEVICES.len();
    {
        let mut mounts = MOUNTS.lock();
        mounts.resize_with(block_mount_count, || None);
        mounts[0] = Some(primary_mount);
    }
    NEXT_MOUNT_ID.store(block_mount_count, Ordering::SeqCst);

    mount_extra_block_devices();
    mount_kernel_pseudo_filesystems();
}

impl MountedFs {
    fn new(
        backend: Box<dyn FileSystemBackend>,
        source: String,
        fs_type: &'static str,
        options: &'static str,
    ) -> Arc<Self> {
        Arc::new(Self {
            source,
            fs_type,
            options,
            backend: SleepMutex::new(backend),
        })
    }
}

fn block_source_name(device_index: usize) -> String {
    if device_index < 26 {
        format!("/dev/vd{}", (b'a' + device_index as u8) as char)
    } else {
        format!("/dev/vd{device_index}")
    }
}

fn block_partition_source_name(device_index: usize, partition_index: usize) -> String {
    format!("{}{}", block_source_name(device_index), partition_index)
}

fn read_le_u32(bytes: &[u8]) -> u32 {
    let mut value = [0u8; 4];
    value.copy_from_slice(bytes);
    u32::from_le_bytes(value)
}

fn open_backend(
    kind: BackendKind,
    device: Arc<crate::drivers::block::VirtIOBlock>,
    device_index: usize,
) -> Result<Arc<MountedFs>, MountError> {
    match kind {
        BackendKind::Ext4 => Ext4Mount::open(device)
            .map(|mount| {
                MountedFs::new(
                    Box::new(mount),
                    block_source_name(device_index),
                    "ext4",
                    "rw",
                )
            })
            .map_err(|err| {
                warn!("ext4 open failed: {:?}", err);
                MountError::InvalidFilesystem
            }),
    }
}

fn register_mount(mounted: Arc<MountedFs>) -> MountId {
    let mount_id = MountId(NEXT_MOUNT_ID.fetch_add(1, Ordering::SeqCst));
    let mut mounts = MOUNTS.lock();
    if mount_id.0 >= mounts.len() {
        mounts.resize_with(mount_id.0 + 1, || None);
    }
    mounts[mount_id.0] = Some(mounted);
    mount_id
}

pub(super) fn with_mount<V>(
    mount_id: MountId,
    f: impl FnOnce(&mut dyn FileSystemBackend) -> V,
) -> Option<V> {
    let mounted = {
        let mounts = MOUNTS.lock();
        mounts
            .get(mount_id.0)
            .and_then(|mount| mount.as_ref().cloned())
    }?;
    let mut backend = mounted.backend.lock();
    drain_pending_inode_releases(mount_id, &mut **backend);
    Some(f(&mut **backend))
}

fn try_with_mount<V>(
    mount_id: MountId,
    f: impl FnOnce(&mut dyn FileSystemBackend) -> V,
) -> Option<V> {
    let mounted = {
        let mounts = MOUNTS.try_lock()?;
        mounts
            .get(mount_id.0)
            .and_then(|mount| mount.as_ref().cloned())
    }?;
    let mut backend = mounted.backend.try_lock()?;
    drain_pending_inode_releases(mount_id, &mut **backend);
    Some(f(&mut **backend))
}

fn drain_pending_inode_releases(mount_id: MountId, backend: &mut dyn FileSystemBackend) {
    let pending = {
        let mut pending = PENDING_INODE_RELEASES.exclusive_access();
        if pending.is_empty() {
            return;
        }
        core::mem::take(&mut *pending)
    };

    let mut deferred = Vec::new();
    for (pending_mount_id, ino) in pending {
        if pending_mount_id == mount_id {
            let _ = backend.release_inode(ino);
        } else {
            deferred.push((pending_mount_id, ino));
        }
    }
    if !deferred.is_empty() {
        PENDING_INODE_RELEASES.exclusive_access().extend(deferred);
    }
}

pub(super) fn release_inode_from_drop(mount_id: MountId, ino: u32) {
    if try_with_mount(mount_id, |mount| mount.release_inode(ino)).is_none() {
        PENDING_INODE_RELEASES
            .exclusive_access()
            .push((mount_id, ino));
    }
}

pub(super) fn mount_exists(mount_id: MountId) -> bool {
    let mounts = MOUNTS.lock();
    mounts.get(mount_id.0).is_some_and(Option::is_some)
}

fn ensure_mount_open(mount_id: MountId) -> Result<(), MountError> {
    let device = BLOCK_DEVICES
        .get(mount_id.0)
        .ok_or(MountError::SourceMissing)?
        .clone();

    {
        let mounts = MOUNTS.lock();
        let Some(mount) = mounts.get(mount_id.0) else {
            return Err(MountError::SourceMissing);
        };
        if mount.is_some() {
            return Ok(());
        }
    }

    let mount = open_backend(BackendKind::Ext4, device, mount_id.0)?;
    let mut mounts = MOUNTS.lock();
    let Some(slot) = mounts.get_mut(mount_id.0) else {
        return Err(MountError::SourceMissing);
    };
    if slot.is_none() {
        *slot = Some(mount);
    }
    Ok(())
}

pub(super) fn root_ino_for(mount_id: MountId) -> Option<u32> {
    with_mount(mount_id, |mount| mount.root_ino())
}

fn primary_root_ino() -> u32 {
    root_ino_for(primary_mount_id()).unwrap_or(2)
}

pub(super) fn mounted_root_for(target: VfsNodeId) -> Option<MountId> {
    DYNAMIC_MOUNTS.exclusive_session(|mounts| {
        mounts
            .iter()
            .rev()
            .find(|mount| mount.target == target)
            .map(|mount| mount.source_mount_id)
    })
}

pub(super) fn mounted_root_parent(source_mount_id: MountId) -> Option<VfsNodeId> {
    DYNAMIC_MOUNTS.exclusive_session(|mounts| {
        // UNFINISHED: MountId currently names one opened block-device
        // filesystem, not a distinct mount instance. If the same source is
        // mounted at multiple targets, `..` from that source root follows the
        // newest dynamic mount instead of a per-mount parent reference.
        mounts
            .iter()
            .rev()
            .find(|mount| mount.source_mount_id == source_mount_id)
            .map(|mount| mount.covered_parent)
    })
}

pub(super) fn primary_mount_id() -> MountId {
    MountId(0)
}

fn lookup_covered_parent(target: VfsNodeId) -> Result<VfsNodeId, MountError> {
    let (parent_ino, kind) = with_mount(target.mount_id, |mount| {
        mount.lookup_component_from(target.ino, "..")
    })
    .ok_or(MountError::InvalidTarget)?
    .map_err(|_| MountError::InvalidTarget)?;
    if kind != FsNodeKind::Directory {
        return Err(MountError::InvalidTarget);
    }
    Ok(VfsNodeId::new(target.mount_id, parent_ino))
}

pub(crate) fn mount_block_device_at(
    target: WorkingDir,
    device_index: usize,
) -> Result<(), MountError> {
    let source_mount_id = MountId(device_index);
    let target = VfsNodeId::new(target.mount_id(), target.ino());
    if root_ino_for(target.mount_id).is_some_and(|root_ino| target.ino == root_ino) {
        return Err(MountError::StaticRoot);
    }

    let target_is_busy = DYNAMIC_MOUNTS
        .exclusive_session(|mounts| mounts.iter().any(|mount| mount.target == target));
    if target_is_busy {
        return Err(MountError::TargetBusy);
    }

    let covered_parent = lookup_covered_parent(target)?;
    let target_path = mount_point_for_target(target);
    ensure_mount_open(source_mount_id)?;

    DYNAMIC_MOUNTS.exclusive_session(|mounts| {
        if mounts.iter().any(|mount| mount.target == target) {
            return Err(MountError::TargetBusy);
        }
        mounts.push(DynamicMount {
            target,
            covered_parent,
            source_mount_id,
            target_path,
        });
        Ok(())
    })
}

fn read_mbr_partition(
    device: &crate::drivers::block::VirtIOBlock,
    partition_index: usize,
) -> Result<BlockPartition, MountError> {
    if !(1..=4).contains(&partition_index) {
        return Err(MountError::SourceMissing);
    }
    let mut mbr = [0u8; 512];
    device.read_block(0, &mut mbr);
    if mbr[510] != 0x55 || mbr[511] != 0xaa {
        return Err(MountError::SourceMissing);
    }
    let entry_offset = 446 + (partition_index - 1) * 16;
    let entry = &mbr[entry_offset..entry_offset + 16];
    let partition_type = entry[4];
    let start_block = read_le_u32(&entry[8..12]) as u64;
    let block_count = read_le_u32(&entry[12..16]) as u64;
    if partition_type == 0 || start_block == 0 || block_count == 0 {
        return Err(MountError::SourceMissing);
    }
    let end_block = start_block
        .checked_add(block_count)
        .ok_or(MountError::SourceMissing)?;
    if end_block > device.num_blocks() {
        return Err(MountError::SourceMissing);
    }
    Ok(BlockPartition {
        start_block,
        block_count,
    })
}

pub(crate) fn mount_fat_device_at(
    target: WorkingDir,
    device_index: usize,
    partition_index: Option<usize>,
) -> Result<MountId, MountError> {
    let device = BLOCK_DEVICES
        .get(device_index)
        .ok_or(MountError::SourceMissing)?
        .clone();
    let (source, partition) = if let Some(partition_index) = partition_index {
        (
            block_partition_source_name(device_index, partition_index),
            read_mbr_partition(&device, partition_index)?,
        )
    } else {
        (
            block_source_name(device_index),
            BlockPartition {
                start_block: 0,
                block_count: device.num_blocks(),
            },
        )
    };
    let fat_mount = FatMount::open(device, partition).map_err(|err| {
        warn!("fat open failed: {:?}", err);
        MountError::InvalidFilesystem
    })?;
    mount_new_fs_at(
        target,
        MountedFs::new(Box::new(fat_mount), source, "vfat", "rw"),
    )
}

fn mount_new_fs_at(target: WorkingDir, mounted: Arc<MountedFs>) -> Result<MountId, MountError> {
    let target = VfsNodeId::new(target.mount_id(), target.ino());
    if root_ino_for(target.mount_id).is_some_and(|root_ino| target.ino == root_ino) {
        return Err(MountError::StaticRoot);
    }

    let target_is_busy = DYNAMIC_MOUNTS
        .exclusive_session(|mounts| mounts.iter().any(|mount| mount.target == target));
    if target_is_busy {
        return Err(MountError::TargetBusy);
    }

    let covered_parent = lookup_covered_parent(target)?;
    let target_path = mount_point_for_target(target);
    let source_mount_id = register_mount(mounted);
    DYNAMIC_MOUNTS.exclusive_session(|mounts| {
        if mounts.iter().any(|mount| mount.target == target) {
            return Err(MountError::TargetBusy);
        }
        mounts.push(DynamicMount {
            target,
            covered_parent,
            source_mount_id,
            target_path,
        });
        Ok(source_mount_id)
    })
}

pub(crate) fn register_pseudo_mount(
    backend: Box<dyn FileSystemBackend>,
    fs_type: &'static str,
) -> MountId {
    register_mount(MountedFs::new(backend, fs_type.into(), fs_type, "rw"))
}

pub(crate) fn mount_pseudo_fs_at(
    target: WorkingDir,
    backend: Box<dyn FileSystemBackend>,
    fs_type: &'static str,
) -> Result<MountId, MountError> {
    let target = VfsNodeId::new(target.mount_id(), target.ino());
    if root_ino_for(target.mount_id).is_some_and(|root_ino| target.ino == root_ino) {
        return Err(MountError::StaticRoot);
    }

    let target_is_busy = DYNAMIC_MOUNTS
        .exclusive_session(|mounts| mounts.iter().any(|mount| mount.target == target));
    if target_is_busy {
        return Err(MountError::TargetBusy);
    }

    let covered_parent = lookup_covered_parent(target)?;
    let target_path = mount_point_for_target(target);
    let source_mount_id = register_pseudo_mount(backend, fs_type);
    DYNAMIC_MOUNTS.exclusive_session(|mounts| {
        if mounts.iter().any(|mount| mount.target == target) {
            return Err(MountError::TargetBusy);
        }
        mounts.push(DynamicMount {
            target,
            covered_parent,
            source_mount_id,
            target_path,
        });
        Ok(source_mount_id)
    })
}

pub(crate) fn mount_tmpfs_at(target: WorkingDir) -> Result<MountId, MountError> {
    mount_pseudo_fs_at(target, Box::new(TmpFs::new()), "tmpfs")
}

pub(crate) fn unmount_at(target: WorkingDir) -> Result<(), MountError> {
    let target = VfsNodeId::new(target.mount_id(), target.ino());
    let target_is_root =
        root_ino_for(target.mount_id).is_some_and(|root_ino| target.ino == root_ino);
    if target_is_root && target.mount_id == primary_mount_id() {
        return Err(MountError::StaticRoot);
    }
    DYNAMIC_MOUNTS.exclusive_session(|mounts| {
        let index = if target_is_root {
            mounts
                .iter()
                .rposition(|mount| mount.source_mount_id == target.mount_id)
        } else {
            mounts.iter().rposition(|mount| mount.target == target)
        };
        let index = index.ok_or(MountError::TargetNotMounted)?;
        let source_mount_id = mounts[index].source_mount_id;
        if any_process_references_mount(source_mount_id) {
            return Err(MountError::TargetBusy);
        }
        mounts.remove(index);
        Ok(())
    })
}

fn ensure_extra_mount_target(index: usize) -> Option<WorkingDir> {
    ensure_primary_dir(&format!("x{index}"), 0o755)
}

fn source_has_dynamic_mount(source_mount_id: MountId) -> bool {
    DYNAMIC_MOUNTS.exclusive_session(|mounts| {
        mounts
            .iter()
            .any(|mount| mount.source_mount_id == source_mount_id)
    })
}

fn mount_extra_block_devices() {
    for index in 1..BLOCK_DEVICES.len() {
        let Some(target) = ensure_extra_mount_target(index) else {
            continue;
        };
        match mount_block_device_at(target, index) {
            Ok(()) => info!("auto-mounted BLOCK_DEVICES[{index}] at /x{index}"),
            Err(MountError::InvalidFilesystem) => {
                warn!("BLOCK_DEVICES[{index}] is not an ext4 filesystem; leaving /x{index} empty")
            }
            Err(err) => warn!("failed to auto-mount BLOCK_DEVICES[{index}] at /x{index}: {err:?}"),
        }
    }
}

fn ensure_primary_dir(name: &str, mode: u32) -> Option<WorkingDir> {
    let root_ino = primary_root_ino();
    ensure_primary_child_dir(
        WorkingDir::new(primary_mount_id(), root_ino),
        name,
        mode,
        &format!("/{name}"),
    )
}

fn ensure_primary_child_dir(
    parent: WorkingDir,
    name: &str,
    mode: u32,
    display_path: &str,
) -> Option<WorkingDir> {
    with_mount(parent.mount_id(), |mount| {
        match mount.lookup_component_from(parent.ino(), name) {
            Ok((ino, kind)) => {
                if kind == FsNodeKind::Directory {
                    return Some(WorkingDir::new(parent.mount_id(), ino));
                }
                warn!(
                    "cannot mount pseudo filesystem at {display_path}: target is not a directory"
                );
                return None;
            }
            Err(FsError::NotFound) => {}
            Err(err) => {
                warn!("cannot lookup {display_path} for pseudo filesystem mount: {err:?}");
                return None;
            }
        }

        mount
            .create_dir(parent.ino(), name, mode)
            .map(|ino| WorkingDir::new(parent.mount_id(), ino))
            .ok()
            .or_else(|| {
                warn!("cannot create {display_path} for pseudo filesystem mount");
                None
            })
    })
    .flatten()
}

fn mount_kernel_pseudo_filesystems() {
    if let Some(target) = ensure_primary_dir("proc", 0o555) {
        match mount_pseudo_fs_at(target, Box::new(ProcFs::new()), "proc") {
            Ok(_) => info!("filesystem mounted from proc at /proc"),
            Err(err) => warn!("failed to mount proc at /proc: {err:?}"),
        }
    }
    if let Some(target) = ensure_primary_dir("tmp", 0o1777) {
        match mount_pseudo_fs_at(target, Box::new(TmpFs::new()), "tmpfs") {
            Ok(_) => info!("filesystem mounted from tmpfs at /tmp"),
            Err(err) => warn!("failed to mount tmpfs at /tmp: {err:?}"),
        }
    }
    if let Some(dev) = ensure_primary_dir("dev", 0o755) {
        if let Some(target) = ensure_primary_child_dir(dev, "shm", 0o1777, "/dev/shm") {
            match mount_pseudo_fs_at(target, Box::new(TmpFs::new()), "tmpfs") {
                Ok(_) => info!("filesystem mounted from tmpfs at /dev/shm"),
                Err(err) => warn!("failed to mount tmpfs at /dev/shm: {err:?}"),
            }
        }
    }
}

pub fn mount_status_log() {
    info!("filesystem mounted from BLOCK_DEVICES[0] at /");
    let mounts_len = MOUNTS.lock().len();
    for index in 1..mounts_len {
        if source_has_dynamic_mount(MountId(index)) {
            info!("filesystem mounted from BLOCK_DEVICES[{index}] at /x{index}");
        } else if mount_exists(MountId(index)) {
            info!("filesystem on BLOCK_DEVICES[{index}] is open but not mounted");
        } else {
            info!("filesystem on BLOCK_DEVICES[{index}] is not mounted");
        }
    }
}

pub fn list_root_apps() -> Vec<String> {
    with_mount(primary_mount_id(), |mount| mount.list_root_names()).unwrap_or_default()
}

fn mount_metadata(mount_id: MountId) -> Option<(String, &'static str, &'static str)> {
    let mounted = {
        let mounts = MOUNTS.lock();
        mounts
            .get(mount_id.0)
            .and_then(|mount| mount.as_ref().cloned())
    }?;
    Some((mounted.source.clone(), mounted.fs_type, mounted.options))
}

fn mount_point_for_target(target: VfsNodeId) -> String {
    if target.mount_id == primary_mount_id() && target.ino == primary_root_ino() {
        return "/".into();
    }
    if target.mount_id == primary_mount_id() {
        let root_ino = primary_root_ino();
        if let Some(path) = with_mount(primary_mount_id(), |mount| {
            for name in mount.list_root_names() {
                if let Ok((ino, kind)) = mount.lookup_component_from(root_ino, &name) {
                    if ino == target.ino && kind == FsNodeKind::Directory {
                        return Some(format!("/{name}"));
                    }
                }
            }
            None
        })
        .flatten()
        {
            return path;
        }
    }
    format!("<mount:{}:{}>", target.mount_id.0, target.ino)
}

pub(crate) fn list_mounts() -> Vec<MountInfo> {
    let mut infos = Vec::new();
    if let Some((source, fs_type, options)) = mount_metadata(primary_mount_id()) {
        infos.push(MountInfo {
            source,
            target: "/".into(),
            fs_type,
            options,
        });
    }

    let dynamic_mounts = DYNAMIC_MOUNTS.exclusive_session(|mounts| mounts.clone());
    for mount in dynamic_mounts {
        if let Some((source, fs_type, options)) = mount_metadata(mount.source_mount_id) {
            infos.push(MountInfo {
                source,
                target: mount.target_path,
                fs_type,
                options,
            });
        }
    }
    infos
}

pub(crate) fn statfs_for_mount(mount_id: MountId) -> Option<FileSystemStat> {
    with_mount(mount_id, |backend| backend.statfs())
}
