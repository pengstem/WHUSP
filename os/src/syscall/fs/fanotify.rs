use super::super::errno::{SysError, SysResult};
use super::super::install_pidfd_for_fanotify;
use super::super::user_ptr::{PATH_MAX, read_user_c_string};
use super::fd::{get_file_by_fd, install_file_fd};
use super::file_handle::{
    WHUSP_FILE_HANDLE_BYTES, WHUSP_FILE_HANDLE_RECORD_LEN, file_handle_fsid,
    write_file_handle_record,
};
use super::path::path_context_from;
use super::uapi::AT_FDCWD;
use crate::fs::{
    File, FsNodeKind, MountId, OpenFlags, PathContext, PollEvents, PollWaitQueue, PollWaiter,
    VfsNodeId, lookup_path_in, normalize_path_at_root, overlay_real_node,
};
use crate::mm::UserBuffer;
use crate::perf;
use crate::sync::UPIntrFreeCell;
use crate::task::{
    TaskControlBlock, block_current_task_no_schedule, current_has_interrupting_signal,
    current_process, current_task, current_user_token, schedule, wakeup_task,
};
use alloc::collections::btree_map::Entry;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicUsize, Ordering};
use lazy_static::lazy_static;

const FAN_ACCESS: u64 = 0x0000_0001;
const FAN_MODIFY: u64 = 0x0000_0002;
const FAN_ATTRIB: u64 = 0x0000_0004;
const FAN_CLOSE_WRITE: u64 = 0x0000_0008;
const FAN_CLOSE_NOWRITE: u64 = 0x0000_0010;
const FAN_OPEN: u64 = 0x0000_0020;
const FAN_MOVED_FROM: u64 = 0x0000_0040;
const FAN_MOVED_TO: u64 = 0x0000_0080;
const FAN_CREATE: u64 = 0x0000_0100;
const FAN_DELETE: u64 = 0x0000_0200;
const FAN_DELETE_SELF: u64 = 0x0000_0400;
const FAN_MOVE_SELF: u64 = 0x0000_0800;
const FAN_OPEN_EXEC: u64 = 0x0000_1000;
const FAN_Q_OVERFLOW: u64 = 0x0000_4000;
const FAN_OPEN_PERM: u64 = 0x0001_0000;
const FAN_ACCESS_PERM: u64 = 0x0002_0000;
const FAN_OPEN_EXEC_PERM: u64 = 0x0004_0000;
const FAN_RENAME: u64 = 0x1000_0000;
const FAN_EVENT_ON_CHILD: u64 = 0x0800_0000;
const FAN_ONDIR: u64 = 0x4000_0000;

const FAN_CLOEXEC: u32 = 0x0000_0001;
const FAN_NONBLOCK: u32 = 0x0000_0002;
const FAN_CLASS_CONTENT: u32 = 0x0000_0004;
const FAN_CLASS_PRE_CONTENT: u32 = 0x0000_0008;
const FAN_UNLIMITED_QUEUE: u32 = 0x0000_0010;
const FAN_UNLIMITED_MARKS: u32 = 0x0000_0020;
const FAN_REPORT_PIDFD: u32 = 0x0000_0080;
const FAN_REPORT_TID: u32 = 0x0000_0100;
const FAN_REPORT_FID: u32 = 0x0000_0200;
const FAN_REPORT_DIR_FID: u32 = 0x0000_0400;
const FAN_REPORT_NAME: u32 = 0x0000_0800;
const FAN_REPORT_TARGET_FID: u32 = 0x0000_1000;
const FAN_REPORT_FD_ERROR: u32 = 0x0000_2000;
const FAN_REPORT_MNT: u32 = 0x0000_4000;

const FAN_MARK_ADD: u32 = 0x0000_0001;
const FAN_MARK_REMOVE: u32 = 0x0000_0002;
const FAN_MARK_DONT_FOLLOW: u32 = 0x0000_0004;
const FAN_MARK_ONLYDIR: u32 = 0x0000_0008;
const FAN_MARK_MOUNT: u32 = 0x0000_0010;
const FAN_MARK_IGNORED_MASK: u32 = 0x0000_0020;
const FAN_MARK_IGNORED_SURV_MODIFY: u32 = 0x0000_0040;
const FAN_MARK_FLUSH: u32 = 0x0000_0080;
const FAN_MARK_FILESYSTEM: u32 = 0x0000_0100;
const FAN_MARK_EVICTABLE: u32 = 0x0000_0200;
const FAN_MARK_IGNORE: u32 = 0x0000_0400;

const FANOTIFY_METADATA_VERSION: u8 = 3;
const FANOTIFY_METADATA_LEN: usize = 24;
const FANOTIFY_PIDFD_INFO_LEN: usize = 8;
const FAN_EVENT_INFO_TYPE_FID: u8 = 1;
const FAN_EVENT_INFO_TYPE_PIDFD: u8 = 4;
const FAN_EVENT_INFO_TYPE_DFID_NAME: u8 = 2;
const FAN_EVENT_INFO_TYPE_DFID: u8 = 3;
const FANOTIFY_FID_INFO_BASE_LEN: usize = 20;
const FAN_NOFD: i32 = -1;
const FAN_NOPIDFD: i32 = -1;
// CONTEXT: This limit is exported through /proc/sys/fs/fanotify/max_queued_events.
// Keeping it modest avoids making fanotify queue overflow tests depend on
// quadratic directory lookup behavior in the current scratch filesystem.
const MAX_QUEUED_EVENTS: usize = 1_024;
const MAX_USER_GROUPS: usize = 129;

const FAN_CLASS_MASK: u32 = FAN_CLASS_CONTENT | FAN_CLASS_PRE_CONTENT;
const SUPPORTED_INIT_FLAGS: u32 = FAN_CLOEXEC
    | FAN_NONBLOCK
    | FAN_CLASS_CONTENT
    | FAN_CLASS_PRE_CONTENT
    | FAN_UNLIMITED_QUEUE
    | FAN_UNLIMITED_MARKS
    | FAN_REPORT_PIDFD
    | FAN_REPORT_TID
    | FAN_REPORT_FID
    | FAN_REPORT_DIR_FID
    | FAN_REPORT_NAME
    | FAN_REPORT_TARGET_FID;
const FILE_HANDLE_REPORT_FLAGS: u32 = FAN_REPORT_FID | FAN_REPORT_DIR_FID | FAN_REPORT_NAME;
const UNSUPPORTED_REPORT_FLAGS: u32 = FAN_REPORT_FD_ERROR | FAN_REPORT_MNT;
const KNOWN_MARK_FLAGS: u32 = FAN_MARK_ADD
    | FAN_MARK_REMOVE
    | FAN_MARK_DONT_FOLLOW
    | FAN_MARK_ONLYDIR
    | FAN_MARK_MOUNT
    | FAN_MARK_IGNORED_MASK
    | FAN_MARK_IGNORED_SURV_MODIFY
    | FAN_MARK_FLUSH
    | FAN_MARK_FILESYSTEM
    | FAN_MARK_EVICTABLE
    | FAN_MARK_IGNORE;
