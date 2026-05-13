use super::super::errno::{SysError, SysResult};
use super::super::user_ptr::{PATH_MAX, read_user_c_string};
use super::fd::{get_file_by_fd, install_file_fd};
use super::path::path_context_from;
use super::uapi::AT_FDCWD;
use crate::fs::{File, FsNodeKind, OpenFlags, PathContext, PollEvents, VfsNodeId, lookup_path_in};
use crate::mm::UserBuffer;
use crate::sync::UPIntrFreeCell;
use crate::task::{
    TaskControlBlock, block_current_task_no_schedule, current_has_interrupting_signal,
    current_process, current_user_token, schedule, wakeup_task,
};
use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use lazy_static::lazy_static;

const FAN_ACCESS: u64 = 0x0000_0001;
const FAN_MODIFY: u64 = 0x0000_0002;
const FAN_CLOSE_WRITE: u64 = 0x0000_0008;
const FAN_CLOSE_NOWRITE: u64 = 0x0000_0010;
const FAN_OPEN: u64 = 0x0000_0020;
const FAN_EVENT_ON_CHILD: u64 = 0x0800_0000;
const FAN_ONDIR: u64 = 0x4000_0000;

const FAN_CLOEXEC: u32 = 0x0000_0001;
const FAN_NONBLOCK: u32 = 0x0000_0002;
const FAN_CLASS_CONTENT: u32 = 0x0000_0004;
const FAN_CLASS_PRE_CONTENT: u32 = 0x0000_0008;
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
const FAN_NOFD: i32 = -1;

const SUPPORTED_INIT_FLAGS: u32 = FAN_CLOEXEC | FAN_NONBLOCK;
const UNSUPPORTED_REPORT_FLAGS: u32 = FAN_REPORT_PIDFD
    | FAN_REPORT_TID
    | FAN_REPORT_FID
    | FAN_REPORT_DIR_FID
    | FAN_REPORT_NAME
    | FAN_REPORT_TARGET_FID
    | FAN_REPORT_FD_ERROR
    | FAN_REPORT_MNT;
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
const SUPPORTED_MARK_EVENTS: u64 =
    FAN_ACCESS | FAN_MODIFY | FAN_CLOSE_WRITE | FAN_CLOSE_NOWRITE | FAN_OPEN;
const SUPPORTED_MARK_MASK: u64 = SUPPORTED_MARK_EVENTS | FAN_EVENT_ON_CHILD | FAN_ONDIR;

lazy_static! {
    static ref FANOTIFY_GROUPS: UPIntrFreeCell<Vec<Weak<FanotifyGroup>>> =
        unsafe { UPIntrFreeCell::new(Vec::new()) };
}

#[derive(Clone)]
struct FanotifyMark {
    node: VfsNodeId,
    mask: u64,
    ignored_mask: u64,
    flags: u32,
}

#[derive(Clone)]
struct FanotifyEvent {
    mask: u64,
    pid: i32,
    source: Arc<dyn File + Send + Sync>,
}

struct FanotifyInner {
    marks: Vec<FanotifyMark>,
    events: VecDeque<FanotifyEvent>,
    read_waiters: VecDeque<Arc<TaskControlBlock>>,
}

struct FanotifyGroup {
    event_file_flags: OpenFlags,
    inner: UPIntrFreeCell<FanotifyInner>,
}

impl FanotifyGroup {
    fn new(event_file_flags: OpenFlags) -> Arc<Self> {
        let group = Arc::new(Self {
            event_file_flags,
            inner: unsafe {
                UPIntrFreeCell::new(FanotifyInner {
                    marks: Vec::new(),
                    events: VecDeque::new(),
                    read_waiters: VecDeque::new(),
                })
            },
        });
        FANOTIFY_GROUPS.exclusive_session(|groups| groups.push(Arc::downgrade(&group)));
        group
    }

    fn flush(&self) {
        self.inner.exclusive_session(|inner| inner.marks.clear());
    }

