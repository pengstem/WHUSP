use super::*;
use crate::fs::SocketFileCapability;

impl SocketFileCapability for LocalSocket {}

impl File for LocalSocket {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn readable(&self) -> bool {
        true
    }

    fn writable(&self) -> bool {
        true
    }

    fn read(&self, buf: UserBuffer) -> usize {
        self.recv_bytes(buf, false)
            .map(|(len, _)| len)
            .unwrap_or_default()
    }

    fn write(&self, buf: UserBuffer) -> usize {
        let len = buf.len();
        match self.kind() {
            SocketKind::Stream => {
                let written = self.send_stream_user_buffer(buf, false).unwrap_or_default();
                perf::record_local_socket_write_user_buffer(len, 0, written);
                written
            }
            SocketKind::Datagram => {
                let data = buf.to_vec();
                perf::record_local_socket_write_user_buffer(len, data.len(), 0);
                self.send_datagram(data, None, false).unwrap_or_default()
            }
        }
    }
    fn socket_write_peer_closed(&self) -> bool {
        self.stream_write_peer_closed()
    }

    fn poll(&self, events: PollEvents) -> PollEvents {
        self.poll_with_wait(events, None)
    }

    fn poll_with_wait(&self, events: PollEvents, waiter: Option<&Arc<PollWaiter>>) -> PollEvents {
        let (kind, listening, readable, read_shutdown, peer_write_shutdown, write_shutdown, peer) = {
            let mut inner = self.inner.exclusive_access();
            if let Some(waiter) = waiter {
                if events
                    .intersects(PollEvents::POLLIN | PollEvents::POLLPRI | PollEvents::POLLRDHUP)
                {
                    inner.read_poll_waiters.register(waiter);
                }
                if events.contains(PollEvents::POLLOUT) {
                    inner.write_poll_waiters.register(waiter);
                }
            }
            let readable = match inner.kind {
                SocketKind::Stream if inner.listening => !inner.accept_queue.is_empty(),
                SocketKind::Stream => !inner.stream_rx.is_empty() || inner.peer_write_shutdown,
                SocketKind::Datagram => !inner.datagram_rx.is_empty(),
            };
            (
                inner.kind,
                inner.listening,
                readable,
                inner.read_shutdown,
                inner.peer_write_shutdown,
                inner.write_shutdown,
                inner.peer_socket.clone(),
            )
        };
        let mut ready = PollEvents::empty();
        if events.intersects(PollEvents::POLLIN | PollEvents::POLLPRI | PollEvents::POLLRDHUP) {
            if readable {
                ready |= PollEvents::POLLIN;
            }
            // CONTEXT: LTP epoll_wait05 expects a stream socket to become
            // RDHUP-ready after userspace shuts down its local read side.
            if read_shutdown {
                ready |= PollEvents::POLLRDHUP;
            }
            if peer_write_shutdown {
                ready |= PollEvents::POLLRDHUP | PollEvents::POLLHUP;
            }
        }
        if events.contains(PollEvents::POLLOUT) && !write_shutdown {
            match kind {
                SocketKind::Stream if !listening => {
                    if let Some(peer) = peer.as_ref().and_then(Weak::upgrade) {
                        let peer = peer.exclusive_access();
                        if peer.stream_rx.len() < (peer.rcvbuf as usize).max(1) {
                            ready |= PollEvents::POLLOUT;
                        }
                    }
                }
                SocketKind::Datagram => {
                    let writable = if let Some(peer) = peer.as_ref().and_then(Weak::upgrade) {
                        let peer = peer.exclusive_access();
                        peer.datagram_rx_bytes < (peer.rcvbuf as usize).max(1)
                    } else {
                        true
                    };
                    if writable {
                        ready |= PollEvents::POLLOUT;
                    }
                }
                _ => ready |= PollEvents::POLLOUT,
            }
        }
        ready
    }

    fn stat(&self) -> crate::fs::FsResult<FileStat> {
        Ok(FileStat::with_mode(S_IFIFO | 0o600))
    }

    fn status_flags(&self) -> OpenFlags {
        *self.status_flags.exclusive_access()
    }

    fn set_status_flags(&self, flags: OpenFlags) {
        *self.status_flags.exclusive_access() = flags;
    }
    fn as_socket(&self) -> Option<&dyn SocketFileCapability> {
        Some(self)
    }
}