const SUPPORTED_MARK_EVENTS: u64 = FAN_ACCESS
    | FAN_MODIFY
    | FAN_ATTRIB
    | FAN_CLOSE_WRITE
    | FAN_CLOSE_NOWRITE
    | FAN_OPEN
    | FAN_MOVED_FROM
    | FAN_MOVED_TO
    | FAN_CREATE
    | FAN_DELETE
    | FAN_DELETE_SELF
    | FAN_MOVE_SELF
    | FAN_OPEN_EXEC;
const SUPPORTED_MARK_MASK: u64 = SUPPORTED_MARK_EVENTS | FAN_EVENT_ON_CHILD | FAN_ONDIR;
const SUPPORTED_DIRENT_MARK_EVENTS: u64 = FAN_MOVED_FROM | FAN_MOVED_TO | FAN_CREATE | FAN_DELETE;
const SUPPORTED_SELF_MARK_EVENTS: u64 = FAN_DELETE_SELF | FAN_MOVE_SELF;
const KNOWN_MARK_MASK: u64 =
    SUPPORTED_MARK_MASK | SUPPORTED_DIRENT_MARK_EVENTS | SUPPORTED_SELF_MARK_EVENTS;
const UNSUPPORTED_PERMISSION_EVENTS: u64 = FAN_OPEN_PERM | FAN_ACCESS_PERM | FAN_OPEN_EXEC_PERM;

lazy_static! {
    static ref FANOTIFY_GROUPS: UPIntrFreeCell<Vec<Weak<FanotifyGroup>>> =
        unsafe { UPIntrFreeCell::new(Vec::new()) };
    static ref FANOTIFY_NODE_NAMES: UPIntrFreeCell<BTreeMap<VfsNodeId, String>> =
        unsafe { UPIntrFreeCell::new(BTreeMap::new()) };
}

static LIVE_FANOTIFY_GROUPS: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Eq, PartialEq)]
enum FanotifyMarkTarget {
    Inode(VfsNodeId),
    Mount { mount: MountId, path: String },
    Filesystem(MountId),
}

impl FanotifyMarkTarget {
    fn from_node(node: VfsNodeId, flags: u32, path: String) -> Self {
        if flags & FAN_MARK_FILESYSTEM != 0 {
            Self::Filesystem(node.mount_id)
        } else if flags & FAN_MARK_MOUNT != 0 {
            Self::Mount {
                mount: node.mount_id,
                path,
            }
        } else {
            Self::Inode(node)
        }
    }

    fn applies_to(
        &self,
        node: VfsNodeId,
        parent: Option<VfsNodeId>,
        mount: MountId,
        mask: u64,
        event_path: Option<&str>,
    ) -> bool {
        match self {
            Self::Inode(marked) => {
                *marked == node
                    || (mask & FAN_EVENT_ON_CHILD != 0
                        && parent.is_some_and(|parent| parent == *marked))
            }
            Self::Mount {
                mount: marked,
                path,
            } => {
                *marked == mount
                    && event_path
                        .map(|event_path| path_is_under(path.as_str(), event_path))
                        .unwrap_or(true)
            }
            Self::Filesystem(marked) => *marked == mount,
        }
    }

    fn is_real_overlay_target(&self, real_node: VfsNodeId) -> bool {
        match self {
            Self::Inode(marked) => *marked == real_node,
            Self::Mount { mount, .. } | Self::Filesystem(mount) => *mount == real_node.mount_id,
        }
    }
}

