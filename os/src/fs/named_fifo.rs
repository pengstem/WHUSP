use super::inode::OpenFlags;
use super::pipe::{Pipe, default_pipe_capacity_for_current_process, make_pipe};
use super::status_flags::StatusFlagsCell;
use super::vfs::{FsError, FsResult, VfsNodeId};
use super::{File, FileStat, PollEvents, S_IFIFO};
use crate::mm::UserBuffer;
use crate::sync::SleepMutex;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use lazy_static::lazy_static;

lazy_static! {
    static ref NAMED_FIFO_STATES: SleepMutex<BTreeMap<VfsNodeId, NamedFifoState>> =
        SleepMutex::new(BTreeMap::new());
}

struct NamedFifoState {
    read_end: Arc<Pipe>,
    write_end: Arc<Pipe>,
    readers: usize,
    writers: usize,
}

impl NamedFifoState {
    fn new() -> Self {
        let (read_end, write_end) = make_pipe(default_pipe_capacity_for_current_process());
        Self {
            read_end,
            write_end,
            readers: 0,
            writers: 0,
        }
    }
}

pub(crate) fn open_named_fifo(
    node: VfsNodeId,
    flags: OpenFlags,
) -> FsResult<Arc<dyn File + Send + Sync>> {
    let (readable, writable) = flags.read_write();
    let mut states = NAMED_FIFO_STATES.lock();
    let state = states.entry(node).or_insert_with(NamedFifoState::new);
    if writable && !readable && flags.contains(OpenFlags::NONBLOCK) && state.readers == 0 {
        return Err(FsError::NoDeviceOrAddress);
    }

    let read_end = if readable {
        state.readers += 1;
        Some(state.read_end.clone())
    } else {
        None
    };
    let write_end = if writable {
        state.writers += 1;
        Some(state.write_end.clone())
    } else {
        None
    };
    Ok(Arc::new(NamedFifoFile {
        node,
        read_end,
        write_end,
        readable,
        writable,
        status_flags: StatusFlagsCell::new(flags),
    }))
}

pub(crate) struct NamedFifoFile {
    node: VfsNodeId,
    read_end: Option<Arc<Pipe>>,
    write_end: Option<Arc<Pipe>>,
    readable: bool,
    writable: bool,
    status_flags: StatusFlagsCell,
}

impl NamedFifoFile {
    fn peer_readers_closed(&self) -> bool {
        if !self.writable {
            return false;
        }
        NAMED_FIFO_STATES
            .lock()
            .get(&self.node)
            .is_none_or(|state| state.readers == 0)
    }

    fn peer_writers_closed(&self) -> bool {
        if !self.readable {
            return false;
        }
        NAMED_FIFO_STATES
            .lock()
            .get(&self.node)
            .is_none_or(|state| state.writers == 0)
    }
}

impl File for NamedFifoFile {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn readable(&self) -> bool {
        self.readable
    }

    fn writable(&self) -> bool {
        self.writable
    }

    fn read(&self, buf: UserBuffer) -> usize {
        self.read_end.as_ref().map_or(0, |pipe| {
            pipe.read_with_status_flags(buf, self.status_flags.get())
        })
    }

    fn write(&self, buf: UserBuffer) -> usize {
        self.write_end.as_ref().map_or(0, |pipe| {
            pipe.write_with_status_flags(buf, self.status_flags.get())
        })
    }

    fn poll(&self, events: PollEvents) -> PollEvents {
        let mut ready = PollEvents::empty();
        if let Some(read_end) = &self.read_end {
            ready |= read_end.poll(events);
            if self.peer_writers_closed() {
                ready |= PollEvents::POLLHUP;
            }
        }
        if let Some(write_end) = &self.write_end {
            ready |= write_end.poll(events);
            if self.peer_readers_closed() {
                ready |= PollEvents::POLLOUT | PollEvents::POLLERR;
            }
        }
        ready
    }

    fn stat(&self) -> FsResult<FileStat> {
        Ok(FileStat::with_mode(S_IFIFO | 0o600))
    }

    fn status_flags(&self) -> OpenFlags {
        self.status_flags.get()
    }

    fn set_status_flags(&self, flags: OpenFlags) {
        self.status_flags.set(flags);
    }

    fn pipe_capacity(&self) -> Option<usize> {
        self.read_end
            .as_ref()
            .or(self.write_end.as_ref())
            .and_then(|pipe| pipe.pipe_capacity())
    }

    fn set_pipe_capacity(&self, capacity: usize) -> FsResult<usize> {
        self.read_end
            .as_ref()
            .or(self.write_end.as_ref())
            .ok_or(FsError::Unsupported)?
            .set_pipe_capacity(capacity)
    }

    fn pipe_occupied(&self) -> Option<usize> {
        self.read_end
            .as_ref()
            .or(self.write_end.as_ref())
            .and_then(|pipe| pipe.pipe_occupied())
    }

    fn pipe_readers_closed(&self) -> bool {
        self.peer_readers_closed()
    }

    fn is_pipe(&self) -> bool {
        true
    }

    fn vfs_node_id(&self) -> Option<VfsNodeId> {
        Some(self.node)
    }
}

impl Drop for NamedFifoFile {
    fn drop(&mut self) {
        let mut states = NAMED_FIFO_STATES.lock();
        let Some(state) = states.get_mut(&self.node) else {
            return;
        };
        if self.readable && state.readers > 0 {
            state.readers -= 1;
        }
        if self.writable && state.writers > 0 {
            state.writers -= 1;
        }
        if state.readers == 0 && state.writers == 0 {
            states.remove(&self.node);
        }
    }
}
