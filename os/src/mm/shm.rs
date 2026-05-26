use super::{FrameTracker, MapPermission, PhysPageNum, frame_alloc};
use crate::config::PAGE_SIZE;
use crate::sync::UPIntrFreeCell;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
use lazy_static::*;

pub(crate) const IPC_PRIVATE: isize = 0;
pub(crate) const IPC_CREAT: i32 = 0o1000;
pub(crate) const IPC_EXCL: i32 = 0o2000;
pub(crate) const SHM_HUGETLB: i32 = 0o4000;
pub(crate) const IPC_RMID: i32 = 0;
pub(crate) const IPC_SET: i32 = 1;
pub(crate) const IPC_STAT: i32 = 2;
pub(crate) const IPC_INFO: i32 = 3;
pub(crate) const SHM_RDONLY: i32 = 0o10000;
pub(crate) const SHM_RND: i32 = 0o20000;
pub(crate) const SHM_EXEC: i32 = 0o100000;
pub(crate) const SHM_LOCK: i32 = 11;
pub(crate) const SHM_UNLOCK: i32 = 12;
pub(crate) const SHM_STAT: i32 = 13;
pub(crate) const SHM_INFO: i32 = 14;
pub(crate) const SHM_STAT_ANY: i32 = 15;

const SHM_MIN: usize = 1;
pub(crate) const SHM_MAX: usize = 16 * 1024 * 1024;
pub(crate) const SHMMNI: usize = 4096;
pub(crate) const SHMALL: usize = (SHM_MAX / PAGE_SIZE) * SHMMNI;
pub(crate) const SHM_DEST: u32 = 0o1000;
pub(crate) const SHM_LOCKED: u32 = 0o2000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ShmError {
    NotFound,
    Exists,
    Invalid,
    NoMem,
    NoSpace,
    AccessDenied,
    NotPermitted,
}

#[derive(Clone, Copy)]
pub(crate) struct ShmCreateContext {
    pub(crate) pid: usize,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
}

#[derive(Clone, Copy)]
pub(crate) struct ShmCaller<'a> {
    pub(crate) pid: usize,
    pub(crate) euid: u32,
    pub(crate) egid: u32,
    pub(crate) groups: &'a [u32],
    pub(crate) can_override_read: bool,
    pub(crate) can_override_owner: bool,
    pub(crate) can_lock_ipc: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ShmSetAttrs {
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) mode: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ShmSegmentStat {
    pub(crate) id: usize,
    pub(crate) key: isize,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) cuid: u32,
    pub(crate) cgid: u32,
    pub(crate) mode: u32,
    pub(crate) size: usize,
    pub(crate) atime: i64,
    pub(crate) dtime: i64,
    pub(crate) ctime: i64,
    pub(crate) cpid: i32,
    pub(crate) lpid: i32,
    pub(crate) nattch: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ShmUsageInfo {
    pub(crate) used_ids: usize,
    pub(crate) total_pages: usize,
    pub(crate) resident_pages: usize,
    pub(crate) swapped_pages: usize,
    pub(crate) highest_index: usize,
}

#[derive(Clone, Copy)]
pub(crate) struct ShmPageMapping {
    pub(crate) page_index: usize,
    pub(crate) ppn: PhysPageNum,
}

pub(crate) struct ShmAttach {
    pub(crate) len: usize,
    pub(crate) pages: Vec<ShmPageMapping>,
}

struct ShmSegment {
    key: isize,
    size: usize,
    aligned_len: usize,
    mode: u32,
    uid: u32,
    gid: u32,
    cuid: u32,
    cgid: u32,
    creator_pid: usize,
    last_pid: usize,
    attach_count: usize,
    marked_for_delete: bool,
    locked: bool,
    atime: i64,
    dtime: i64,
    ctime: i64,
    pages: Vec<FrameTracker>,
}

impl ShmSegment {
    fn new(
        key: isize,
        size: usize,
        aligned_len: usize,
        mode: u32,
        context: ShmCreateContext,
    ) -> Option<Self> {
        let page_count = aligned_len / PAGE_SIZE;
        let mut pages = Vec::with_capacity(page_count);
        for _ in 0..page_count {
            pages.push(frame_alloc()?);
        }
        let now = now_sec();
        Some(Self {
            key,
            size,
            aligned_len,
            mode,
            uid: context.uid,
            gid: context.gid,
            cuid: context.uid,
            cgid: context.gid,
            creator_pid: context.pid,
            last_pid: 0,
            attach_count: 0,
            marked_for_delete: false,
            locked: false,
            atime: 0,
            dtime: 0,
            ctime: now,
            pages,
        })
    }