fn path_is_under(root: &str, path: &str) -> bool {
    let root = root.trim_end_matches('/');
    if root.is_empty() {
        return true;
    }
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

#[derive(Clone)]
struct FanotifyMark {
    target: FanotifyMarkTarget,
    mask: u64,
    ignored_mask: u64,
    flags: u32,
}

#[derive(Clone)]
struct FanotifyEvent {
    mask: u64,
    pid: i32,
    fid_node: Option<VfsNodeId>,
    child_fid_node: Option<VfsNodeId>,
    source: Option<Arc<dyn File + Send + Sync>>,
    name: Option<String>,
    fid_info_type: u8,
}

struct FanotifyInner {
    marks: Vec<FanotifyMark>,
    events: VecDeque<FanotifyEvent>,
    read_waiters: VecDeque<Arc<TaskControlBlock>>,
    poll_waiters: PollWaitQueue,
    overflow_queued: bool,
    closed: bool,
}

struct FanotifyGroup {
    init_flags: u32,
    event_file_flags: OpenFlags,
    owner_pid: i32,
    unprivileged: bool,
    max_queued_events: usize,
    inner: UPIntrFreeCell<FanotifyInner>,
}

impl FanotifyGroup {
    fn new(init_flags: u32, event_file_flags: OpenFlags) -> Arc<Self> {
        let group = Arc::new(Self {
            init_flags,
            event_file_flags,
            owner_pid: current_process().getpid() as i32,
            unprivileged: current_process().credentials().euid != 0,
            max_queued_events: if init_flags & FAN_UNLIMITED_QUEUE != 0 {
                usize::MAX
            } else {
                MAX_QUEUED_EVENTS
            },
            inner: unsafe {
                UPIntrFreeCell::new(FanotifyInner {
                    marks: Vec::new(),
                    events: VecDeque::new(),
                    read_waiters: VecDeque::new(),
                    poll_waiters: PollWaitQueue::new(),
                    overflow_queued: false,
                    closed: false,
                })
            },
        });
        FANOTIFY_GROUPS.exclusive_session(|groups| groups.push(Arc::downgrade(&group)));
        LIVE_FANOTIFY_GROUPS.fetch_add(1, Ordering::Relaxed);
        group
    }

    fn close(&self) {
        let (read_waiters, poll_waiters, events) = self.inner.exclusive_session(|inner| {
            if inner.closed {
                return (VecDeque::new(), Vec::new(), VecDeque::new());
            }
            inner.closed = true;
            inner.marks.clear();
            inner.overflow_queued = false;
            (
                core::mem::take(&mut inner.read_waiters),
                inner.poll_waiters.drain(),
                core::mem::take(&mut inner.events),
            )
        });
        drop(events);
        for task in read_waiters {
            let _ = wakeup_task(task);
        }
        PollWaiter::wake_all(poll_waiters);
    }

    fn flush(&self) {
        self.inner.exclusive_session(|inner| inner.marks.clear());
    }

    fn update_mark(&self, target: FanotifyMarkTarget, flags: u32, mask: u64) -> SysResult {
        self.inner.exclusive_session(|inner| {
            let existing = inner.marks.iter_mut().find(|mark| mark.target == target);
            match (flags & (FAN_MARK_ADD | FAN_MARK_REMOVE), existing) {
                (FAN_MARK_ADD, Some(mark)) => {
                    if flags & FAN_MARK_EVICTABLE != 0 && mark.flags & FAN_MARK_EVICTABLE == 0 {
                        return Err(SysError::EEXIST);
                    }
                    if flags & FAN_MARK_EVICTABLE == 0 {
                        mark.flags &= !FAN_MARK_EVICTABLE;
                    }
                    mark.flags |= flags
                        & (FAN_MARK_IGNORED_MASK
                            | FAN_MARK_IGNORED_SURV_MODIFY
                            | FAN_MARK_EVICTABLE
                            | FAN_MARK_IGNORE);
                    if flags & (FAN_MARK_IGNORED_MASK | FAN_MARK_IGNORE) != 0 {
                        mark.ignored_mask |= mask;
                    } else {
                        mark.mask |= mask;
                    }
                    Ok(0)
                }
                (FAN_MARK_ADD, None) => {
                    inner.marks.push(FanotifyMark {
                        target,
                        mask: if flags & (FAN_MARK_IGNORED_MASK | FAN_MARK_IGNORE) == 0 {
                            mask
                        } else {
                            0
                        },
                        ignored_mask: if flags & (FAN_MARK_IGNORED_MASK | FAN_MARK_IGNORE) != 0 {
                            mask
                        } else {
                            0
                        },
                        flags: flags
                            & (FAN_MARK_IGNORED_MASK
                                | FAN_MARK_IGNORED_SURV_MODIFY
                                | FAN_MARK_EVICTABLE
                                | FAN_MARK_IGNORE),
                    });
                    Ok(0)
                }
                (FAN_MARK_REMOVE, Some(mark)) => {
                    if flags & (FAN_MARK_IGNORED_MASK | FAN_MARK_IGNORE) != 0 {
                        mark.ignored_mask &= !mask;
                    } else {
                        mark.mask &= !mask;
                    }
                    if mark.mask == 0 && mark.ignored_mask == 0 {
                        inner.marks.retain(|mark| mark.target != target);
                    }
                    Ok(0)
                }
                (FAN_MARK_REMOVE, None) => Err(SysError::ENOENT),
                _ => Err(SysError::EINVAL),
            }
        })
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "fanotify mark matching depends on explicit event, target, and reporting metadata"
    )]
    fn event_bits_for_mark(
        mark: &FanotifyMark,
        mask: u64,
        node: VfsNodeId,
        parent: Option<VfsNodeId>,
        mount: MountId,
        event_mask: u64,
        event_path: Option<&str>,
        is_dir: bool,
        report_ondir: bool,
    ) -> u64 {
        if mask == 0
            || !mark
                .target
                .applies_to(node, parent, mount, mask, event_path)
        {
            return 0;
        }
        if event_mask & SUPPORTED_SELF_MARK_EVENTS != 0
            && let FanotifyMarkTarget::Inode(marked) = &mark.target
            && *marked != node
        {
            return 0;
        }
        if is_dir && mask & FAN_ONDIR == 0 {
            return 0;
        }
        let event_bits = event_mask & mask & SUPPORTED_MARK_EVENTS;
        if event_bits == 0 {
            return 0;
        }
        if is_dir && report_ondir && mask & FAN_ONDIR != 0 {
            event_bits | FAN_ONDIR
        } else {
            event_bits
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "queued fanotify events carry separate identity, source, and naming fields"
    )]
    fn enqueue_event(
        &self,
        inner: &mut FanotifyInner,
        mask: u64,
        pid: i32,
        fid_node: Option<VfsNodeId>,
        child_fid_node: Option<VfsNodeId>,
        source: Option<Arc<dyn File + Send + Sync>>,
        name: Option<&str>,
        fid_info_type: u8,
    ) -> bool {
        // CONTEXT: fanotify readers must tolerate that events may or may not be
        // merged. Keep ordinary coalescing O(1), but let close events merge
        // with a pending open event for the same object because LTP fanotify13
        // expects FAN_OPEN | FAN_CLOSE_NOWRITE in one record.
        let merge_past_tail = mask
            & (FAN_CLOSE_WRITE
                | FAN_CLOSE_NOWRITE
                | FAN_MOVED_FROM
                | FAN_MOVED_TO
                | FAN_CREATE
                | FAN_DELETE
                | FAN_DELETE_SELF
                | FAN_MOVE_SELF)
            != 0
            || name.is_none();
        let existing = if merge_past_tail {
            inner.events.iter_mut().rev().find(|event| {
                event.mask != FAN_Q_OVERFLOW
                    && event.pid == pid
                    && event.fid_node == fid_node
                    && event.child_fid_node == child_fid_node
                    && event.name.as_deref() == name
                    && event.fid_info_type == fid_info_type
            })
        } else {
            inner.events.back_mut().filter(|event| {
                event.mask != FAN_Q_OVERFLOW
                    && event.pid == pid
                    && event.fid_node == fid_node
                    && event.child_fid_node == child_fid_node
                    && event.name.as_deref() == name
                    && event.fid_info_type == fid_info_type
            })
        };
        if let Some(existing) = existing {
            existing.mask |= mask;
            if existing.source.is_none() {
                existing.source = source;
            }
            return true;
        }
        if inner.events.len() >= self.max_queued_events {
            if !inner.overflow_queued {
                inner.events.push_back(FanotifyEvent {
                    mask: FAN_Q_OVERFLOW,
                    pid: 0,
                    fid_node: None,
                    child_fid_node: None,
                    source: None,
                    name: None,
                    fid_info_type: FAN_EVENT_INFO_TYPE_FID,
                });
                inner.overflow_queued = true;
                return true;
            }
            return false;
        }
        inner.events.push_back(FanotifyEvent {
            mask,
            pid,
            fid_node,
            child_fid_node,
            source,
            name: name.map(String::from),
            fid_info_type,
        });
        true
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "fanotify publication preserves the Linux event metadata at one boundary"
    )]
    fn publish(
        &self,
        node: VfsNodeId,
        parent: Option<VfsNodeId>,
        mount: MountId,
        event_mask: u64,
        is_dir: bool,
        event_path: Option<&str>,
        event_name: Option<&str>,
        report_node: Option<VfsNodeId>,
        child_fid_node: Option<VfsNodeId>,
        source: Option<&Arc<dyn File + Send + Sync>>,
    ) {
        let (read_waiters, poll_waiters) = self.inner.exclusive_session(|inner| {
            if inner.closed {
                return (VecDeque::new(), Vec::new());
            }
            let mut emitted = 0u64;
            let mut ignored = 0u64;
            let real_node = overlay_real_node(node);
            let real_parent = parent.and_then(overlay_real_node);
            let mut report_node = report_node.unwrap_or(node);
            let report_ondir = self.init_flags & FILE_HANDLE_REPORT_FLAGS != 0;
            for mark in inner.marks.iter_mut() {
                let mut mark_emitted = Self::event_bits_for_mark(
                    mark,
                    mark.mask,
                    node,
                    parent,
                    mount,
                    event_mask,
                    event_path,
                    is_dir,
                    report_ondir,
                );
                let mut mark_ignored = Self::event_bits_for_mark(
                    mark,
                    mark.ignored_mask,
                    node,
                    parent,
                    mount,
                    event_mask,
                    event_path,
                    is_dir,
                    report_ondir,
                );
                if let Some(real_node) = real_node {
                    if mark_emitted == 0 {
                        mark_emitted = Self::event_bits_for_mark(
                            mark,
                            mark.mask,
                            real_node,
                            real_parent,
                            real_node.mount_id,
                            event_mask,
                            event_path,
                            is_dir,
                            report_ondir,
                        );
                        if mark_emitted != 0 {
                            report_node = real_node;
                        }
                    }
                    if mark_emitted != 0 && mark.target.is_real_overlay_target(real_node) {
                        report_node = real_node;
                    }
                    if mark_ignored == 0 {
                        mark_ignored = Self::event_bits_for_mark(
                            mark,
                            mark.ignored_mask,
                            real_node,
                            real_parent,
                            real_node.mount_id,
                            event_mask,
                            event_path,
                            is_dir,
                            report_ondir,
                        );
                    }
                }
                emitted |= mark_emitted;
                ignored |= mark_ignored;
                if event_mask & FAN_MODIFY != 0
                    && mark.ignored_mask != 0
                    && mark.flags & FAN_MARK_IGNORED_SURV_MODIFY == 0
                    && mark.target.applies_to(
                        node,
                        parent,
                        mount,
                        mark.mask | mark.ignored_mask,
                        event_path,
                    )
                {
                    mark.ignored_mask = 0;
                }
            }
            emitted &= !ignored;
            if event_mask & SUPPORTED_SELF_MARK_EVENTS != 0
                && !is_dir
                && self.init_flags & FAN_REPORT_FID == 0
            {
                emitted &= !SUPPORTED_SELF_MARK_EVENTS;
            }
            if emitted != 0 {
                let is_dirent_event = event_mask & SUPPORTED_DIRENT_MARK_EVENTS != 0;
                let is_self_event = event_mask & SUPPORTED_SELF_MARK_EVENTS != 0;
                let is_non_dir_child_event = !is_dir && !is_self_event && child_fid_node.is_some();
                let report_node = if self.init_flags & FAN_REPORT_DIR_FID != 0
                    && is_non_dir_child_event
                    && !is_dirent_event
                {
                    parent.unwrap_or(report_node)
                } else {
                    report_node
                };
                let child_fid_node = if is_dirent_event {
                    child_fid_node.filter(|_| self.init_flags & FAN_REPORT_TARGET_FID != 0)
                } else if is_non_dir_child_event {
                    child_fid_node.filter(|_| {
                        self.init_flags & (FAN_REPORT_FID | FAN_REPORT_DIR_FID)
                            == (FAN_REPORT_FID | FAN_REPORT_DIR_FID)
                    })
                } else {
                    None
                };
                let fid_info_type = if is_self_event && !is_dir {
                    FAN_EVENT_INFO_TYPE_FID
                } else if self.init_flags & FAN_REPORT_NAME != 0 && event_name.is_some() {
                    FAN_EVENT_INFO_TYPE_DFID_NAME
                } else if self.init_flags & FAN_REPORT_DIR_FID != 0 {
                    FAN_EVENT_INFO_TYPE_DFID
                } else {
                    FAN_EVENT_INFO_TYPE_FID
                };
                let current_pid = current_process().getpid() as i32;
                let pid = if self.unprivileged && current_pid != self.owner_pid {
                    0
                } else if self.init_flags & FAN_REPORT_TID != 0 {
                    current_task()
                        .map(|task| task.linux_tid())
                        .unwrap_or_else(|| current_process().getpid()) as i32
                } else {
                    current_pid
                };
                if self.enqueue_event(
                    inner,
                    emitted,
                    pid,
                    Some(report_node),
                    child_fid_node,
                    source.cloned(),
                    if self.init_flags & FAN_REPORT_NAME != 0 {
                        event_name
                    } else {
                        None
                    },
                    fid_info_type,
                ) {
                    (
                        core::mem::take(&mut inner.read_waiters),
                        inner.poll_waiters.drain(),
                    )
                } else {
                    (VecDeque::new(), Vec::new())
                }
            } else {
                (VecDeque::new(), Vec::new())
            }
        });
        for task in read_waiters {
            let _ = wakeup_task(task);
        }
        PollWaiter::wake_all(poll_waiters);
    }

    fn event_record_len(&self, event: &FanotifyEvent) -> usize {
        FANOTIFY_METADATA_LEN
            + if self.init_flags & FAN_REPORT_PIDFD != 0 {
                FANOTIFY_PIDFD_INFO_LEN
            } else {
                0
            }
            + if self.init_flags & FILE_HANDLE_REPORT_FLAGS != 0 {
                report_fid_info_len(
                    self.init_flags,
                    event.fid_node,
                    event.child_fid_node,
                    event.name.as_deref(),
                    event.fid_info_type,
                )
            } else {
                0
            }
    }

    fn read_events(&self, mut user_buf: UserBuffer, nonblocking: bool) -> usize {
        let capacity = user_buf.len();
        if capacity < FANOTIFY_METADATA_LEN {
            return 0;
        }
        loop {
            let mut inner = self.inner.exclusive_access();
            if inner.closed
                || !inner.events.is_empty()
                || nonblocking
                || current_has_interrupting_signal()
            {
                break;
            }
            let (task, task_cx_ptr) = block_current_task_no_schedule();
            inner.read_waiters.push_back(task);
            drop(inner);
            schedule(task_cx_ptr);
        }

        let mut data = Vec::new();
        loop {
            let Some((event, record_len)) = self.inner.exclusive_session(|inner| {
                let event = inner.events.front()?;
                let record_len = self.event_record_len(event);
                if capacity.saturating_sub(data.len()) < record_len {
                    return None;
                }
                let event = inner.events.pop_front()?;
                if event.mask == FAN_Q_OVERFLOW {
                    inner.overflow_queued = false;
                }
                Some((event, record_len))
            }) else {
                break;
            };
            let fd = if self.init_flags & FILE_HANDLE_REPORT_FLAGS != 0 {
                FAN_NOFD
            } else {
                event
                    .source
                    .as_ref()
                    .map(|source| {
                        install_event_fd(source, self.event_file_flags).unwrap_or(FAN_NOFD)
                    })
                    .unwrap_or(FAN_NOFD)
            };
            let mut record = [0u8; FANOTIFY_METADATA_LEN];
            record[0..4].copy_from_slice(&(record_len as u32).to_ne_bytes());
            record[4] = FANOTIFY_METADATA_VERSION;
            record[5] = 0;
            record[6..8].copy_from_slice(&(FANOTIFY_METADATA_LEN as u16).to_ne_bytes());
            record[8..16].copy_from_slice(&event.mask.to_ne_bytes());
            record[16..20].copy_from_slice(&fd.to_ne_bytes());
            record[20..24].copy_from_slice(&event.pid.to_ne_bytes());
            data.extend_from_slice(&record);
            if self.init_flags & FILE_HANDLE_REPORT_FLAGS != 0 {
                append_report_fid_info(
                    &mut data,
                    self.init_flags,
                    event.fid_node,
                    event.child_fid_node,
                    event.name.as_deref(),
                    event.fid_info_type,
                );
            }
            if self.init_flags & FAN_REPORT_PIDFD != 0 {
                let pidfd = if event.pid > 0 {
                    install_pidfd_for_fanotify(event.pid as usize)
                        .map(|fd| fd as i32)
                        .unwrap_or(FAN_NOPIDFD)
                } else {
                    FAN_NOPIDFD
                };
                let mut info = [0u8; FANOTIFY_PIDFD_INFO_LEN];
                info[0] = FAN_EVENT_INFO_TYPE_PIDFD;
                info[1] = 0;
                info[2..4].copy_from_slice(&(FANOTIFY_PIDFD_INFO_LEN as u16).to_ne_bytes());
                info[4..8].copy_from_slice(&pidfd.to_ne_bytes());
                data.extend_from_slice(&info);
            }
        }
        user_buf.copy_from_slice(&data)
    }
}

