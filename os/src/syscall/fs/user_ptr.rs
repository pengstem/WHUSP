use crate::mm::{PageTable, StepByOne, VirtAddr};
use alloc::string::String;
use alloc::vec::Vec;
use core::mem::{MaybeUninit, size_of};

use super::super::errno::{SysError, SysResult};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum UserBufferAccess {
    Read,
    Write,
}

// TODO: i think these functions are taking the responsibility of the mm module
pub(super) fn translated_byte_buffer_checked(
    token: usize,
    ptr: *const u8,
    len: usize,
    access: UserBufferAccess,
) -> SysResult<Vec<&'static mut [u8]>> {
    if len == 0 {
        return Ok(Vec::new());
    }
    let mut start = ptr as usize;
    let end = start.checked_add(len).ok_or(SysError::EFAULT)?;
    let page_table = PageTable::from_token(token);
    let mut buffers = Vec::new();
    while start < end {
        let start_va = VirtAddr::from(start);
        let mut vpn = start_va.floor();
        let pte = page_table.translate(vpn).ok_or(SysError::EFAULT)?;
        let permitted = match access {
            UserBufferAccess::Read => pte.readable(),
            UserBufferAccess::Write => pte.writable(),
        };
        if !pte.is_valid() || !permitted {
            return Err(SysError::EFAULT);
        }
        let ppn = pte.ppn();
        vpn.step();
        let mut end_va: VirtAddr = vpn.into();
        end_va = end_va.min(VirtAddr::from(end));
        if end_va.page_offset() == 0 {
            buffers.push(&mut ppn.get_bytes_array()[start_va.page_offset()..]);
        } else {
            buffers.push(&mut ppn.get_bytes_array()[start_va.page_offset()..end_va.page_offset()]);
        }
        start = end_va.into();
    }
    Ok(buffers)
}

pub(super) const PATH_MAX: usize = 4096;

pub(super) fn read_user_c_string(
    token: usize,
    ptr: *const u8,
    max_len: usize,
) -> SysResult<String> {
    if ptr.is_null() {
        return Err(SysError::EFAULT);
    }

    let mut string = String::with_capacity(64);
    let mut offset = 0usize;
    while offset < max_len {
        let addr = (ptr as usize).checked_add(offset).ok_or(SysError::EFAULT)?;
        let page_remaining = crate::config::PAGE_SIZE - (addr & (crate::config::PAGE_SIZE - 1));
        let chunk_len = page_remaining.min(max_len - offset);
        let buffers = translated_byte_buffer_checked(
            token,
            addr as *const u8,
            chunk_len,
            UserBufferAccess::Read,
        )?;
        for buffer in &buffers {
            for &byte in buffer.iter() {
                if byte == 0 {
                    return Ok(string);
                }
                string.push(byte as char);
            }
        }
        offset += chunk_len;
    }
    Err(SysError::ENAMETOOLONG)
}

pub(super) fn read_user_usize(token: usize, addr: usize) -> SysResult<usize> {
    let mut bytes = [0u8; size_of::<usize>()];
    let buffers = translated_byte_buffer_checked(
        token,
        addr as *const u8,
        bytes.len(),
        UserBufferAccess::Read,
    )?;
    let mut copied = 0usize;
    for buffer in buffers.iter() {
        let next = copied + buffer.len();
        bytes[copied..next].copy_from_slice(buffer);
        copied = next;
    }
    Ok(usize::from_ne_bytes(bytes))
}

fn copy_from_user(token: usize, ptr: *const u8, dst: &mut [u8]) -> SysResult<()> {
    let buffers = translated_byte_buffer_checked(token, ptr, dst.len(), UserBufferAccess::Read)?;
    let mut copied = 0usize;
    for buffer in buffers.iter() {
        let next = copied + buffer.len();
        dst[copied..next].copy_from_slice(buffer);
        copied = next;
    }
    Ok(())
}

pub(super) fn copy_to_user(token: usize, ptr: *mut u8, src: &[u8]) -> SysResult<()> {
    let buffers = translated_byte_buffer_checked(
        token,
        ptr.cast_const(),
        src.len(),
        UserBufferAccess::Write,
    )?;
    let mut copied = 0usize;
    for buffer in buffers {
        let next = copied + buffer.len();
        buffer.copy_from_slice(&src[copied..next]);
        copied = next;
    }
    Ok(())
}

pub(in crate::syscall) fn read_user_value<T: Copy>(token: usize, ptr: *const T) -> SysResult<T> {
    let mut value = MaybeUninit::<T>::uninit();
    let bytes =
        unsafe { core::slice::from_raw_parts_mut(value.as_mut_ptr().cast::<u8>(), size_of::<T>()) };
    copy_from_user(token, ptr.cast::<u8>(), bytes)?;
    Ok(unsafe { value.assume_init() })
}

pub(in crate::syscall) fn write_user_value<T: Copy>(
    token: usize,
    ptr: *mut T,
    value: &T,
) -> SysResult<()> {
    let bytes =
        unsafe { core::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) };
    copy_to_user(token, ptr.cast::<u8>(), bytes)
}
