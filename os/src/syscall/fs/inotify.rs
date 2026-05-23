use super::super::errno::{SysError, SysResult};
use super::super::user_ptr::{PATH_MAX, read_user_c_string};
use super::fd::{get_file_by_fd, install_file_fd};
use super::path::path_context_from;
use super::uapi::AT_FDCWD;
use crate::fs::{
    File, FileStat, FsError, FsNodeKind, MountId, OpenFlags, PathContext, PollEvents,
    PollWaitQueue, PollWaiter, S_IFIFO, VfsNodeId, lookup_path_in, overlay_real_node,
};
use crate::mm::UserBuffer;
use crate::sync::UPIntrFreeCell;
use crate::task::{
    TaskControlBlock, block_current_task_no_schedule, current_has_interrupting_signal,
    current_process, current_user_token, schedule, wakeup_task,
};
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicU32, Ordering};
use lazy_static::lazy_static;

const IN_ACCESS: u32 = 0x0000_0001;
const IN_MODIFY: u32 = 0x0000_0002;
const IN_ATTRIB: u32 = 0x0000_0004;
const IN_CLOSE_WRITE: u32 = 0x0000_0008;
const IN_CLOSE_NOWRITE: u32 = 0x0000_0010;
const IN_OPEN: u32 = 0x0000_0020;
const IN_MOVED_FROM: u32 = 0x0000_0040;
const IN_MOVED_TO: u32 = 0x0000_0080;
const IN_CREATE: u32 = 0x0000_0100;
const IN_DELETE: u32 = 0x0000_0200;
const IN_DELETE_SELF: u32 = 0x0000_0400;
const IN_MOVE_SELF: u32 = 0x0000_0800;
const IN_UNMOUNT: u32 = 0x0000_2000;
const IN_Q_OVERFLOW: u32 = 0x0000_4000;
const IN_IGNORED: u32 = 0x0000_8000;
const IN_ONLYDIR: u32 = 0x0100_0000;
const IN_DONT_FOLLOW: u32 = 0x0200_0000;
const IN_EXCL_UNLINK: u32 = 0x0400_0000;
const IN_MASK_CREATE: u32 = 0x1000_0000;
const IN_MASK_ADD: u32 = 0x2000_0000;
const IN_ISDIR: u32 = 0x4000_0000;
const IN_ONESHOT: u32 = 0x8000_0000;

const IN_CLOEXEC: u32 = OpenFlags::CLOEXEC.bits();
const IN_NONBLOCK: u32 = OpenFlags::NONBLOCK.bits();

const INOTIFY_EVENT_LEN: usize = 16;
pub(crate) const INOTIFY_MAX_QUEUED_EVENTS: usize = 64;
pub(crate) const INOTIFY_MAX_USER_INSTANCES: usize = 512;
pub(crate) const INOTIFY_MAX_USER_WATCHES: usize = 8192;

const WATCH_EVENTS: u32 = IN_ACCESS
    | IN_MODIFY
    | IN_ATTRIB
    | IN_CLOSE_WRITE
    | IN_CLOSE_NOWRITE
    | IN_OPEN
    | IN_MOVED_FROM
    | IN_MOVED_TO
    | IN_CREATE
    | IN_DELETE
    | IN_DELETE_SELF
    | IN_MOVE_SELF;
const GENERATED_EVENTS: u32 = WATCH_EVENTS | IN_UNMOUNT | IN_Q_OVERFLOW | IN_IGNORED;
const WATCH_FLAGS: u32 =
    IN_ONLYDIR | IN_DONT_FOLLOW | IN_EXCL_UNLINK | IN_MASK_CREATE | IN_MASK_ADD | IN_ONESHOT;
const VALID_WATCH_MASK: u32 = WATCH_EVENTS | GENERATED_EVENTS | WATCH_FLAGS | IN_ISDIR;

static NEXT_MOVE_COOKIE: AtomicU32 = AtomicU32::new(1);

lazy_static! {
    static ref INOTIFY_GROUPS: UPIntrFreeCell<Vec<Weak<InotifyGroup>>> =
        unsafe { UPIntrFreeCell::new(Vec::new()) };
    static ref INOTIFY_NODE_NAMES: UPIntrFreeCell<Vec<(VfsNodeId, String)>> =
        unsafe { UPIntrFreeCell::new(Vec::new()) };
    static ref INOTIFY_UNLINKED_NODES: UPIntrFreeCell<Vec<VfsNodeId>> =
        unsafe { UPIntrFreeCell::new(Vec::new()) };
}