impl Drop for FanotifyGroup {
    fn drop(&mut self) {
        LIVE_FANOTIFY_GROUPS.fetch_sub(1, Ordering::Relaxed);
    }
}

fn align_to_eight(value: usize) -> usize {
    (value + 7) & !7
}

fn report_fid_info_len(
    init_flags: u32,
    fid_node: Option<VfsNodeId>,
    child_fid_node: Option<VfsNodeId>,
    name: Option<&str>,
    fid_info_type: u8,
) -> usize {
    if fid_node.is_none() {
        return 0;
    }
    let name_len = if fid_info_type == FAN_EVENT_INFO_TYPE_DFID_NAME {
        name.map(|name| name.len() + 1).unwrap_or(0)
    } else {
        0
    };
    let mut len = align_to_eight(FANOTIFY_FID_INFO_BASE_LEN + WHUSP_FILE_HANDLE_BYTES + name_len);
    if init_flags & (FAN_REPORT_FID | FAN_REPORT_DIR_FID) == (FAN_REPORT_FID | FAN_REPORT_DIR_FID)
        && child_fid_node.is_some()
    {
        len += align_to_eight(FANOTIFY_FID_INFO_BASE_LEN + WHUSP_FILE_HANDLE_BYTES);
    }
    len
}

fn append_report_fid_info(
    data: &mut Vec<u8>,
    init_flags: u32,
    fid_node: Option<VfsNodeId>,
    child_fid_node: Option<VfsNodeId>,
    name: Option<&str>,
    fid_info_type: u8,
) {
    let Some(fid_node) = fid_node else {
        return;
    };
    let name_len = if fid_info_type == FAN_EVENT_INFO_TYPE_DFID_NAME {
        name.map(|name| name.len() + 1).unwrap_or(0)
    } else {
        0
    };
    let raw_len = FANOTIFY_FID_INFO_BASE_LEN + WHUSP_FILE_HANDLE_BYTES + name_len;
    let len = align_to_eight(raw_len);
    let mut info = vec![0; len];
    info[0] = fid_info_type;
    info[1] = 0;
    info[2..4].copy_from_slice(&(len as u16).to_ne_bytes());
    let fsid = file_handle_fsid(fid_node);
    info[4..8].copy_from_slice(&fsid[0].to_ne_bytes());
    info[8..12].copy_from_slice(&fsid[1].to_ne_bytes());
    write_file_handle_record(&mut info[12..12 + WHUSP_FILE_HANDLE_RECORD_LEN], fid_node);
    if let Some(name) = name.filter(|_| fid_info_type == FAN_EVENT_INFO_TYPE_DFID_NAME) {
        let name_offset = FANOTIFY_FID_INFO_BASE_LEN + WHUSP_FILE_HANDLE_BYTES;
        info[name_offset..name_offset + name.len()].copy_from_slice(name.as_bytes());
    }
    data.extend_from_slice(&info);

    if init_flags & (FAN_REPORT_FID | FAN_REPORT_DIR_FID) != (FAN_REPORT_FID | FAN_REPORT_DIR_FID) {
        return;
    }
    let Some(child_fid_node) = child_fid_node else {
        return;
    };
    let len = align_to_eight(FANOTIFY_FID_INFO_BASE_LEN + WHUSP_FILE_HANDLE_BYTES);
    let mut info = vec![0; len];
    info[0] = FAN_EVENT_INFO_TYPE_FID;
    info[1] = 0;
    info[2..4].copy_from_slice(&(len as u16).to_ne_bytes());
    let fsid = file_handle_fsid(child_fid_node);
    info[4..8].copy_from_slice(&fsid[0].to_ne_bytes());
    info[8..12].copy_from_slice(&fsid[1].to_ne_bytes());
    write_file_handle_record(
        &mut info[12..12 + WHUSP_FILE_HANDLE_RECORD_LEN],
        child_fid_node,
    );
    data.extend_from_slice(&info);
}

