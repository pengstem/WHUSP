use crate::arch::interrupt;
use crate::fs::{File, PollEvents, PollWaiter};
use crate::perf;
use crate::task::{
    block_current_task_no_schedule, current_has_interrupting_signal, current_task,
    current_user_token, schedule,
};
use crate::timer::{add_timer, get_time_ms};
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::super::errno::{SysError, SysResult};
use super::super::time::relative_timeout_deadline_ms;
use super::super::uapi::LinuxTimeSpec;
use super::super::user_ptr::{read_user_array, write_user_array};
use super::fd::get_file_by_fd;
use super::uapi::{LinuxPollFd, PPOLL_MAX_NFDS};

const SELECT_MAX_NFDS: usize = 1024;
const FD_SET_WORD_BITS: usize = usize::BITS as usize;

type PollFile = Arc<dyn File + Send + Sync>;

#[derive(Clone)]
enum PollFileSlot {
    File(PollFile),
    Error(SysError),
}

struct ProcSleepGuard {
    task: Arc<crate::task::TaskControlBlock>,
}

impl ProcSleepGuard {
    fn new() -> SysResult<Self> {
        let task = current_task().ok_or(SysError::ESRCH)?;
        task.inner_exclusive_access().proc_sleeping = true;
        Ok(Self { task })
    }
}

impl Drop for ProcSleepGuard {
    fn drop(&mut self) {
        self.task.inner_exclusive_access().proc_sleeping = false;
    }
}

fn sleep_until_poll_event(waiter: &Arc<PollWaiter>, deadline_ms: Option<usize>) -> SysResult {
    if waiter.was_triggered() {
        return Ok(0);
    }
    let now_ms = get_time_ms();
    if deadline_ms.is_some_and(|deadline_ms| now_ms >= deadline_ms) {
        return Ok(0);
    }
    if current_has_interrupting_signal() {
        return Err(SysError::EINTR);
    }

    let _sleep_guard = ProcSleepGuard::new()?;
    let interrupts_enabled = interrupt::supervisor_interrupt_enabled();
    // CONTEXT: Registering the waiter and blocking must be atomic with respect
    // to IRQ wakeups. Recheck after disabling interrupts so a device/timer wake
    // cannot be lost between scan_pollfds() and schedule().
    interrupt::disable_supervisor_interrupt();
    if waiter.was_triggered() {
        if interrupts_enabled {
            interrupt::enable_supervisor_interrupt();
        }
        return Ok(0);
    }
    let (task, task_cx_ptr) = block_current_task_no_schedule();
    debug_assert!(waiter.task_matches(&task));
    if let Some(deadline_ms) = deadline_ms {
        add_timer(deadline_ms, task);
    }
    if interrupts_enabled {
        interrupt::enable_supervisor_interrupt();
    }
    schedule(task_cx_ptr);
    Ok(0)
}

fn block_signal_only_waiter() -> SysResult {
    let (blocked_task, task_cx_ptr) = block_current_task_no_schedule();
    drop(blocked_task);
    schedule(task_cx_ptr);
    Ok(0)
}

fn read_user_pollfds(
    token: usize,
    fds: *const LinuxPollFd,
    nfds: usize,
) -> SysResult<Vec<LinuxPollFd>> {
    if nfds == 0 {
        return Ok(Vec::new());
    }
    if fds.is_null() {
        return Err(SysError::EFAULT);
    }
    if nfds > PPOLL_MAX_NFDS {
        return Err(SysError::EINVAL);
    }

    read_user_array(token, fds, nfds)
}

fn write_user_pollfds(token: usize, fds: *mut LinuxPollFd, pollfds: &[LinuxPollFd]) -> SysResult {
    write_user_array(token, fds, pollfds)?;
    Ok(0)
}

fn poll_events_from_user(events: i16) -> PollEvents {
    PollEvents::from_bits_truncate(events as u16)
}

fn poll_events_to_user(events: PollEvents) -> i16 {
    events.bits() as i16
}

fn poll_get_file_by_fd(fd: usize) -> SysResult<PollFile> {
    perf::record_poll_fd_table_lookup();
    get_file_by_fd(fd)
}