    fn update_mark(&self, node: VfsNodeId, flags: u32, mask: u64) -> SysResult {
        self.inner.exclusive_session(|inner| {
            let existing = inner.marks.iter_mut().find(|mark| mark.node == node);
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
                        node,
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
                        inner.marks.retain(|mark| mark.node != node);
                    }
                    Ok(0)
                }
                (FAN_MARK_REMOVE, None) => Err(SysError::ENOENT),
                _ => Err(SysError::EINVAL),
            }
        })
    }

    fn publish(
        &self,
        node: VfsNodeId,
        parent: Option<VfsNodeId>,
        event_mask: u64,
        is_dir: bool,
        pid: i32,
        source: &Arc<dyn File + Send + Sync>,
    ) {
        let read_waiters = self.inner.exclusive_session(|inner| {
            let mut emitted = 0u64;
            for mark in inner.marks.iter_mut() {
                let is_self = mark.node == node;
                let is_child = parent.is_some_and(|parent| parent == mark.node);
                if !is_self && !(is_child && mark.mask & FAN_EVENT_ON_CHILD != 0) {
                    continue;
                }
                if is_dir && mark.mask & FAN_ONDIR == 0 {
                    continue;
                }
                let matched = event_mask & mark.mask & SUPPORTED_MARK_EVENTS;
                if matched == 0 {
                    continue;
                }
                if matched & mark.ignored_mask != 0 {
                    if event_mask & FAN_MODIFY != 0
                        && mark.flags & FAN_MARK_IGNORED_SURV_MODIFY == 0
                    {
                        mark.ignored_mask = 0;
                    }
                    continue;
                }
                if event_mask & FAN_MODIFY != 0 && mark.flags & FAN_MARK_IGNORED_SURV_MODIFY == 0 {
                    mark.ignored_mask = 0;
                }
                emitted |= matched;
            }
            if emitted != 0 {
                inner.events.push_back(FanotifyEvent {
                    mask: emitted,
                    pid,
                    source: Arc::clone(source),
                });
                core::mem::take(&mut inner.read_waiters)
            } else {
                VecDeque::new()
            }
        });
        for task in read_waiters {
            let _ = wakeup_task(task);
        }
    }

    fn has_events(&self) -> bool {
        self.inner
            .exclusive_session(|inner| !inner.events.is_empty())
    }

    fn read_events(&self, mut user_buf: UserBuffer, nonblocking: bool) -> usize {
        let capacity = user_buf.len();
        if capacity < FANOTIFY_METADATA_LEN {
            return 0;
        }
        loop {
            let task_cx_ptr = self.inner.exclusive_session(|inner| {
                if !inner.events.is_empty() || nonblocking || current_has_interrupting_signal() {
                    None
                } else {
                    let (task, task_cx_ptr) = block_current_task_no_schedule();
                    inner.read_waiters.push_back(task);
                    Some(task_cx_ptr)
                }
            });
            let Some(task_cx_ptr) = task_cx_ptr else {
                break;
            };
            schedule(task_cx_ptr);
        }

        let mut data = Vec::new();
        loop {
            if capacity.saturating_sub(data.len()) < FANOTIFY_METADATA_LEN {
                break;
            }
            let Some(event) = self
                .inner
                .exclusive_session(|inner| inner.events.pop_front())
            else {
                break;
            };
            let fd = install_event_fd(&event.source, self.event_file_flags).unwrap_or(FAN_NOFD);
            let mut record = [0u8; FANOTIFY_METADATA_LEN];
            record[0..4].copy_from_slice(&(FANOTIFY_METADATA_LEN as u32).to_ne_bytes());
            record[4] = FANOTIFY_METADATA_VERSION;
            record[5] = 0;
            record[6..8].copy_from_slice(&(FANOTIFY_METADATA_LEN as u16).to_ne_bytes());
            record[8..16].copy_from_slice(&event.mask.to_ne_bytes());
            record[16..20].copy_from_slice(&fd.to_ne_bytes());
            record[20..24].copy_from_slice(&event.pid.to_ne_bytes());
            data.extend_from_slice(&record);
        }
        user_buf.copy_from_slice(&data)
    }
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
        let mut ready = PollEvents::empty();
        if events.intersects(PollEvents::POLLIN | PollEvents::POLLPRI) && self.group.has_events() {
            ready |= PollEvents::POLLIN;
        }
        if events.contains(PollEvents::POLLOUT) {
            ready |= PollEvents::POLLOUT;
        }
        ready
    }

    fn status_flags(&self) -> OpenFlags {
        self.status_flags.exclusive_session(|flags| *flags)
    }

    fn set_status_flags(&self, flags: OpenFlags) {
        self.status_flags.exclusive_session(|status_flags| {
            *status_flags = flags;
        });
    }
}

fn install_event_fd(file: &Arc<dyn File + Send + Sync>, flags: OpenFlags) -> SysResult<i32> {
    let event_file = file.clone_for_fanotify_event(flags)?;
    install_file_fd(event_file, flags, None).map(|fd| fd as i32)
}