    fn page_mappings(&self) -> Vec<ShmPageMapping> {
        self.pages
            .iter()
            .enumerate()
            .map(|(page_index, frame)| ShmPageMapping {
                page_index,
                ppn: frame.ppn,
            })
            .collect()
    }

    fn stat(&self, id: usize) -> ShmSegmentStat {
        let mut mode = self.mode & 0o777;
        if self.marked_for_delete {
            mode |= SHM_DEST;
        }
        if self.locked {
            mode |= SHM_LOCKED;
        }
        ShmSegmentStat {
            id,
            key: self.key,
            uid: self.uid,
            gid: self.gid,
            cuid: self.cuid,
            cgid: self.cgid,
            mode,
            size: self.size,
            atime: self.atime,
            dtime: self.dtime,
            ctime: self.ctime,
            cpid: pid_to_i32(self.creator_pid),
            lpid: pid_to_i32(self.last_pid),
            nattch: self.attach_count,
        }
    }

    fn can_read(&self, caller: &ShmCaller<'_>) -> bool {
        self.mode_allows(caller, 0o400, 0o040, 0o004) || caller.can_override_read
    }

    fn can_write(&self, caller: &ShmCaller<'_>) -> bool {
        self.mode_allows(caller, 0o200, 0o020, 0o002) || caller.can_override_read
    }

    fn mode_allows(
        &self,
        caller: &ShmCaller<'_>,
        owner_bit: u32,
        group_bit: u32,
        other_bit: u32,
    ) -> bool {
        if caller.euid == self.uid {
            return self.mode & owner_bit != 0;
        }
        if caller.egid == self.gid || caller.groups.contains(&self.gid) {
            return self.mode & group_bit != 0;
        }
        self.mode & other_bit != 0
    }

    fn is_owner_or_creator(&self, caller: &ShmCaller<'_>) -> bool {
        caller.euid == self.uid || caller.euid == self.cuid || caller.can_override_owner
    }

    fn page_count(&self) -> usize {
        self.aligned_len / PAGE_SIZE
    }
}

struct ShmManager {
    next_id: usize,
    segments: BTreeMap<usize, ShmSegment>,
    keyed_segments: BTreeMap<isize, usize>,
}

impl ShmManager {
    fn new() -> Self {
        Self {
            next_id: 1,
            segments: BTreeMap::new(),
            keyed_segments: BTreeMap::new(),
        }
    }

    fn alloc_id(&mut self) -> usize {
        if let Some(id) = requested_next_id() {
            if !self.segments.contains_key(&id) {
                return id;
            }
        }
        while self.segments.contains_key(&self.next_id) {
            self.next_id += 1;
        }
        let id = self.next_id;
        self.next_id += 1;
        while self.segments.contains_key(&self.next_id) {
            self.next_id += 1;
        }
        id
    }

    fn create_segment(
        &mut self,
        key: isize,
        size: usize,
        mode: u32,
        context: ShmCreateContext,
    ) -> Result<usize, ShmError> {
        if !(SHM_MIN..=current_shmmax()).contains(&size) {
            return Err(ShmError::Invalid);
        }
        if self.segments.len() >= current_shmmni() {
            return Err(ShmError::NoSpace);
        }
        let aligned_len = align_up(size).ok_or(ShmError::Invalid)?;
        let page_count = aligned_len / PAGE_SIZE;
        if self.usage_info().total_pages.saturating_add(page_count) > current_shmall() {
            return Err(ShmError::NoSpace);
        }
        let shmid = self.alloc_id();
        let segment =
            ShmSegment::new(key, size, aligned_len, mode, context).ok_or(ShmError::NoMem)?;
        self.segments.insert(shmid, segment);
        if key != IPC_PRIVATE {
            self.keyed_segments.insert(key, shmid);
        }
        reset_next_id();
        Ok(shmid)
    }