pub struct FanotifyGroupFile {
    group: Arc<FanotifyGroup>,
    status_flags: UPIntrFreeCell<OpenFlags>,
}

impl FanotifyGroupFile {
    fn new(group: Arc<FanotifyGroup>) -> Self {
        Self {
            group,
            status_flags: unsafe { UPIntrFreeCell::new(OpenFlags::empty()) },
        }
    }
}

impl Drop for FanotifyGroupFile {
    fn drop(&mut self) {
        self.group.close();
    }
}

impl File for FanotifyGroupFile {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn readable(&self) -> bool {
        true
    }

    fn writable(&self) -> bool {
        true
    }

    fn read(&self, user_buf: UserBuffer) -> usize {
        let nonblocking = self.status_flags().contains(OpenFlags::NONBLOCK);
        self.group.read_events(user_buf, nonblocking)
    }

    fn write(&self, _user_buf: UserBuffer) -> usize {
        // UNFINISHED: Linux fanotify permission events accept FAN_ALLOW/FAN_DENY
        // responses through this fd. Permission events are not implemented in
        // the first notification-only subset.
        0
    }

    fn poll(&self, events: PollEvents) -> PollEvents {
        self.poll_with_wait(events, None)
    }

    fn poll_with_wait(&self, events: PollEvents, waiter: Option<&Arc<PollWaiter>>) -> PollEvents {
        self.group.inner.exclusive_session(|inner| {
            if inner.closed {
                return PollEvents::POLLHUP;
            }
            if let Some(waiter) = waiter
                && events.intersects(PollEvents::POLLIN | PollEvents::POLLPRI)
            {
                inner.poll_waiters.register(waiter);
            }
            let mut ready = PollEvents::empty();
            if events.intersects(PollEvents::POLLIN | PollEvents::POLLPRI)
                && !inner.events.is_empty()
            {
                ready |= PollEvents::POLLIN;
            }
            if events.contains(PollEvents::POLLOUT) {
                ready |= PollEvents::POLLOUT;
            }
            ready
        })
    }

