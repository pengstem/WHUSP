use alloc::format;
use alloc::string::String;
use core::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct KernelPerfSnapshot {
    pub(crate) scheduler_fetch_calls: usize,
    pub(crate) scheduler_scanned_tasks: usize,
    pub(crate) scheduler_pruned_exited_tasks: usize,
    pub(crate) scheduler_ready_queue_len_max: usize,
    pub(crate) wakeup_front_successes: usize,
    pub(crate) wakeup_back_successes: usize,
    pub(crate) fd_alloc_calls: usize,
    pub(crate) fd_alloc_failures: usize,
    pub(crate) fd_alloc_probe_slots: usize,
    pub(crate) fd_alloc_expanded_slots: usize,
    pub(crate) fd_table_len_max: usize,
    pub(crate) fd_install_calls: usize,
    pub(crate) fd_take_calls: usize,
    pub(crate) pipe_read_calls: usize,
    pub(crate) pipe_write_calls: usize,
    pub(crate) pipe_read_bytes: usize,
    pub(crate) pipe_write_bytes: usize,
    pub(crate) pipe_read_byte_copy_bytes: usize,
    pub(crate) pipe_write_byte_copy_bytes: usize,
    pub(crate) pipe_read_chunk_copy_bytes: usize,
    pub(crate) pipe_write_chunk_copy_bytes: usize,
    pub(crate) pipe_reader_sleeps: usize,
    pub(crate) pipe_writer_sleeps: usize,
}

static SCHEDULER_FETCH_CALLS: AtomicUsize = AtomicUsize::new(0);
static SCHEDULER_SCANNED_TASKS: AtomicUsize = AtomicUsize::new(0);
static SCHEDULER_PRUNED_EXITED_TASKS: AtomicUsize = AtomicUsize::new(0);
static SCHEDULER_READY_QUEUE_LEN_MAX: AtomicUsize = AtomicUsize::new(0);
static WAKEUP_FRONT_SUCCESSES: AtomicUsize = AtomicUsize::new(0);
static WAKEUP_BACK_SUCCESSES: AtomicUsize = AtomicUsize::new(0);

static FD_ALLOC_CALLS: AtomicUsize = AtomicUsize::new(0);
static FD_ALLOC_FAILURES: AtomicUsize = AtomicUsize::new(0);
static FD_ALLOC_PROBE_SLOTS: AtomicUsize = AtomicUsize::new(0);
static FD_ALLOC_EXPANDED_SLOTS: AtomicUsize = AtomicUsize::new(0);
static FD_TABLE_LEN_MAX: AtomicUsize = AtomicUsize::new(0);
static FD_INSTALL_CALLS: AtomicUsize = AtomicUsize::new(0);
static FD_TAKE_CALLS: AtomicUsize = AtomicUsize::new(0);

static PIPE_READ_CALLS: AtomicUsize = AtomicUsize::new(0);
static PIPE_WRITE_CALLS: AtomicUsize = AtomicUsize::new(0);
static PIPE_READ_BYTES: AtomicUsize = AtomicUsize::new(0);
static PIPE_WRITE_BYTES: AtomicUsize = AtomicUsize::new(0);
static PIPE_READ_BYTE_COPY_BYTES: AtomicUsize = AtomicUsize::new(0);
static PIPE_WRITE_BYTE_COPY_BYTES: AtomicUsize = AtomicUsize::new(0);
static PIPE_READ_CHUNK_COPY_BYTES: AtomicUsize = AtomicUsize::new(0);
static PIPE_WRITE_CHUNK_COPY_BYTES: AtomicUsize = AtomicUsize::new(0);
static PIPE_READER_SLEEPS: AtomicUsize = AtomicUsize::new(0);
static PIPE_WRITER_SLEEPS: AtomicUsize = AtomicUsize::new(0);

fn update_max(cell: &AtomicUsize, value: usize) {
    let mut current = cell.load(Ordering::Relaxed);
    while value > current {
        match cell.compare_exchange_weak(current, value, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(next) => current = next,
        }
    }
}

pub(crate) fn record_scheduler_fetch(queue_len: usize, scanned: usize, pruned_exited: usize) {
    SCHEDULER_FETCH_CALLS.fetch_add(1, Ordering::Relaxed);
    SCHEDULER_SCANNED_TASKS.fetch_add(scanned, Ordering::Relaxed);
    SCHEDULER_PRUNED_EXITED_TASKS.fetch_add(pruned_exited, Ordering::Relaxed);
    update_max(&SCHEDULER_READY_QUEUE_LEN_MAX, queue_len);
}