    fn get_or_create(
        &mut self,
        key: isize,
        size: usize,
        shmflg: i32,
        context: ShmCreateContext,
        caller: &ShmCaller<'_>,
    ) -> Result<usize, ShmError> {
        let mode = (shmflg & 0o777) as u32;
        let flags = shmflg & !0o777;
        // UNFINISHED: huge-page flags and Linux's full key lookup rules are
        // not modeled; the contest path uses ordinary pages.
        if flags & SHM_HUGETLB != 0 {
            return Err(ShmError::Invalid);
        }
        if key == IPC_PRIVATE {
            return self.create_segment(key, size, mode, context);
        }

        if let Some(shmid) = self.keyed_segments.get(&key).copied() {
            if flags & (IPC_CREAT | IPC_EXCL) == (IPC_CREAT | IPC_EXCL) {
                return Err(ShmError::Exists);
            }
            let segment = self.segments.get(&shmid).ok_or(ShmError::NotFound)?;
            if segment.size < size {
                return Err(ShmError::Invalid);
            }
            if !segment.can_read(caller) && !segment.can_write(caller) {
                return Err(ShmError::AccessDenied);
            }
            return Ok(shmid);
        }

        if flags & IPC_CREAT == 0 {
            return Err(ShmError::NotFound);
        }
        self.create_segment(key, size, mode, context)
    }

    fn attach(&mut self, shmid: usize, pid: usize) -> Result<ShmAttach, ShmError> {
        let segment = self.segments.get_mut(&shmid).ok_or(ShmError::Invalid)?;
        if segment.marked_for_delete {
            return Err(ShmError::Invalid);
        }
        segment.attach_count += 1;
        segment.last_pid = pid;
        segment.atime = now_sec();
        Ok(ShmAttach {
            len: segment.aligned_len,
            pages: segment.page_mappings(),
        })
    }

    fn retain_attached(&mut self, shmid: usize, pid: usize) -> bool {
        let Some(segment) = self.segments.get_mut(&shmid) else {
            return false;
        };
        segment.attach_count += 1;
        segment.last_pid = pid;
        true
    }

    fn page_mappings(&self, shmid: usize) -> Option<Vec<ShmPageMapping>> {
        self.segments.get(&shmid).map(ShmSegment::page_mappings)
    }

    fn detach(&mut self, shmid: usize, pid: usize) -> Result<(), ShmError> {
        let Some(segment) = self.segments.get_mut(&shmid) else {
            return Err(ShmError::Invalid);
        };
        segment.attach_count = segment.attach_count.saturating_sub(1);
        segment.last_pid = pid;
        segment.dtime = now_sec();
        if segment.attach_count == 0 && segment.marked_for_delete {
            let key = segment.key;
            self.segments.remove(&shmid);
            if key != IPC_PRIVATE {
                self.keyed_segments.remove(&key);
            }
        }
        Ok(())
    }

    fn mark_for_delete(&mut self, shmid: usize, caller: &ShmCaller<'_>) -> Result<(), ShmError> {
        let Some(segment) = self.segments.get_mut(&shmid) else {
            return Err(ShmError::Invalid);
        };
        if !segment.is_owner_or_creator(caller) {
            return Err(ShmError::NotPermitted);
        }
        segment.marked_for_delete = true;
        segment.last_pid = caller.pid;
        let key = segment.key;
        if key != IPC_PRIVATE {
            self.keyed_segments.remove(&key);
        }
        if segment.attach_count == 0 {
            self.segments.remove(&shmid);
        }
        Ok(())
    }

    fn stat_by_id(&self, shmid: usize, caller: &ShmCaller<'_>) -> Result<ShmSegmentStat, ShmError> {
        let segment = self.segments.get(&shmid).ok_or(ShmError::Invalid)?;
        if !segment.can_read(caller) {
            return Err(ShmError::AccessDenied);
        }
        Ok(segment.stat(shmid))
    }

    fn stat_by_index(
        &self,
        index: usize,
        caller: &ShmCaller<'_>,
        skip_permission: bool,
    ) -> Result<(usize, ShmSegmentStat), ShmError> {
        let segment = self.segments.get(&index).ok_or(ShmError::Invalid)?;
        if !skip_permission && !segment.can_read(caller) {
            return Err(ShmError::AccessDenied);
        }
        Ok((index, segment.stat(index)))
    }

    fn set_attrs(
        &mut self,
        shmid: usize,
        attrs: ShmSetAttrs,
        caller: &ShmCaller<'_>,
    ) -> Result<(), ShmError> {
        let Some(segment) = self.segments.get_mut(&shmid) else {
            return Err(ShmError::Invalid);
        };
        if !segment.is_owner_or_creator(caller) {
            return Err(ShmError::NotPermitted);
        }
        segment.uid = attrs.uid;
        segment.gid = attrs.gid;
        segment.mode = attrs.mode & 0o777;
        segment.ctime = now_sec();
        Ok(())
    }