fn snapshot_pollfds(pollfds: &[LinuxPollFd]) -> Vec<Option<PollFileSlot>> {
    pollfds
        .iter()
        .map(|pollfd| {
            if pollfd.fd < 0 {
                None
            } else {
                Some(match poll_get_file_by_fd(pollfd.fd as usize) {
                    Ok(file) => PollFileSlot::File(file),
                    Err(err) => PollFileSlot::Error(err),
                })
            }
        })
        .collect()
}

fn scan_pollfds(
    pollfds: &mut [LinuxPollFd],
    files: &[Option<PollFileSlot>],
    waiter: Option<&Arc<PollWaiter>>,
) -> usize {
    let mut ready = 0usize;
    for (pollfd, file_slot) in pollfds.iter_mut().zip(files.iter()) {
        pollfd.revents = 0;
        if pollfd.fd < 0 {
            continue;
        }

        let events = poll_events_from_user(pollfd.events);
        match file_slot {
            Some(PollFileSlot::File(file)) => {
                let revents = file.poll_with_wait(events, waiter);
                pollfd.revents = poll_events_to_user(revents);
                if !revents.is_empty() {
                    ready += 1;
                }
            }
            Some(PollFileSlot::Error(SysError::EBADF)) => {
                pollfd.revents = poll_events_to_user(PollEvents::POLLNVAL);
                ready += 1;
            }
            Some(PollFileSlot::Error(_)) | None => {
                pollfd.revents = poll_events_to_user(PollEvents::POLLERR);
                ready += 1;
            }
        }
    }
    ready
}

fn poll_deadline_expired(deadline_ms: Option<usize>) -> bool {
    deadline_ms.is_some_and(|deadline_ms| get_time_ms() >= deadline_ms)
}

pub fn sys_ppoll(
    fds: *mut LinuxPollFd,
    nfds: usize,
    timeout: *const LinuxTimeSpec,
    _sigmask: *const u8,
    _sigsetsize: usize,
) -> SysResult {
    // UNFINISHED: ppoll currently ignores per-call signal-mask installation.
    // CONTEXT: musl implements pause() through ppoll() with a non-null mask on
    // RISC-V. Accepting the mask as a no-op lets LTP namespace helper daemons
    // sleep until killed instead of exiting immediately with ENOSYS.

    let token = current_user_token();
    let mut pollfds = read_user_pollfds(token, fds.cast_const(), nfds)?;
    let poll_files = snapshot_pollfds(&pollfds);
    let deadline_ms = relative_timeout_deadline_ms(token, timeout)?;
    let task = current_task().ok_or(SysError::ESRCH)?;

    loop {
        let ready = scan_pollfds(&mut pollfds, &poll_files, None);
        perf::record_poll_scan(pollfds.len(), ready);
        if ready > 0 {
            write_user_pollfds(token, fds, &pollfds)?;
            return Ok(ready as isize);
        }
        if poll_deadline_expired(deadline_ms) {
            write_user_pollfds(token, fds, &pollfds)?;
            return Ok(0);
        }
        if current_has_interrupting_signal() {
            return Err(SysError::EINTR);
        }
        if pollfds.is_empty() && deadline_ms.is_none() {
            block_signal_only_waiter()?;
        } else {
            let waiter = PollWaiter::new(Arc::clone(&task));
            let ready = scan_pollfds(&mut pollfds, &poll_files, Some(&waiter));
            perf::record_poll_scan(pollfds.len(), ready);
            if ready > 0 {
                write_user_pollfds(token, fds, &pollfds)?;
                return Ok(ready as isize);
            }
            if poll_deadline_expired(deadline_ms) {
                write_user_pollfds(token, fds, &pollfds)?;
                return Ok(0);
            }
            if current_has_interrupting_signal() {
                return Err(SysError::EINTR);
            }
            sleep_until_poll_event(&waiter, deadline_ms)?;
        }
    }
}

fn fdset_words(nfds: usize) -> usize {
    nfds.div_ceil(FD_SET_WORD_BITS)
}