pub(crate) fn record_task_wakeup(front: bool) {
    let counter = if front {
        &WAKEUP_FRONT_SUCCESSES
    } else {
        &WAKEUP_BACK_SUCCESSES
    };
    counter.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_fd_alloc(
    probed_slots: usize,
    expanded_slots: usize,
    table_len: usize,
    success: bool,
) {
    FD_ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
    FD_ALLOC_PROBE_SLOTS.fetch_add(probed_slots, Ordering::Relaxed);
    FD_ALLOC_EXPANDED_SLOTS.fetch_add(expanded_slots, Ordering::Relaxed);
    if !success {
        FD_ALLOC_FAILURES.fetch_add(1, Ordering::Relaxed);
    }
    update_max(&FD_TABLE_LEN_MAX, table_len);
}

pub(crate) fn record_fd_install(table_len: usize) {
    FD_INSTALL_CALLS.fetch_add(1, Ordering::Relaxed);
    update_max(&FD_TABLE_LEN_MAX, table_len);
}

pub(crate) fn record_fd_take() {
    FD_TAKE_CALLS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_pipe_read_call() {
    PIPE_READ_CALLS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_pipe_write_call() {
    PIPE_WRITE_CALLS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_pipe_read_chunk_copy(bytes: usize) {
    PIPE_READ_BYTES.fetch_add(bytes, Ordering::Relaxed);
    PIPE_READ_CHUNK_COPY_BYTES.fetch_add(bytes, Ordering::Relaxed);
}

pub(crate) fn record_pipe_write_chunk_copy(bytes: usize) {
    PIPE_WRITE_BYTES.fetch_add(bytes, Ordering::Relaxed);
    PIPE_WRITE_CHUNK_COPY_BYTES.fetch_add(bytes, Ordering::Relaxed);
}

pub(crate) fn record_pipe_reader_sleep() {
    PIPE_READER_SLEEPS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_pipe_writer_sleep() {
    PIPE_WRITER_SLEEPS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn snapshot() -> KernelPerfSnapshot {
    KernelPerfSnapshot {
        scheduler_fetch_calls: SCHEDULER_FETCH_CALLS.load(Ordering::Relaxed),
        scheduler_scanned_tasks: SCHEDULER_SCANNED_TASKS.load(Ordering::Relaxed),
        scheduler_pruned_exited_tasks: SCHEDULER_PRUNED_EXITED_TASKS.load(Ordering::Relaxed),
        scheduler_ready_queue_len_max: SCHEDULER_READY_QUEUE_LEN_MAX.load(Ordering::Relaxed),
        wakeup_front_successes: WAKEUP_FRONT_SUCCESSES.load(Ordering::Relaxed),
        wakeup_back_successes: WAKEUP_BACK_SUCCESSES.load(Ordering::Relaxed),
        fd_alloc_calls: FD_ALLOC_CALLS.load(Ordering::Relaxed),
        fd_alloc_failures: FD_ALLOC_FAILURES.load(Ordering::Relaxed),
        fd_alloc_probe_slots: FD_ALLOC_PROBE_SLOTS.load(Ordering::Relaxed),
        fd_alloc_expanded_slots: FD_ALLOC_EXPANDED_SLOTS.load(Ordering::Relaxed),
        fd_table_len_max: FD_TABLE_LEN_MAX.load(Ordering::Relaxed),
        fd_install_calls: FD_INSTALL_CALLS.load(Ordering::Relaxed),
        fd_take_calls: FD_TAKE_CALLS.load(Ordering::Relaxed),
        pipe_read_calls: PIPE_READ_CALLS.load(Ordering::Relaxed),
        pipe_write_calls: PIPE_WRITE_CALLS.load(Ordering::Relaxed),
        pipe_read_bytes: PIPE_READ_BYTES.load(Ordering::Relaxed),
        pipe_write_bytes: PIPE_WRITE_BYTES.load(Ordering::Relaxed),
        pipe_read_byte_copy_bytes: PIPE_READ_BYTE_COPY_BYTES.load(Ordering::Relaxed),
        pipe_write_byte_copy_bytes: PIPE_WRITE_BYTE_COPY_BYTES.load(Ordering::Relaxed),
        pipe_read_chunk_copy_bytes: PIPE_READ_CHUNK_COPY_BYTES.load(Ordering::Relaxed),
        pipe_write_chunk_copy_bytes: PIPE_WRITE_CHUNK_COPY_BYTES.load(Ordering::Relaxed),
        pipe_reader_sleeps: PIPE_READER_SLEEPS.load(Ordering::Relaxed),
        pipe_writer_sleeps: PIPE_WRITER_SLEEPS.load(Ordering::Relaxed),
    }
}

pub(crate) fn stats_content() -> String {
    let stats = snapshot();
    format!(
        "scheduler_fetch_calls {}\n\
         scheduler_scanned_tasks {}\n\
         scheduler_pruned_exited_tasks {}\n\
         scheduler_ready_queue_len_max {}\n\
         wakeup_front_successes {}\n\
         wakeup_back_successes {}\n\
         fd_alloc_calls {}\n\
         fd_alloc_failures {}\n\
         fd_alloc_probe_slots {}\n\
         fd_alloc_expanded_slots {}\n\
         fd_table_len_max {}\n\
         fd_install_calls {}\n\
         fd_take_calls {}\n\
         pipe_read_calls {}\n\
         pipe_write_calls {}\n\
         pipe_read_bytes {}\n\
         pipe_write_bytes {}\n\
         pipe_read_byte_copy_bytes {}\n\
         pipe_write_byte_copy_bytes {}\n\
         pipe_read_chunk_copy_bytes {}\n\
         pipe_write_chunk_copy_bytes {}\n\
         pipe_reader_sleeps {}\n\
         pipe_writer_sleeps {}\n",
        stats.scheduler_fetch_calls,
        stats.scheduler_scanned_tasks,
        stats.scheduler_pruned_exited_tasks,
        stats.scheduler_ready_queue_len_max,
        stats.wakeup_front_successes,
        stats.wakeup_back_successes,
        stats.fd_alloc_calls,
        stats.fd_alloc_failures,
        stats.fd_alloc_probe_slots,
        stats.fd_alloc_expanded_slots,
        stats.fd_table_len_max,
        stats.fd_install_calls,
        stats.fd_take_calls,
        stats.pipe_read_calls,
        stats.pipe_write_calls,
        stats.pipe_read_bytes,
        stats.pipe_write_bytes,
        stats.pipe_read_byte_copy_bytes,
        stats.pipe_write_byte_copy_bytes,
        stats.pipe_read_chunk_copy_bytes,
        stats.pipe_write_chunk_copy_bytes,
        stats.pipe_reader_sleeps,
        stats.pipe_writer_sleeps,
    )
}