fn validate_init_flags(flags: u32) -> Result<(), SysError> {
    if flags & (FAN_CLASS_CONTENT | FAN_CLASS_PRE_CONTENT) != 0 {
        // UNFINISHED: Content and pre-content permission classes require
        // blocking permission events and userspace FAN_ALLOW/FAN_DENY replies.
        return Err(SysError::EINVAL);
    }
    if flags & UNSUPPORTED_REPORT_FLAGS != 0 {
        // UNFINISHED: FID/name/pidfd/mount report records are not encoded by
        // this first metadata-only implementation.
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
    let mut open_flags = OpenFlags::RDONLY;
    if flags & FAN_CLOEXEC != 0 {
        open_flags |= OpenFlags::CLOEXEC;
    }
    if flags & FAN_NONBLOCK != 0 {
        open_flags |= OpenFlags::NONBLOCK;
    }

    let file = Arc::new(FanotifyGroupFile::new(FanotifyGroup::new(event_file_flags)));
    install_file_fd(file, open_flags, None)
}

fn validate_mark_args(flags: u32, mask: u64) -> Result<(), SysError> {
    if flags & !KNOWN_MARK_FLAGS != 0 {
        return Err(SysError::EINVAL);
    }
    let ops = flags & (FAN_MARK_ADD | FAN_MARK_REMOVE | FAN_MARK_FLUSH);
    if ops.count_ones() != 1 {
        return Err(SysError::EINVAL);
    }
    if flags & (FAN_MARK_MOUNT | FAN_MARK_FILESYSTEM) != 0 {
        // UNFINISHED: Mount and filesystem marks require mount-wide fanotify
        // registries. The first subset supports inode marks only.
        return Err(SysError::EINVAL);
    }
    if flags & FAN_MARK_FLUSH == 0 {
        if mask == 0 || mask & !SUPPORTED_MARK_MASK != 0 {
            return Err(SysError::EINVAL);
        }
    }
    Ok(())
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
) -> SysResult<(VfsNodeId, FsNodeKind)> {
    let follow_final_symlink = flags & FAN_MARK_DONT_FOLLOW == 0;
    if pathname.is_null() {
        if dirfd == AT_FDCWD || dirfd < 0 {
            return Err(SysError::EBADF);
        }
        let file = get_file_by_fd(dirfd as usize)?;
        let node = file.vfs_node_id().ok_or(SysError::EBADF)?;
        return Ok((node, kind_from_file(&file)?));
    }

    let token = current_user_token();
    let path = read_user_c_string(token, pathname, PATH_MAX)?;
    let snapshot = current_process().path_snapshot();
    let context: PathContext = path_context_from(&snapshot, dirfd, path.as_str())?;
    let resolved = lookup_path_in(context, path.as_str(), follow_final_symlink)?;
    Ok((resolved.node, resolved.kind))
}

pub fn sys_fanotify_mark(
    fanotify_fd: usize,
    flags: u32,
    mask: u64,
    dirfd: isize,
    pathname: *const u8,
) -> SysResult {
    validate_mark_args(flags, mask)?;
    let group = group_from_fd(fanotify_fd)?;

    if flags & FAN_MARK_FLUSH != 0 {
        if flags != FAN_MARK_FLUSH {
            return Err(SysError::EINVAL);
        }
        group.flush();
        return Ok(0);
    }

    let (node, kind) = resolve_mark_target(dirfd, pathname, flags)?;
    if flags & FAN_MARK_ONLYDIR != 0 && kind != FsNodeKind::Directory {
        return Err(SysError::ENOTDIR);
    }
    group.update_mark(node, flags, mask)
}

pub(super) fn fanotify_notify_file(file: &Arc<dyn File + Send + Sync>, event_mask: u64) {
    if file.suppresses_fanotify() {
        return;
    }
    let Some(node) = file.vfs_node_id() else {
        return;
    };
    let parent = file.vfs_parent_node_id();
    let is_dir = file.working_dir().is_some();
    let pid = current_process().getpid() as i32;

    FANOTIFY_GROUPS.exclusive_session(|groups| {
        groups.retain(|weak| weak.strong_count() > 0);
        let live_groups: Vec<_> = groups.iter().filter_map(Weak::upgrade).collect();
        for group in live_groups {
            group.publish(node, parent, event_mask, is_dir, pid, file);
        }
    });
}

pub(super) fn fanotify_notify_open(file: &Arc<dyn File + Send + Sync>) {
    fanotify_notify_file(file, FAN_OPEN);
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

pub(super) fn fanotify_notify_close(file: &Arc<dyn File + Send + Sync>, writable: bool) {
    if writable {
        fanotify_notify_file(file, FAN_CLOSE_WRITE);
    } else {
        fanotify_notify_file(file, FAN_CLOSE_NOWRITE);
    }
}