#[derive(Clone)]
struct InotifyWatch {
    wd: i32,
    node: VfsNodeId,
    kind: FsNodeKind,
    mask: u32,
}

#[derive(Clone, Eq, PartialEq)]
struct InotifyEvent {
    wd: i32,
    mask: u32,
    cookie: u32,
    name: Option<String>,
}

struct InotifyInner {
    next_wd: i32,
    watches: Vec<InotifyWatch>,
    events: VecDeque<InotifyEvent>,
    overflow_queued: bool,
    read_waiters: VecDeque<Arc<TaskControlBlock>>,
    poll_waiters: PollWaitQueue,
}

struct InotifyGroup {
    inner: UPIntrFreeCell<InotifyInner>,
}

pub struct InotifyFile {
    group: Arc<InotifyGroup>,
    status_flags: UPIntrFreeCell<OpenFlags>,
}

impl InotifyEvent {
    fn new(wd: i32, mask: u32, cookie: u32, name: Option<&str>) -> Self {
        Self {
            wd,
            mask,
            cookie,
            name: name.map(String::from),
        }
    }

    fn name_len(&self) -> usize {
        self.name
            .as_ref()
            .map(|name| (name.len() + 1 + 3) & !3)
            .unwrap_or(0)
    }

    fn record_len(&self) -> usize {
        INOTIFY_EVENT_LEN + self.name_len()
    }

    fn write_to(&self, data: &mut Vec<u8>) {
        data.extend_from_slice(&self.wd.to_ne_bytes());
        data.extend_from_slice(&self.mask.to_ne_bytes());
        data.extend_from_slice(&self.cookie.to_ne_bytes());
        data.extend_from_slice(&(self.name_len() as u32).to_ne_bytes());
        if let Some(name) = &self.name {
            data.extend_from_slice(name.as_bytes());
            data.push(0);
            while data.len() % 4 != 0 {
                data.push(0);
            }
        }
    }
}

impl InotifyGroup {
    fn new() -> Arc<Self> {
        let group = Arc::new(Self {
            inner: unsafe {
                UPIntrFreeCell::new(InotifyInner {
                    next_wd: 1,
                    watches: Vec::new(),
                    events: VecDeque::new(),
                    overflow_queued: false,
                    read_waiters: VecDeque::new(),
                    poll_waiters: PollWaitQueue::new(),
                })
            },
        });
        INOTIFY_GROUPS.exclusive_session(|groups| groups.push(Arc::downgrade(&group)));
        group
    }

    fn add_watch(&self, node: VfsNodeId, kind: FsNodeKind, mask: u32) -> SysResult<i32> {
        self.inner.exclusive_session(|inner| {
            if let Some(watch) = inner.watches.iter_mut().find(|watch| watch.node == node) {
                if mask & IN_MASK_CREATE != 0 {
                    return Err(SysError::EEXIST);
                }
                if mask & IN_MASK_ADD != 0 {
                    watch.mask |= mask;
                } else {
                    watch.mask = mask;
                }
                watch.kind = kind;
                return Ok(watch.wd);
            }

            let wd = inner.next_wd;
            inner.next_wd = inner.next_wd.saturating_add(1).max(1);
            inner.watches.push(InotifyWatch {
                wd,
                node,
                kind,
                mask,
            });
            Ok(wd)
        })
    }

    fn remove_watch(&self, wd: i32, emit_ignored: bool) -> SysResult {
        let (waiters, poll_waiters) = self.inner.exclusive_session(|inner| {
            let Some(index) = inner.watches.iter().position(|watch| watch.wd == wd) else {
                return Err(SysError::EINVAL);
            };
            inner.watches.remove(index);
            if emit_ignored {
                enqueue_event_locked(inner, InotifyEvent::new(wd, IN_IGNORED, 0, None));
                Ok((
                    core::mem::take(&mut inner.read_waiters),
                    inner.poll_waiters.drain(),
                ))
            } else {
                Ok((VecDeque::new(), Vec::new()))
            }
        })?;
        wake_waiters(waiters);
        PollWaiter::wake_all(poll_waiters);
        Ok(0)
    }