fn read_user_fdset(token: usize, ptr: usize, nfds: usize) -> SysResult<Option<Vec<usize>>> {
    if ptr == 0 {
        return Ok(None);
    }
    let word_count = fdset_words(nfds);
    Ok(Some(read_user_array(
        token,
        ptr as *const usize,
        word_count,
    )?))
}

fn write_user_fdset(token: usize, ptr: usize, words: &[usize]) -> SysResult {
    if ptr == 0 {
        return Ok(0);
    }
    write_user_array(token, ptr as *mut usize, words)?;
    Ok(0)
}

fn fd_set(words: &mut [usize], fd: usize) {
    let word = fd / FD_SET_WORD_BITS;
    let bit = fd % FD_SET_WORD_BITS;
    if let Some(word) = words.get_mut(word) {
        *word |= 1usize << bit;
    }
}

fn for_each_set_fd<F>(nfds: usize, input: Option<&[usize]>, mut f: F) -> SysResult<usize>
where
    F: FnMut(usize) -> SysResult,
{
    let Some(input) = input else {
        return Ok(0);
    };
    let word_count = fdset_words(nfds);
    let mut visits = 0usize;
    for word_index in 0..word_count {
        let Some(mut word) = input.get(word_index).copied() else {
            break;
        };
        let first_fd = word_index * FD_SET_WORD_BITS;
        if first_fd + FD_SET_WORD_BITS > nfds {
            let valid_bits = nfds - first_fd;
            if valid_bits < FD_SET_WORD_BITS {
                word &= (1usize << valid_bits) - 1;
            }
        }
        while word != 0 {
            let bit = word.trailing_zeros() as usize;
            let fd = first_fd + bit;
            if fd >= nfds {
                break;
            }
            f(fd)?;
            visits += 1;
            word &= word - 1;
        }
    }
    Ok(visits)
}

fn snapshot_select_fdsets(
    nfds: usize,
    read_input: Option<&[usize]>,
    write_input: Option<&[usize]>,
    except_input: Option<&[usize]>,
) -> SysResult<Vec<Option<PollFileSlot>>> {
    let mut files: Vec<Option<PollFileSlot>> =
        Vec::from_iter(core::iter::repeat_with(|| None).take(nfds));
    for_each_set_fd(nfds, read_input, |fd| {
        if files[fd].is_none() {
            files[fd] = Some(match poll_get_file_by_fd(fd) {
                Ok(file) => PollFileSlot::File(file),
                Err(err) => PollFileSlot::Error(err),
            });
        }
        Ok(0)
    })?;
    for_each_set_fd(nfds, write_input, |fd| {
        if files[fd].is_none() {
            files[fd] = Some(match poll_get_file_by_fd(fd) {
                Ok(file) => PollFileSlot::File(file),
                Err(err) => PollFileSlot::Error(err),
            });
        }
        Ok(0)
    })?;
    for_each_set_fd(nfds, except_input, |fd| {
        if files[fd].is_none() {
            files[fd] = Some(match poll_get_file_by_fd(fd) {
                Ok(file) => PollFileSlot::File(file),
                Err(err) => PollFileSlot::Error(err),
            });
        }
        Ok(0)
    })?;
    Ok(files)
}

fn scan_fdset(
    nfds: usize,
    input: Option<&[usize]>,
    output: &mut [usize],
    events: PollEvents,
    files: &[Option<PollFileSlot>],
    waiter: Option<&Arc<PollWaiter>>,
) -> SysResult<usize> {
    let mut ready = 0usize;
    for_each_set_fd(nfds, input, |fd| {
        match files.get(fd).and_then(Option::as_ref) {
            Some(PollFileSlot::File(file)) => {
                if file.poll_with_wait(events, waiter).intersects(events) {
                    fd_set(output, fd);
                    ready += 1;
                }
                Ok(0)
            }
            Some(PollFileSlot::Error(err)) => Err(*err),
            None => Err(SysError::EBADF),
        }
    })?;
    Ok(ready)
}

