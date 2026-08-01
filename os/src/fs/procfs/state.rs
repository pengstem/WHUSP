use super::super::pipe::{PIPE_DEFAULT_CAPACITY, PIPE_MAX_CAPACITY};
use crate::config::PAGE_SIZE;
use crate::sync::SpinNoIrqLock;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicIsize, AtomicUsize};
use lazy_static::lazy_static;

const DEFAULT_PID_MAX: usize = 4_194_304;
// CONTEXT: Linux defaults this sysctl to 16384 pages, but this kernel does not
// account pipe pages per user and still has a smaller fd-table ceiling. Expose
// one default pipe worth of pages so pipe-limit tests exercise real pipe
// behavior instead of deriving a zero-pipe workload.
const DEFAULT_PIPE_USER_PAGES_SOFT: usize = PIPE_DEFAULT_CAPACITY / PAGE_SIZE;
const DEFAULT_LEASE_BREAK_TIME: usize = 45;
pub(super) const DEFAULT_NET_IPV4_CONF_TAG: isize = 0;
pub(super) const PROC_MEMINFO_OBSERVED_CACHE_KB: usize = 64 * 1024;

pub(super) static PROC_PID_MAX: AtomicUsize = AtomicUsize::new(DEFAULT_PID_MAX);
pub(super) static PROC_PIPE_MAX_SIZE: AtomicUsize = AtomicUsize::new(PIPE_MAX_CAPACITY);
pub(super) static PROC_PIPE_USER_PAGES_SOFT: AtomicUsize =
    AtomicUsize::new(DEFAULT_PIPE_USER_PAGES_SOFT);
pub(super) static PROC_LEASE_BREAK_TIME: AtomicUsize = AtomicUsize::new(DEFAULT_LEASE_BREAK_TIME);
pub(super) static PROC_NET_IPV4_CONF_LO_TAG: AtomicIsize =
    AtomicIsize::new(DEFAULT_NET_IPV4_CONF_TAG);
pub(super) static PROC_NET_CORE_BUSY_READ: AtomicUsize = AtomicUsize::new(0);
pub(super) static PROC_NET_CORE_BUSY_POLL: AtomicUsize = AtomicUsize::new(0);
pub(super) static PROC_VFS_CACHE_PRESSURE: AtomicUsize = AtomicUsize::new(100);
pub(super) static PROC_MEMINFO_CACHED_KB: AtomicUsize = AtomicUsize::new(0);
pub(super) static PROC_MEMINFO_SWAP_CACHED_KB: AtomicUsize = AtomicUsize::new(0);
pub(super) static PROC_IO_READ_BYTES: AtomicUsize = AtomicUsize::new(0);
pub(super) static PROC_IO_READAHEAD_SUPPRESS_READS: AtomicUsize = AtomicUsize::new(0);
pub(super) static PROC_OOM_SCORE_ADJ: AtomicIsize = AtomicIsize::new(0);

lazy_static! {
    pub(super) static ref PROC_DOMAINNAME: SpinNoIrqLock<Vec<u8>> = {
        let mut value = Vec::new();
        value.extend_from_slice(b"(none)");
        SpinNoIrqLock::new(value)
    };
    pub(super) static ref PROC_CORE_PATTERN: SpinNoIrqLock<Vec<u8>> = {
        let mut value = Vec::new();
        value.extend_from_slice(b"core");
        SpinNoIrqLock::new(value)
    };
}