    fn read_events(&self, mut user_buf: UserBuffer, nonblocking: bool) -> usize {
        let capacity = user_buf.len();
        if capacity < INOTIFY_EVENT_LEN {
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
            let Some(event) = self.inner.exclusive_session(|inner| {
                let event = inner.events.front()?;
                if capacity.saturating_sub(data.len()) < event.record_len() {
                    return None;
                }
                let event = inner.events.pop_front()?;
                if event.mask == IN_Q_OVERFLOW {
                    inner.overflow_queued = false;
                }
                Some(event)
            }) else {
                break;
            };
            event.write_to(&mut data);
        }
        user_buf.copy_from_slice(data.as_slice())
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "inotify publication keeps watch target and event fields explicit"
    )]
    fn publish(
        &self,
        node: VfsNodeId,
        parent: Option<VfsNodeId>,
        event_mask: u32,
        is_dir: bool,
        cookie: u32,
        name: Option<&str>,
        include_direct: bool,
    ) {
        let (waiters, poll_waiters) = self.inner.exclusive_session(|inner| {
            let mut emitted = false;
            let real_node = overlay_real_node(node);
            let real_parent = parent.and_then(overlay_real_node);
            let unlinked = node_is_unlinked(node) || real_node.is_some_and(node_is_unlinked);

            let mut to_remove = Vec::new();
            let watches = inner.watches.clone();
            for watch in watches {
                let direct = include_direct
                    && (watch.node == node || real_node.is_some_and(|real| watch.node == real));
                let child = parent.is_some_and(|parent| watch.node == parent)
                    || real_parent.is_some_and(|parent| watch.node == parent);
                if !direct && !child {
                    continue;
                }
                if child && watch.mask & IN_EXCL_UNLINK != 0 && unlinked {
                    continue;
                }
                if watch.mask & event_mask == 0 {
                    continue;
                }

                let event_name = if child { name } else { None };
                let mut out_mask = event_mask;
                if is_dir && event_reports_isdir(event_mask) {
                    out_mask |= IN_ISDIR;
                }
                if enqueue_event_locked(
                    inner,
                    InotifyEvent::new(watch.wd, out_mask, cookie, event_name),
                ) {
                    emitted = true;
                }
                if watch.mask & IN_ONESHOT != 0 {
                    to_remove.push(watch.wd);
                }
            }

            for wd in to_remove {
                if remove_watch_locked(inner, wd) {
                    emitted = true;
                }
            }
            if emitted {
                (
                    core::mem::take(&mut inner.read_waiters),
                    inner.poll_waiters.drain(),
                )
            } else {
                (VecDeque::new(), Vec::new())
            }
        });
        wake_waiters(waiters);
        PollWaiter::wake_all(poll_waiters);
    }

    fn remove_matching_watches(&self, node: VfsNodeId, emit_unmount: bool) {
        let (waiters, poll_waiters) = self.inner.exclusive_session(|inner| {
            let real_node = overlay_real_node(node);
            let targets: Vec<_> = inner
                .watches
                .iter()
                .filter(|watch| {
                    watch.node == node || real_node.is_some_and(|real| watch.node == real)
                })
                .map(|watch| watch.wd)
                .collect();
            let mut emitted = false;
            for wd in targets {
                if emit_unmount {
                    enqueue_event_locked(inner, InotifyEvent::new(wd, IN_UNMOUNT, 0, None));
                    emitted = true;
                }
                if remove_watch_locked(inner, wd) {
                    emitted = true;
                }
            }
            if emitted {
                (
                    core::mem::take(&mut inner.read_waiters),
                    inner.poll_waiters.drain(),
                )
            } else {
                (VecDeque::new(), Vec::new())
            }
        });
        wake_waiters(waiters);
        PollWaiter::wake_all(poll_waiters);
    }

    fn remove_watches_on_mount(&self, mount: MountId) {
        let (waiters, poll_waiters) = self.inner.exclusive_session(|inner| {
            let targets: Vec<_> = inner
                .watches
                .iter()
                .filter(|watch| watch.node.mount_id == mount)
                .map(|watch| watch.wd)
                .collect();
            let mut emitted = false;
            for wd in targets {
                enqueue_event_locked(inner, InotifyEvent::new(wd, IN_UNMOUNT, 0, None));
                emitted = true;
                if remove_watch_locked(inner, wd) {
                    emitted = true;
                }
            }
            if emitted {
                (
                    core::mem::take(&mut inner.read_waiters),
                    inner.poll_waiters.drain(),
                )
            } else {
                (VecDeque::new(), Vec::new())
            }
        });
        wake_waiters(waiters);
        PollWaiter::wake_all(poll_waiters);
    }

    fn fdinfo(&self) -> String {
        self.inner.exclusive_session(|inner| {
            let mut output = String::new();
            for watch in inner.watches.iter() {
                output.push_str(
                    format!(
                        "inotify wd:{} ino:{:x} sdev:{:x} mask:{:x}\n",
                        watch.wd, watch.node.ino, watch.node.mount_id.0, watch.mask
                    )
                    .as_str(),
                );
            }
            output
        })
    }
}

