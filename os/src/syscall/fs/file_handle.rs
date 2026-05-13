use super::super::errno::{SysError, SysResult};
use super::super::user_ptr::{
    PATH_MAX, copy_to_user, read_user_c_string, read_user_value, write_user_value,
};
use super::fd::get_file_by_fd;
use super::path::path_context_from;
use super::uapi::{AT_EMPTY_PATH, AT_FDCWD};
use crate::fs::{VfsNodeId, lookup_path_in};
use crate::task::{current_process, current_user_token};

const AT_HANDLE_FID: i32 = 0x200;
const NAME_TO_HANDLE_AT_SYMLINK_FOLLOW: i32 = 0x400;
const VALID_NAME_TO_HANDLE_FLAGS: i32 =
    AT_EMPTY_PATH | NAME_TO_HANDLE_AT_SYMLINK_FOLLOW | AT_HANDLE_FID;
const MAX_HANDLE_SZ: u32 = 128;
const FILE_HANDLE_HEADER_LEN: usize = 8;
const WHUSP_FILE_HANDLE_TYPE: i32 = 0x5753_4855;

pub(crate) const WHUSP_FILE_HANDLE_BYTES: usize = 16;
pub(crate) const WHUSP_FILE_HANDLE_RECORD_LEN: usize =
    FILE_HANDLE_HEADER_LEN + WHUSP_FILE_HANDLE_BYTES;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct LinuxFileHandleHeader {
    handle_bytes: u32,
    handle_type: i32,
}

pub(crate) fn file_handle_fsid(_node: VfsNodeId) -> [i32; 2] {
    // CONTEXT: LinuxStatfs currently reports a zero fsid for all mounted
    // filesystems in this kernel. Fanotify FID records use the same fsid so
    // LTP comparisons against statfs(2) remain consistent.
    [0, 0]
}

fn encode_file_handle_payload(node: VfsNodeId) -> [u8; WHUSP_FILE_HANDLE_BYTES] {
    let mut payload = [0u8; WHUSP_FILE_HANDLE_BYTES];
    payload[0..8].copy_from_slice(&(node.mount_id.0 as u64).to_ne_bytes());
    payload[8..16].copy_from_slice(&(node.ino as u64).to_ne_bytes());
    payload
}

pub(crate) fn write_file_handle_record(record: &mut [u8], node: VfsNodeId) {
    let payload = encode_file_handle_payload(node);
    record[0..4].copy_from_slice(&(WHUSP_FILE_HANDLE_BYTES as u32).to_ne_bytes());
    record[4..8].copy_from_slice(&WHUSP_FILE_HANDLE_TYPE.to_ne_bytes());
    record[FILE_HANDLE_HEADER_LEN..WHUSP_FILE_HANDLE_RECORD_LEN].copy_from_slice(&payload);
}

fn resolve_handle_node(dirfd: isize, path: &str, flags: i32) -> SysResult<VfsNodeId> {
    let follow_final_symlink = flags & NAME_TO_HANDLE_AT_SYMLINK_FOLLOW != 0;
    if path.is_empty() {
        if flags & AT_EMPTY_PATH == 0 {
            return Err(SysError::ENOENT);
        }
        if dirfd == AT_FDCWD {
            let snapshot = current_process().path_snapshot();
            let context = path_context_from(&snapshot, AT_FDCWD, ".")?;
            return Ok(lookup_path_in(context, ".", follow_final_symlink)?.node);
        }
        if dirfd < 0 {
            return Err(SysError::EBADF);
        }
        let file = get_file_by_fd(dirfd as usize)?;
        return file.vfs_node_id().ok_or(SysError::EBADF);
    }

    let snapshot = current_process().path_snapshot();
    let context = path_context_from(&snapshot, dirfd, path)?;
    Ok(lookup_path_in(context, path, follow_final_symlink)?.node)
}

pub fn sys_name_to_handle_at(
    dirfd: isize,
    pathname: *const u8,
    handle: *mut u8,
    mount_id: *mut i32,
    flags: i32,
) -> SysResult {
    if flags & !VALID_NAME_TO_HANDLE_FLAGS != 0 {
        return Err(SysError::EINVAL);
    }
    if handle.is_null() || mount_id.is_null() {
        return Err(SysError::EFAULT);
    }

    let token = current_user_token();
    let path = read_user_c_string(token, pathname, PATH_MAX)?;
    let node = resolve_handle_node(dirfd, path.as_str(), flags)?;
    let mut header: LinuxFileHandleHeader =
        read_user_value(token, handle as *const LinuxFileHandleHeader)?;
    if header.handle_bytes > MAX_HANDLE_SZ {
        return Err(SysError::EINVAL);
    }

    let mount_id_value = node.mount_id.0 as i32;
    write_user_value(token, mount_id, &mount_id_value)?;
    if header.handle_bytes < WHUSP_FILE_HANDLE_BYTES as u32 {
        header.handle_bytes = WHUSP_FILE_HANDLE_BYTES as u32;
        write_user_value(token, handle as *mut LinuxFileHandleHeader, &header)?;
        return Err(SysError::EOVERFLOW);
    }

    header.handle_bytes = WHUSP_FILE_HANDLE_BYTES as u32;
    header.handle_type = WHUSP_FILE_HANDLE_TYPE;
    write_user_value(token, handle as *mut LinuxFileHandleHeader, &header)?;
    let payload = encode_file_handle_payload(node);
    copy_to_user(token, handle.wrapping_add(FILE_HANDLE_HEADER_LEN), &payload)?;
    Ok(0)
}
