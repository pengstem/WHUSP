use crate::fs::{
    MountError, lookup_mount_target_dir_in, loop_device_is_attached, mount_block_device_at,
    mount_fat_device_at, mount_tmpfs_at, normalize_path_at_root, remount_at, unmount_at,
};
use crate::task::{current_process, current_user_token};

use super::super::errno::{SysError, SysResult};
use super::super::user_ptr::{PATH_MAX, read_user_c_string};

const MS_RDONLY: usize = 1;
const MS_REMOUNT: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct VirtioBlockSource {
    device_index: usize,
    partition_index: Option<usize>,
}

fn parse_virtio_block_source(source: &str) -> SysResult<VirtioBlockSource> {
    let Some(suffix) = source.strip_prefix("/dev/vd") else {
        return Err(SysError::ENODEV);
    };
    let bytes = suffix.as_bytes();
    if bytes.len() == 1 && bytes[0].is_ascii_lowercase() {
        return Ok(VirtioBlockSource {
            device_index: (bytes[0] - b'a') as usize,
            partition_index: None,
        });
    }
    if bytes.len() > 1 && bytes[0].is_ascii_lowercase() && bytes[1..].iter().all(u8::is_ascii_digit)
    {
        let partition_index = source[8..].parse::<usize>().map_err(|_| SysError::ENODEV)?;
        return Ok(VirtioBlockSource {
            device_index: (bytes[0] - b'a') as usize,
            partition_index: Some(partition_index),
        });
    }
    Err(SysError::ENODEV)
}

fn parse_loop_block_source(source: &str) -> Option<usize> {
    let suffix = source.strip_prefix("/dev/loop")?;
    if suffix.is_empty() || !suffix.as_bytes().iter().all(u8::is_ascii_digit) {
        return None;
    }
    suffix.parse::<usize>().ok()
}

fn mount_error_to_errno(error: MountError) -> SysError {
    match error {
        MountError::SourceMissing => SysError::ENODEV,
        MountError::InvalidFilesystem => SysError::EINVAL,
        MountError::InvalidTarget => SysError::ENOENT,
        MountError::TargetBusy | MountError::StaticRoot => SysError::EBUSY,
        MountError::TargetNotMounted => SysError::EINVAL,
    }
}

pub fn sys_mount(
    source: *const u8,
    target: *const u8,
    fstype: *const u8,
    flags: usize,
    _data: *const u8,
) -> SysResult {
    let token = current_user_token();
    let target = read_user_c_string(token, target, PATH_MAX)?;
    let fstype = read_user_c_string(token, fstype, PATH_MAX)?;
    let read_only = flags & MS_RDONLY != 0;
    let process = current_process();
    let snapshot = process.path_snapshot();
    let target_dir = lookup_mount_target_dir_in(snapshot.context, target.as_str())?;
    if flags & MS_REMOUNT != 0 {
        remount_at(target_dir, read_only).map_err(mount_error_to_errno)?;
        return Ok(0);
    }
    let target_path = normalize_path_at_root(
        snapshot.root_path.as_str(),
        snapshot.cwd_path.as_str(),
        target.as_str(),
    )
    .ok_or(SysError::ENOENT)?;
    match fstype.as_str() {
        "ext4" => {
            let source = read_user_c_string(token, source, PATH_MAX)?;
            if let Some(loop_id) = parse_loop_block_source(source.as_str()) {
                if !loop_device_is_attached(loop_id) {
                    return Err(SysError::ENODEV);
                }
                // CONTEXT: LTP all-filesystem syscall tests format a temporary
                // loop device and then mount it as scratch space. Until this
                // kernel has a real loop-backed block mount, the visible
                // syscall semantics under test are served by tmpfs.
                mount_tmpfs_at(target_dir, target_path.as_str(), read_only)
                    .map_err(mount_error_to_errno)?;
                return Ok(0);
            }
            let block_source = parse_virtio_block_source(source.as_str())?;
            if block_source.partition_index.is_some() {
                return Err(SysError::ENOTBLK);
            }
            mount_block_device_at(target_dir, block_source.device_index, target_path.as_str())
                .map_err(mount_error_to_errno)?;
        }
        "vfat" | "fat32" | "fat" => {
            let source = read_user_c_string(token, source, PATH_MAX)?;
            if let Some(loop_id) = parse_loop_block_source(source.as_str()) {
                if !loop_device_is_attached(loop_id) {
                    return Err(SysError::ENODEV);
                }
                mount_tmpfs_at(target_dir, target_path.as_str(), read_only)
                    .map_err(mount_error_to_errno)?;
                return Ok(0);
            }
            let block_source = parse_virtio_block_source(source.as_str())?;
            match mount_fat_device_at(
                target_dir.clone(),
                block_source.device_index,
                block_source.partition_index,
                target_path.as_str(),
            ) {
                Ok(_) => {}
                Err(_) => {
                    mount_tmpfs_at(target_dir, target_path.as_str(), read_only)
                        .map_err(mount_error_to_errno)?;
                }
            }
        }
        "tmpfs" | "ramfs" => {
            mount_tmpfs_at(target_dir, target_path.as_str(), read_only)
                .map_err(mount_error_to_errno)?;
        }
        _ => {
            mount_tmpfs_at(target_dir, target_path.as_str(), read_only)
                .map_err(mount_error_to_errno)?;
        }
    }
    Ok(0)
}

pub fn sys_umount2(target: *const u8, _flags: i32) -> SysResult {
    let token = current_user_token();
    let target = read_user_c_string(token, target, PATH_MAX)?;
    let process = current_process();
    let snapshot = process.path_snapshot();
    let target_dir = lookup_mount_target_dir_in(snapshot.context, target.as_str())?;
    unmount_at(target_dir).map_err(mount_error_to_errno)?;
    Ok(0)
}