impl InotifyFile {
    fn new(group: Arc<InotifyGroup>) -> Self {
        Self {
            group,
            status_flags: unsafe { UPIntrFreeCell::new(OpenFlags::empty()) },
        }
    }
}

impl File for InotifyFile {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn readable(&self) -> bool {
        true
    }

    fn writable(&self) -> bool {
        false
    }

    fn read(&self, user_buf: UserBuffer) -> usize {
        let nonblocking = self.status_flags().contains(OpenFlags::NONBLOCK);
        self.group.read_events(user_buf, nonblocking)
    }

    fn write(&self, _buf: UserBuffer) -> usize {
        0
    }

    fn poll(&self, events: PollEvents) -> PollEvents {
        self.poll_with_wait(events, None)
    }

    fn poll_with_wait(&self, events: PollEvents, waiter: Option<&Arc<PollWaiter>>) -> PollEvents {
        self.group.inner.exclusive_session(|inner| {
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
            ready
        })
    }

    fn stat(&self) -> Result<FileStat, FsError> {
        Ok(FileStat::with_mode(S_IFIFO | 0o600))
    }

    fn check_read(&self, len: usize) -> Result<(), FsError> {
        if len < INOTIFY_EVENT_LEN {
            Err(FsError::InvalidInput)
        } else {
            Ok(())
        }
    }

    fn status_flags(&self) -> OpenFlags {
        self.status_flags.exclusive_session(|flags| *flags)
    }

    fn set_status_flags(&self, flags: OpenFlags) {
        self.status_flags
            .exclusive_session(|status_flags| *status_flags = flags);
    }
}

fn enqueue_event_locked(inner: &mut InotifyInner, event: InotifyEvent) -> bool {
    if event.mask != IN_Q_OVERFLOW && inner.events.back().is_some_and(|last| *last == event) {
        return true;
    }
    if event.mask != IN_Q_OVERFLOW && inner.events.len() >= INOTIFY_MAX_QUEUED_EVENTS {
        if !inner.overflow_queued {
            inner
                .events
                .push_back(InotifyEvent::new(-1, IN_Q_OVERFLOW, 0, None));
            inner.overflow_queued = true;
            return true;
        }
        return false;
    }
    inner.events.push_back(event);
    true
}

fn remove_watch_locked(inner: &mut InotifyInner, wd: i32) -> bool {
    let Some(index) = inner.watches.iter().position(|watch| watch.wd == wd) else {
        return false;
    };
    inner.watches.remove(index);
    enqueue_event_locked(inner, InotifyEvent::new(wd, IN_IGNORED, 0, None))
}

fn wake_waiters(waiters: VecDeque<Arc<TaskControlBlock>>) {
    for task in waiters {
        let _ = wakeup_task(task);
    }
}

fn node_is_unlinked(node: VfsNodeId) -> bool {
    INOTIFY_UNLINKED_NODES.exclusive_session(|nodes| nodes.contains(&node))
}

fn mark_node_unlinked(node: VfsNodeId) {
    INOTIFY_UNLINKED_NODES.exclusive_session(|nodes| {
        if !nodes.contains(&node) {
            nodes.push(node);
        }
    });
}

fn clear_node_unlinked(node: VfsNodeId) {
    INOTIFY_UNLINKED_NODES.exclusive_session(|nodes| nodes.retain(|stored| *stored != node));
}

fn event_reports_isdir(event_mask: u32) -> bool {
    event_mask & (IN_DELETE_SELF | IN_MOVE_SELF) == 0
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
    INOTIFY_NODE_NAMES.exclusive_session(|names| {
        if let Some((_, stored)) = names
            .iter_mut()
            .find(|(stored_node, _)| *stored_node == node)
        {
            stored.clear();
            stored.push_str(name);
        } else {
            names.push((node, String::from(name)));
        }
    });
}

fn remembered_node_name(node: VfsNodeId) -> Option<String> {
    INOTIFY_NODE_NAMES.exclusive_session(|names| {
        names
            .iter()
            .find(|(stored_node, _)| *stored_node == node)
            .map(|(_, name)| name.clone())
    })
}