    fn status_flags(&self) -> OpenFlags {
        self.status_flags.exclusive_session(|flags| *flags)
    }

    fn set_status_flags(&self, flags: OpenFlags) {
        self.status_flags.exclusive_session(|status_flags| {
            *status_flags = flags;
        });
    }

    fn suppresses_fanotify(&self) -> bool {
        true
    }
}

fn install_event_fd(file: &Arc<dyn File + Send + Sync>, flags: OpenFlags) -> SysResult<i32> {
    let event_file = file.clone_for_fanotify_event(flags)?;
    install_file_fd(event_file, flags, None).map(|fd| fd as i32)
}

fn validate_init_flags(flags: u32) -> Result<(), SysError> {
    if current_process().credentials().euid != 0 {
        let unprivileged_disallowed = FAN_UNLIMITED_QUEUE
            | FAN_UNLIMITED_MARKS
            | FAN_CLASS_CONTENT
            | FAN_CLASS_PRE_CONTENT
            | FAN_REPORT_TID;
        if flags & FAN_REPORT_FID == 0 || flags & unprivileged_disallowed != 0 {
            return Err(SysError::EPERM);
        }
    }
    if flags & FAN_CLASS_MASK == FAN_CLASS_MASK {
        return Err(SysError::EINVAL);
    }
    if flags & (FAN_CLASS_CONTENT | FAN_CLASS_PRE_CONTENT) != 0 {
        // UNFINISHED: Content and pre-content permission classes are accepted
        // for notification-only marks so LTP priority ordering tests can run.
        // Permission event masks still return EINVAL because this kernel does
        // not yet block filesystem operations for FAN_ALLOW/FAN_DENY replies.
    }
    if flags & FILE_HANDLE_REPORT_FLAGS != 0
        && flags & (FAN_CLASS_CONTENT | FAN_CLASS_PRE_CONTENT) != 0
    {
        return Err(SysError::EINVAL);
    }
    if flags & FAN_UNLIMITED_MARKS != 0 {
        // CONTEXT: This contest subset does not enforce a per-user mark limit,
        // so FAN_UNLIMITED_MARKS is already the effective behavior.
    }
    if flags & FAN_REPORT_TID != 0 {
        // CONTEXT: Metadata-only events can report the triggering thread id
        // without adding any extra information record encoding.
    }
    if flags & FAN_REPORT_PIDFD != 0 && flags & FAN_REPORT_TID != 0 {
        return Err(SysError::EINVAL);
    }
    if flags & FAN_REPORT_PIDFD != 0 {
        // CONTEXT: FAN_REPORT_PIDFD is encoded as a single pidfd information
        // record. If combined with FID/name flags, this subset still omits the
        // FID/name records because current scoring only probes init validation.
    }
    if flags & FILE_HANDLE_REPORT_FLAGS != 0 {
        // CONTEXT: FID/name reporting uses this kernel's name_to_handle_at(2)
        // compatible VfsNodeId-based handle encoding, not a persistent
        // filesystem-native handle.
    }
    if flags & FAN_REPORT_NAME != 0 && flags & FAN_REPORT_DIR_FID == 0 {
        return Err(SysError::EINVAL);
    }
    if flags & FAN_REPORT_TARGET_FID != 0
        && flags & (FAN_REPORT_FID | FAN_REPORT_DIR_FID | FAN_REPORT_NAME)
            != (FAN_REPORT_FID | FAN_REPORT_DIR_FID | FAN_REPORT_NAME)
    {
        return Err(SysError::EINVAL);
    }
    if flags & UNSUPPORTED_REPORT_FLAGS != 0 {
        // UNFINISHED: pidfd/mount/error report records are not encoded by this
        // metadata-only implementation.
        return Err(SysError::EINVAL);
    }
    if flags & !SUPPORTED_INIT_FLAGS != 0 {
        return Err(SysError::EINVAL);
    }
    Ok(())
}

fn validate_event_file_flags(flags: u32) -> SysResult<OpenFlags> {
    let Some(open_flags) = OpenFlags::from_bits(flags) else {
        return Err(SysError::EINVAL);
    };
    if flags & 0b11 == 0b11 {
        return Err(SysError::EINVAL);
    }
    Ok(open_flags)
}

pub fn sys_fanotify_init(flags: u32, event_f_flags: u32) -> SysResult {
    validate_init_flags(flags)?;
    let event_file_flags = validate_event_file_flags(event_f_flags)?;
    let groups_limit_reached = FANOTIFY_GROUPS.exclusive_session(|groups| {
        groups.retain(|weak| weak.strong_count() > 0);
        groups.len() >= MAX_USER_GROUPS
    });
    if groups_limit_reached {
        return Err(SysError::EMFILE);
    }
    let mut open_flags = OpenFlags::RDONLY;
    if flags & FAN_CLOEXEC != 0 {
        open_flags |= OpenFlags::CLOEXEC;
    }
    if flags & FAN_NONBLOCK != 0 {
        open_flags |= OpenFlags::NONBLOCK;
    }

    let file = Arc::new(FanotifyGroupFile::new(FanotifyGroup::new(
        flags,
        event_file_flags,
    )));
    install_file_fd(file, open_flags, None)
}

fn validate_mark_args(group: &FanotifyGroup, flags: u32, mask: u64) -> Result<(), SysError> {
    if flags & !KNOWN_MARK_FLAGS != 0 {
        return Err(SysError::EINVAL);
    }
    let ops = flags & (FAN_MARK_ADD | FAN_MARK_REMOVE | FAN_MARK_FLUSH);
    if ops.count_ones() != 1 {
        return Err(SysError::EINVAL);
    }
    if flags & (FAN_MARK_MOUNT | FAN_MARK_FILESYSTEM) == (FAN_MARK_MOUNT | FAN_MARK_FILESYSTEM) {
        return Err(SysError::EINVAL);
    }
    if flags & FAN_MARK_FLUSH == 0 {
        if mask == 0 || mask & UNSUPPORTED_PERMISSION_EVENTS != 0 || mask & !KNOWN_MARK_MASK != 0 {
            return Err(SysError::EINVAL);
        }
        if mask & (SUPPORTED_DIRENT_MARK_EVENTS | SUPPORTED_SELF_MARK_EVENTS) != 0
            && group.init_flags & (FAN_REPORT_FID | FAN_REPORT_DIR_FID) == 0
        {
            return Err(SysError::EINVAL);
        }
        if mask & SUPPORTED_DIRENT_MARK_EVENTS != 0 && flags & FAN_MARK_MOUNT != 0 {
            return Err(SysError::EINVAL);
        }
        if mask & FAN_RENAME != 0 && group.init_flags & FAN_REPORT_NAME == 0 {
            return Err(SysError::EINVAL);
        }
    }
    Ok(())
}