fn scan_pselect_fdsets(
    nfds: usize,
    read_input: Option<&[usize]>,
    write_input: Option<&[usize]>,
    except_input: Option<&[usize]>,
    read_output: &mut [usize],
    write_output: &mut [usize],
    except_output: &mut [usize],
    files: &[Option<PollFileSlot>],
    waiter: Option<&Arc<PollWaiter>>,
) -> SysResult<usize> {
    read_output.fill(0);
    write_output.fill(0);
    except_output.fill(0);

    Ok(scan_fdset(
        nfds,
        read_input,
        read_output,
        PollEvents::POLLIN | PollEvents::POLLHUP | PollEvents::POLLRDHUP,
        files,
        waiter,
    )? + scan_fdset(
        nfds,
        write_input,
        write_output,
        PollEvents::POLLOUT,
        files,
        waiter,
    )? + scan_fdset(
        nfds,
        except_input,
        except_output,
        PollEvents::POLLPRI,
        files,
        waiter,
    )?)
}

pub fn sys_pselect6(
    nfds: usize,
    readfds: usize,
    writefds: usize,
    exceptfds: usize,
    timeout: *const LinuxTimeSpec,
    _sigmask: usize,
) -> SysResult {
    // UNFINISHED: pselect6 signal-mask installation is not implemented; the
    // mask argument is accepted as a no-op for libc select() compatibility on
    // the netperf path.
    if nfds > SELECT_MAX_NFDS {
        return Err(SysError::EINVAL);
    }

    let token = current_user_token();
    let read_input = read_user_fdset(token, readfds, nfds)?;
    let write_input = read_user_fdset(token, writefds, nfds)?;
    let except_input = read_user_fdset(token, exceptfds, nfds)?;
    let select_files = snapshot_select_fdsets(
        nfds,
        read_input.as_deref(),
        write_input.as_deref(),
        except_input.as_deref(),
    )?;
    let deadline_ms = relative_timeout_deadline_ms(token, timeout)?;
    let word_count = fdset_words(nfds);
    let mut read_output = Vec::from_iter(core::iter::repeat_n(0usize, word_count));
    let mut write_output = Vec::from_iter(core::iter::repeat_n(0usize, word_count));
    let mut except_output = Vec::from_iter(core::iter::repeat_n(0usize, word_count));
    let task = current_task().ok_or(SysError::ESRCH)?;
    let fdset_visits = nfds * usize::from(read_input.is_some())
        + nfds * usize::from(write_input.is_some())
        + nfds * usize::from(except_input.is_some());

    loop {
        let ready = scan_pselect_fdsets(
            nfds,
            read_input.as_deref(),
            write_input.as_deref(),
            except_input.as_deref(),
            &mut read_output,
            &mut write_output,
            &mut except_output,
            &select_files,
            None,
        )?;
        perf::record_poll_scan(fdset_visits, ready);

        if ready > 0 || poll_deadline_expired(deadline_ms) {
            write_user_fdset(token, readfds, &read_output)?;
            write_user_fdset(token, writefds, &write_output)?;
            write_user_fdset(token, exceptfds, &except_output)?;
            return Ok(ready as isize);
        }
        if current_has_interrupting_signal() {
            return Err(SysError::EINTR);
        }
        if read_input.is_none()
            && write_input.is_none()
            && except_input.is_none()
            && deadline_ms.is_none()
        {
            block_signal_only_waiter()?;
        } else {
            let waiter = PollWaiter::new(Arc::clone(&task));
            let ready = scan_pselect_fdsets(
                nfds,
                read_input.as_deref(),
                write_input.as_deref(),
                except_input.as_deref(),
                &mut read_output,
                &mut write_output,
                &mut except_output,
                &select_files,
                Some(&waiter),
            )?;
            perf::record_poll_scan(fdset_visits, ready);

            if ready > 0 || poll_deadline_expired(deadline_ms) {
                write_user_fdset(token, readfds, &read_output)?;
                write_user_fdset(token, writefds, &write_output)?;
                write_user_fdset(token, exceptfds, &except_output)?;
                return Ok(ready as isize);
            }
            if current_has_interrupting_signal() {
                return Err(SysError::EINTR);
            }
            sleep_until_poll_event(&waiter, deadline_ms)?;
        }
    }
}
