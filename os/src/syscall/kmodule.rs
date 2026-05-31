use super::errno::{SysError, SysResult};
use super::fs::get_file_by_fd;
use super::user_ptr::{PATH_MAX, read_user_c_string};
use crate::task::current_user_token;

const DNS_RESOLVER_MODULE: &str = "dns_resolver";
const DNS_RESOLVER_KO_SUFFIX: &str = "/dns_resolver.ko";
const HWPOISON_MODULE: &str = "hwpoison_inject";
const HWPOISON_KO_SUFFIX: &str = "/hwpoison_inject.ko";

pub fn sys_init_module(module_image: *const u8, len: usize, _param_values: *const u8) -> SysResult {
    if module_image.is_null() {
        return Err(SysError::EFAULT);
    }
    if len == 0 {
        return Err(SysError::ENOEXEC);
    }
    // CONTEXT: This kernel has no loadable module subsystem. BusyBox modprobe
    // reaches init_module() only after resolving a static module placeholder,
    // so treat that placeholder as an already built-in module.
    Ok(0)
}

pub fn sys_finit_module(fd: usize, _param_values: *const u8, _flags: u32) -> SysResult {
    let file = get_file_by_fd(fd)?;
    if file
        .proc_fd_target()
        .as_deref()
        .map(|path| path.ends_with(DNS_RESOLVER_KO_SUFFIX) || path.ends_with(HWPOISON_KO_SUFFIX))
        .unwrap_or(false)
    {
        return Ok(0);
    }
    Err(SysError::ENOEXEC)
}

pub fn sys_delete_module(name_ptr: *const u8, _flags: u32) -> SysResult {
    let name = read_user_c_string(current_user_token(), name_ptr, PATH_MAX)?;
    if name == DNS_RESOLVER_MODULE || name == HWPOISON_MODULE {
        return Ok(0);
    }
    Err(SysError::ENOENT)
}