    fn set_locked(
        &mut self,
        shmid: usize,
        locked: bool,
        caller: &ShmCaller<'_>,
    ) -> Result<(), ShmError> {
        let Some(segment) = self.segments.get_mut(&shmid) else {
            return Err(ShmError::Invalid);
        };
        if !(segment.is_owner_or_creator(caller) || caller.can_lock_ipc) {
            return Err(ShmError::NotPermitted);
        }
        segment.locked = locked;
        Ok(())
    }

    fn usage_info(&self) -> ShmUsageInfo {
        let highest_index = self.highest_index();
        let total_pages = self
            .segments
            .values()
            .map(ShmSegment::page_count)
            .sum::<usize>();
        ShmUsageInfo {
            used_ids: self.segments.len(),
            total_pages,
            resident_pages: total_pages,
            swapped_pages: 0,
            highest_index,
        }
    }

    fn highest_index(&self) -> usize {
        self.segments.keys().next_back().copied().unwrap_or(0)
    }

    fn proc_sysvipc_shm_content(&self) -> String {
        let mut output = String::from(
            "       key      shmid perms                  size  cpid  lpid nattch   uid   gid  cuid  cgid      atime      dtime      ctime   rss  swap\n",
        );
        for (&shmid, segment) in &self.segments {
            let stat = segment.stat(shmid);
            output.push_str(&format!(
                "{:10} {:10} {:5o} {:21} {:5} {:5} {:6} {:5} {:5} {:5} {:5} {:10} {:10} {:10} {:5} {:5}\n",
                stat.key,
                stat.id,
                stat.mode & 0o777,
                stat.size,
                stat.cpid,
                stat.lpid,
                stat.nattch,
                stat.uid,
                stat.gid,
                stat.cuid,
                stat.cgid,
                stat.atime,
                stat.dtime,
                stat.ctime,
                segment.aligned_len,
                0
            ));
        }
        output
    }
}

lazy_static! {
    static ref SHM_MANAGER: UPIntrFreeCell<ShmManager> =
        unsafe { UPIntrFreeCell::new(ShmManager::new()) };
}

static SHM_MAX_LIMIT: AtomicUsize = AtomicUsize::new(SHM_MAX);
static SHMMNI_LIMIT: AtomicUsize = AtomicUsize::new(SHMMNI);
static SHMALL_LIMIT: AtomicUsize = AtomicUsize::new(SHMALL);
static SHM_NEXT_ID: AtomicIsize = AtomicIsize::new(-1);

pub(crate) fn current_shmmax() -> usize {
    SHM_MAX_LIMIT.load(Ordering::Relaxed)
}

pub(crate) fn current_shmmni() -> usize {
    SHMMNI_LIMIT.load(Ordering::Relaxed)
}

pub(crate) fn current_shmall() -> usize {
    SHMALL_LIMIT.load(Ordering::Relaxed)
}

pub(crate) fn current_shm_next_id() -> isize {
    SHM_NEXT_ID.load(Ordering::Relaxed)
}

pub(crate) fn set_shmmax(value: usize) -> bool {
    if value < SHM_MIN {
        return false;
    }
    SHM_MAX_LIMIT.store(value, Ordering::Relaxed);
    true
}

pub(crate) fn set_shmmni(value: usize) -> bool {
    if value == 0 {
        return false;
    }
    SHMMNI_LIMIT.store(value, Ordering::Relaxed);
    true
}

pub(crate) fn set_shmall(value: usize) -> bool {
    SHMALL_LIMIT.store(value, Ordering::Relaxed);
    true
}

pub(crate) fn set_shm_next_id(value: isize) -> bool {
    if value < -1 {
        return false;
    }
    SHM_NEXT_ID.store(value, Ordering::Relaxed);
    true
}

fn requested_next_id() -> Option<usize> {
    SHM_NEXT_ID.load(Ordering::Relaxed).try_into().ok()
}

fn reset_next_id() {
    SHM_NEXT_ID.store(-1, Ordering::Relaxed);
}