fn next_move_cookie() -> u32 {
    let cookie = NEXT_MOVE_COOKIE.fetch_add(1, Ordering::Relaxed);
    if cookie == 0 { 1 } else { cookie }
}

fn lookup_watch_target(
    context: PathContext,
    path: &str,
    mask: u32,
) -> SysResult<(VfsNodeId, FsNodeKind)> {
    let follow_final_symlink = mask & IN_DONT_FOLLOW == 0;
    let target = lookup_path_in(context, path, follow_final_symlink)?;
    if mask & IN_ONLYDIR != 0 && target.kind != FsNodeKind::Directory {
        return Err(SysError::ENOTDIR);
    }
    Ok((target.node, target.kind))
}

fn group_from_fd(fd: usize) -> SysResult<Arc<InotifyGroup>> {
    let file = get_file_by_fd(fd)?;
    file.as_any()
        .downcast_ref::<InotifyFile>()
        .map(|file| Arc::clone(&file.group))
        .ok_or(SysError::EINVAL)
}

fn publish_file_event(
    file: &Arc<dyn File + Send + Sync>,
    event_mask: u32,
    event_path: Option<&str>,
) {
    let Some(node) = file.vfs_node_id() else {
        return;
    };
    let parent = file.vfs_parent_node_id();
    let is_dir = file.working_dir().is_some();
    if let Some(path) = event_path {
        remember_node_name(node, path);
    }
    let name = event_path
        .and_then(path_basename)
        .map(String::from)
        .or_else(|| remembered_node_name(node));

    INOTIFY_GROUPS.exclusive_session(|groups| {
        groups.retain(|weak| weak.strong_count() > 0);
        let live_groups: Vec<_> = groups.iter().filter_map(Weak::upgrade).collect();
        for group in live_groups {
            group.publish(node, parent, event_mask, is_dir, 0, name.as_deref(), true);
        }
    });
}

fn publish_child_event(
    file: &Arc<dyn File + Send + Sync>,
    event_mask: u32,
    path: &str,
    cookie: u32,
) {
    let Some(node) = file.vfs_node_id() else {
        return;
    };
    let Some(parent) = file.vfs_parent_node_id() else {
        return;
    };
    let is_dir = file.working_dir().is_some();
    remember_node_name(node, path);
    let name = path_basename(path).map(String::from);

    INOTIFY_GROUPS.exclusive_session(|groups| {
        groups.retain(|weak| weak.strong_count() > 0);
        let live_groups: Vec<_> = groups.iter().filter_map(Weak::upgrade).collect();
        for group in live_groups {
            group.publish(
                node,
                Some(parent),
                event_mask,
                is_dir,
                cookie,
                name.as_deref(),
                false,
            );
        }
    });
}

fn publish_self_event(file: &Arc<dyn File + Send + Sync>, event_mask: u32) {
    let Some(node) = file.vfs_node_id() else {
        return;
    };
    let is_dir = file.working_dir().is_some();

    INOTIFY_GROUPS.exclusive_session(|groups| {
        groups.retain(|weak| weak.strong_count() > 0);
        let live_groups: Vec<_> = groups.iter().filter_map(Weak::upgrade).collect();
        for group in live_groups {
            group.publish(node, None, event_mask, is_dir, 0, None, true);
        }
    });
}

pub(super) fn inotify_notify_open(file: &Arc<dyn File + Send + Sync>) {
    if file.status_flags().contains(OpenFlags::PATH) {
        return;
    }
    publish_file_event(file, IN_OPEN, None);
}

pub(super) fn inotify_notify_open_at(file: &Arc<dyn File + Send + Sync>, path: &str) {
    if file.status_flags().contains(OpenFlags::PATH) {
        return;
    }
    publish_file_event(file, IN_OPEN, Some(path));
}

pub(super) fn inotify_notify_access(file: &Arc<dyn File + Send + Sync>, bytes: usize) {
    if bytes > 0 {
        publish_file_event(file, IN_ACCESS, None);
    }
}

pub(super) fn inotify_notify_modify(file: &Arc<dyn File + Send + Sync>, bytes: usize) {
    if bytes > 0 {
        publish_file_event(file, IN_MODIFY, None);
    }
}

pub(super) fn inotify_notify_attrib(file: &Arc<dyn File + Send + Sync>) {
    publish_file_event(file, IN_ATTRIB, None);
}