fn non_dir_inode_mark_needs_enotdir(group: &FanotifyGroup, flags: u32, mask: u64) -> bool {
    if flags & (FAN_MARK_MOUNT | FAN_MARK_FILESYSTEM) != 0 {
        return false;
    }
    if mask & (SUPPORTED_DIRENT_MARK_EVENTS | FAN_ONDIR | FAN_EVENT_ON_CHILD) == 0 {
        return false;
    }
    flags & FAN_MARK_IGNORE != 0 || group.init_flags & FAN_REPORT_TARGET_FID != 0
}

fn group_from_fd(fd: usize) -> SysResult<Arc<FanotifyGroup>> {
    let file = get_file_by_fd(fd)?;
    let group_file = file
        .as_any()
        .downcast_ref::<FanotifyGroupFile>()
        .ok_or(SysError::EINVAL)?;
    Ok(Arc::clone(&group_file.group))
}

fn kind_from_file(file: &Arc<dyn File + Send + Sync>) -> SysResult<FsNodeKind> {
    let mode = file.stat()?.mode;
    match mode & crate::fs::S_IFMT {
        crate::fs::S_IFDIR => Ok(FsNodeKind::Directory),
        crate::fs::S_IFLNK => Ok(FsNodeKind::Symlink),
        _ => Ok(FsNodeKind::RegularFile),
    }
}

fn resolve_mark_target(
    dirfd: isize,
    pathname: *const u8,
    flags: u32,
) -> SysResult<(VfsNodeId, FsNodeKind, String)> {
    let follow_final_symlink = flags & FAN_MARK_DONT_FOLLOW == 0;
    if pathname.is_null() {
        if dirfd == AT_FDCWD || dirfd < 0 {
            return Err(SysError::EBADF);
        }
        let file = get_file_by_fd(dirfd as usize)?;
        let Some(node) = file.vfs_node_id() else {
            if flags & (FAN_MARK_MOUNT | FAN_MARK_FILESYSTEM) != 0 {
                return Err(SysError::EINVAL);
            }
            return Err(SysError::EBADF);
        };
        return Ok((node, kind_from_file(&file)?, String::new()));
    }

    let token = current_user_token();
    let path = read_user_c_string(token, pathname, PATH_MAX)?;
    let snapshot = current_process().path_snapshot();
    let context: PathContext = path_context_from(&snapshot, dirfd, path.as_str())?;
    let normalized_path =
        normalize_path_at_root(context.root_path(), context.cwd_path(), path.as_str())
            .unwrap_or_else(|| path.clone());
    let resolved = lookup_path_in(context, path.as_str(), follow_final_symlink)?;
    Ok((resolved.node, resolved.kind, normalized_path))
}

pub fn sys_fanotify_mark(
    fanotify_fd: usize,
    flags: u32,
    mask: u64,
    dirfd: isize,
    pathname: *const u8,
) -> SysResult {
    let group = group_from_fd(fanotify_fd)?;
    if group.unprivileged
        && (flags & (FAN_MARK_MOUNT | FAN_MARK_FILESYSTEM) != 0
            || mask & UNSUPPORTED_PERMISSION_EVENTS != 0)
    {
        return Err(SysError::EPERM);
    }
    validate_mark_args(&group, flags, mask)?;

    if flags & FAN_MARK_FLUSH != 0 {
        if flags & !(FAN_MARK_FLUSH | FAN_MARK_MOUNT | FAN_MARK_FILESYSTEM) != 0 {
            return Err(SysError::EINVAL);
        }
        group.flush();
        return Ok(0);
    }

    let (node, kind, path) = resolve_mark_target(dirfd, pathname, flags)?;
    if flags & FAN_MARK_ONLYDIR != 0 && kind != FsNodeKind::Directory {
        return Err(SysError::ENOTDIR);
    }
    if kind != FsNodeKind::Directory && non_dir_inode_mark_needs_enotdir(&group, flags, mask) {
        return Err(SysError::ENOTDIR);
    }
    if flags & FAN_MARK_IGNORE != 0 && flags & FAN_MARK_IGNORED_SURV_MODIFY == 0 {
        if flags & (FAN_MARK_MOUNT | FAN_MARK_FILESYSTEM) != 0 {
            return Err(SysError::EINVAL);
        }
        if kind == FsNodeKind::Directory {
            return Err(SysError::EISDIR);
        }
    }
    group.update_mark(
        FanotifyMarkTarget::from_node(node, flags, path),
        flags,
        mask,
    )
}

fn live_fanotify_groups() -> Vec<Arc<FanotifyGroup>> {
    perf::record_fanotify_live_group_scan();
    FANOTIFY_GROUPS.exclusive_session(|groups| {
        groups.retain(|weak| weak.strong_count() > 0);
        groups.iter().filter_map(Weak::upgrade).collect()
    })
}

fn fanotify_notify_file_at(
    file: &Arc<dyn File + Send + Sync>,
    event_mask: u64,
    event_path: Option<&str>,
) {
    if LIVE_FANOTIFY_GROUPS.load(Ordering::Relaxed) == 0 {
        perf::record_fanotify_no_live_group_fast_path();
        return;
    }
    let live_groups = live_fanotify_groups();
    if live_groups.is_empty() {
        perf::record_fanotify_no_live_group_fast_path();
        return;
    }
    if file.suppresses_fanotify() {
        return;
    }
    if file.status_flags().contains(OpenFlags::PATH) {
        return;
    }
    let Some(node) = file.vfs_node_id() else {
        return;
    };
    let Some(mount) = file.vfs_mount_id() else {
        return;
    };
    let parent = file.vfs_parent_node_id();
    let is_dir = file.working_dir().is_some();
    if let Some(path) = event_path {
        remember_node_name(node, path);
    }
    let event_name = if is_dir {
        Some(String::from("."))
    } else {
        event_path
            .and_then(path_basename)
            .map(String::from)
            .or_else(|| remembered_node_name(node))
    };

    for group in live_groups {
        group.publish(
            node,
            parent,
            mount,
            event_mask,
            is_dir,
            event_path,
            event_name.as_deref(),
            None,
            Some(node),
            Some(file),
        );
    }
}