pub(crate) fn shmget_segment(
    key: isize,
    size: usize,
    shmflg: i32,
    context: ShmCreateContext,
    caller: &ShmCaller<'_>,
) -> Result<usize, ShmError> {
    SHM_MANAGER
        .exclusive_access()
        .get_or_create(key, size, shmflg, context, caller)
}

pub(crate) fn attach_segment(shmid: usize, pid: usize) -> Result<ShmAttach, ShmError> {
    SHM_MANAGER.exclusive_access().attach(shmid, pid)
}

pub(crate) fn retain_attached_segment(shmid: usize, pid: usize) -> bool {
    SHM_MANAGER.exclusive_access().retain_attached(shmid, pid)
}

pub(crate) fn attached_segment_pages(shmid: usize) -> Option<Vec<ShmPageMapping>> {
    SHM_MANAGER.exclusive_access().page_mappings(shmid)
}

pub(crate) fn detach_segment(shmid: usize, pid: usize) -> Result<(), ShmError> {
    SHM_MANAGER.exclusive_access().detach(shmid, pid)
}

pub(crate) fn segment_remap_available(shmid: usize) -> Option<bool> {
    let manager = SHM_MANAGER.exclusive_access();
    let segment = manager.segments.get(&shmid)?;
    if segment.marked_for_delete {
        return Some(false);
    }
    if segment.key == IPC_PRIVATE {
        return Some(true);
    }
    Some(manager.keyed_segments.get(&segment.key).copied() == Some(shmid))
}

pub(crate) fn mark_segment_for_delete(
    shmid: usize,
    caller: &ShmCaller<'_>,
) -> Result<(), ShmError> {
    SHM_MANAGER
        .exclusive_access()
        .mark_for_delete(shmid, caller)
}

pub(crate) fn stat_segment(
    shmid: usize,
    caller: &ShmCaller<'_>,
) -> Result<ShmSegmentStat, ShmError> {
    SHM_MANAGER.exclusive_access().stat_by_id(shmid, caller)
}

pub(crate) fn stat_segment_by_index(
    index: usize,
    caller: &ShmCaller<'_>,
    skip_permission: bool,
) -> Result<(usize, ShmSegmentStat), ShmError> {
    SHM_MANAGER
        .exclusive_access()
        .stat_by_index(index, caller, skip_permission)
}

pub(crate) fn set_segment_attrs(
    shmid: usize,
    attrs: ShmSetAttrs,
    caller: &ShmCaller<'_>,
) -> Result<(), ShmError> {
    SHM_MANAGER
        .exclusive_access()
        .set_attrs(shmid, attrs, caller)
}

pub(crate) fn set_segment_locked(
    shmid: usize,
    locked: bool,
    caller: &ShmCaller<'_>,
) -> Result<(), ShmError> {
    SHM_MANAGER
        .exclusive_access()
        .set_locked(shmid, locked, caller)
}

pub(crate) fn usage_info() -> ShmUsageInfo {
    SHM_MANAGER.exclusive_access().usage_info()
}

pub(crate) fn highest_index() -> usize {
    SHM_MANAGER.exclusive_access().highest_index()
}

pub(crate) fn proc_sysvipc_shm_content() -> String {
    SHM_MANAGER.exclusive_access().proc_sysvipc_shm_content()
}

pub(crate) fn shm_permission_from_flags(shmflg: i32) -> Result<MapPermission, ShmError> {
    // UNFINISHED: SHM_RND address rounding, SHM_REMAP, SHM_LOCKED, and
    // permission checks are deferred; iozone attaches read/write at addr 0.
    let unsupported = shmflg & !(SHM_RDONLY | SHM_RND | SHM_EXEC);
    if unsupported != 0 {
        return Err(ShmError::Invalid);
    }
    let mut permission = MapPermission::U | MapPermission::R;
    if shmflg & SHM_RDONLY == 0 {
        permission |= MapPermission::W;
    }
    if shmflg & SHM_EXEC != 0 {
        permission |= MapPermission::X;
    }
    Ok(permission)
}

fn align_up(size: usize) -> Option<usize> {
    size.checked_add(PAGE_SIZE - 1)
        .map(|value| value & !(PAGE_SIZE - 1))
}

fn now_sec() -> i64 {
    (crate::timer::wall_time_nanos() / 1_000_000_000) as i64
}

fn pid_to_i32(pid: usize) -> i32 {
    pid.try_into().unwrap_or(i32::MAX)
}