pub(super) fn inotify_notify_close(file: &Arc<dyn File + Send + Sync>, writable: bool) {
    if file.status_flags().contains(OpenFlags::PATH) {
        return;
    }
    publish_file_event(
        file,
        if writable {
            IN_CLOSE_WRITE
        } else {
            IN_CLOSE_NOWRITE
        },
        None,
    );
}

pub(super) fn inotify_notify_create(file: &Arc<dyn File + Send + Sync>, path: &str) {
    if let Some(node) = file.vfs_node_id() {
        clear_node_unlinked(node);
    }
    publish_child_event(file, IN_CREATE, path, 0);
}

pub(super) fn inotify_notify_delete(file: &Arc<dyn File + Send + Sync>, path: &str) {
    if let Some(node) = file.vfs_node_id() {
        mark_node_unlinked(node);
    }
    publish_child_event(file, IN_DELETE, path, 0);
    if file.working_dir().is_none() {
        publish_self_event(file, IN_ATTRIB);
    }
    publish_self_event(file, IN_DELETE_SELF);
    remove_matching_watches(file, false);
}

pub(super) fn inotify_notify_move(
    old_file: &Arc<dyn File + Send + Sync>,
    old_path: &str,
    new_file: &Arc<dyn File + Send + Sync>,
    new_path: &str,
) {
    let cookie = next_move_cookie();
    publish_child_event(old_file, IN_MOVED_FROM, old_path, cookie);
    publish_child_event(new_file, IN_MOVED_TO, new_path, cookie);
    publish_self_event(old_file, IN_MOVE_SELF);
    publish_self_event(new_file, IN_MOVE_SELF);
}

pub(crate) fn inotify_notify_unmount(mount: MountId) {
    INOTIFY_GROUPS.exclusive_session(|groups| {
        groups.retain(|weak| weak.strong_count() > 0);
        let live_groups: Vec<_> = groups.iter().filter_map(Weak::upgrade).collect();
        for group in live_groups {
            group.remove_watches_on_mount(mount);
        }
    });
}

fn remove_matching_watches(file: &Arc<dyn File + Send + Sync>, emit_unmount: bool) {
    let Some(node) = file.vfs_node_id() else {
        return;
    };
    INOTIFY_GROUPS.exclusive_session(|groups| {
        groups.retain(|weak| weak.strong_count() > 0);
        let live_groups: Vec<_> = groups.iter().filter_map(Weak::upgrade).collect();
        for group in live_groups {
            group.remove_matching_watches(node, emit_unmount);
        }
    });
}

pub(crate) fn inotify_fdinfo(file: &Arc<dyn File + Send + Sync>) -> Option<String> {
    file.as_any()
        .downcast_ref::<InotifyFile>()
        .map(|file| file.group.fdinfo())
}

pub fn sys_inotify_init1(flags: u32) -> SysResult {
    if flags & !(IN_CLOEXEC | IN_NONBLOCK) != 0 {
        return Err(SysError::EINVAL);
    }
    let mut open_flags = OpenFlags::RDONLY;
    if flags & IN_CLOEXEC != 0 {
        open_flags |= OpenFlags::CLOEXEC;
    }
    if flags & IN_NONBLOCK != 0 {
        open_flags |= OpenFlags::NONBLOCK;
    }
    install_file_fd(
        Arc::new(InotifyFile::new(InotifyGroup::new())),
        open_flags,
        None,
    )
}

pub fn sys_inotify_add_watch(fd: usize, pathname: *const u8, mask: u32) -> SysResult {
    if pathname.is_null() {
        return Err(SysError::EFAULT);
    }
    if mask == 0 || mask & !VALID_WATCH_MASK != 0 {
        return Err(SysError::EINVAL);
    }
    let token = current_user_token();
    let path = read_user_c_string(token, pathname, PATH_MAX)?;
    if path.is_empty() {
        return Err(SysError::ENOENT);
    }
    let group = group_from_fd(fd)?;
    let snapshot = current_process().path_snapshot();
    let context = path_context_from(&snapshot, AT_FDCWD, path.as_str())?;
    let (node, kind) = lookup_watch_target(context, path.as_str(), mask)?;
    remember_node_name(node, path.as_str());
    group.add_watch(node, kind, mask).map(|wd| wd as isize)
}

pub fn sys_inotify_rm_watch(fd: usize, wd: i32) -> SysResult {
    let group = group_from_fd(fd)?;
    group.remove_watch(wd, true)
}