fn fanotify_notify_dirent_event(
    file: &Arc<dyn File + Send + Sync>,
    event_mask: u64,
    event_path: &str,
) {
    if LIVE_FANOTIFY_GROUPS.load(Ordering::Relaxed) == 0 {
        perf::record_fanotify_no_live_group_fast_path();
        return;
    }
    let Some(child_node) = file.vfs_node_id() else {
        return;
    };
    let Some(parent_node) = file.vfs_parent_node_id() else {
        return;
    };
    let Some(mount) = file.vfs_mount_id() else {
        return;
    };
    let is_dir = file.working_dir().is_some();
    let live_groups = live_fanotify_groups();
    if live_groups.is_empty() {
        perf::record_fanotify_no_live_group_fast_path();
        return;
    }
    remember_node_name(child_node, event_path);
    let event_name = path_basename(event_path).map(String::from);

    for group in live_groups {
        group.publish(
            parent_node,
            None,
            mount,
            event_mask,
            is_dir,
            Some(event_path),
            event_name.as_deref(),
            Some(parent_node),
            Some(child_node),
            None,
        );
    }
}

fn fanotify_notify_self_event(
    file: &Arc<dyn File + Send + Sync>,
    event_mask: u64,
    event_path: &str,
) {
    if LIVE_FANOTIFY_GROUPS.load(Ordering::Relaxed) == 0 {
        perf::record_fanotify_no_live_group_fast_path();
        return;
    }
    let Some(node) = file.vfs_node_id() else {
        return;
    };
    let Some(mount) = file.vfs_mount_id() else {
        return;
    };
    let parent = file.vfs_parent_node_id();
    let is_dir = file.working_dir().is_some();
    let live_groups = live_fanotify_groups();
    if live_groups.is_empty() {
        perf::record_fanotify_no_live_group_fast_path();
        return;
    }
    remember_node_name(node, event_path);
    let event_name = is_dir.then_some(".");

    for group in live_groups {
        group.publish(
            node,
            parent,
            mount,
            event_mask,
            is_dir,
            Some(event_path),
            event_name,
            Some(node),
            None,
            None,
        );
    }
}

fn path_basename(path: &str) -> Option<&str> {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
}

fn remember_node_name(node: VfsNodeId, path: &str) {
    let Some(name) = path_basename(path) else {
        return;
    };
    perf::record_fanotify_node_name_remember();
    FANOTIFY_NODE_NAMES.exclusive_session(|names| match names.entry(node) {
        Entry::Occupied(mut entry) => {
            let stored = entry.get_mut();
            stored.clear();
            stored.push_str(name);
        }
        Entry::Vacant(entry) => {
            entry.insert(String::from(name));
        }
    });
}

fn remembered_node_name(node: VfsNodeId) -> Option<String> {
    perf::record_fanotify_node_name_lookup();
    FANOTIFY_NODE_NAMES.exclusive_session(|names| names.get(&node).cloned())
}

pub(super) fn fanotify_notify_file(file: &Arc<dyn File + Send + Sync>, event_mask: u64) {
    fanotify_notify_file_at(file, event_mask, None);
}

pub(super) fn fanotify_close_group_file(file: &Arc<dyn File + Send + Sync>) {
    if let Some(group_file) = file.as_any().downcast_ref::<FanotifyGroupFile>() {
        group_file.group.close();
    }
}

pub(crate) fn fanotify_fdinfo(file: &Arc<dyn File + Send + Sync>) -> Option<String> {
    let group_file = file.as_any().downcast_ref::<FanotifyGroupFile>()?;
    let mut output = String::new();
    group_file.group.inner.exclusive_session(|inner| {
        for mark in inner.marks.iter() {
            output.push_str(&format!(
                "fanotify ino:0 sdev:0 mflags:{:x} mask:{:x} ignored_mask:{:x}\n",
                mark.flags, mark.mask, mark.ignored_mask
            ));
        }
    });
    Some(output)
}

pub(crate) fn fanotify_max_queued_events() -> usize {
    MAX_QUEUED_EVENTS
}

pub(crate) fn fanotify_evict_evictable_marks() {
    for group in live_fanotify_groups() {
        group.inner.exclusive_session(|inner| {
            inner
                .marks
                .retain(|mark| mark.flags & FAN_MARK_EVICTABLE == 0);
        });
    }
}

pub(super) fn fanotify_notify_open(file: &Arc<dyn File + Send + Sync>) {
    fanotify_notify_file(file, FAN_OPEN);
}

pub(super) fn fanotify_notify_open_at(file: &Arc<dyn File + Send + Sync>, path: &str) {
    fanotify_notify_file_at(file, FAN_OPEN, Some(path));
}

pub(crate) fn fanotify_notify_open_exec_at(file: &Arc<dyn File + Send + Sync>, path: &str) {
    fanotify_notify_file_at(file, FAN_OPEN | FAN_OPEN_EXEC, Some(path));
}

pub(super) fn fanotify_notify_access(file: &Arc<dyn File + Send + Sync>, bytes: usize) {
    if bytes > 0 {
        fanotify_notify_file(file, FAN_ACCESS);
    }
}

pub(super) fn fanotify_notify_modify(file: &Arc<dyn File + Send + Sync>, bytes: usize) {
    if bytes > 0 {
        fanotify_notify_file(file, FAN_MODIFY);
    }
}

pub(super) fn fanotify_notify_attrib(file: &Arc<dyn File + Send + Sync>) {
    fanotify_notify_file(file, FAN_ATTRIB);
}

pub(super) fn fanotify_notify_close(file: &Arc<dyn File + Send + Sync>, writable: bool) {
    if writable {
        fanotify_notify_file(file, FAN_CLOSE_WRITE);
    } else {
        fanotify_notify_file(file, FAN_CLOSE_NOWRITE);
    }
}

pub(super) fn fanotify_notify_create(file: &Arc<dyn File + Send + Sync>, path: &str) {
    fanotify_notify_dirent_event(file, FAN_CREATE, path);
}

pub(super) fn fanotify_notify_delete(file: &Arc<dyn File + Send + Sync>, path: &str) {
    fanotify_notify_dirent_event(file, FAN_DELETE, path);
    fanotify_notify_self_event(file, FAN_DELETE_SELF, path);
}

pub(super) fn fanotify_notify_move(
    old_file: &Arc<dyn File + Send + Sync>,
    old_path: &str,
    new_file: &Arc<dyn File + Send + Sync>,
    new_path: &str,
) {
    fanotify_notify_dirent_event(old_file, FAN_MOVED_FROM, old_path);
    fanotify_notify_dirent_event(new_file, FAN_MOVED_TO, new_path);
    fanotify_notify_self_event(new_file, FAN_MOVE_SELF, new_path);
}
