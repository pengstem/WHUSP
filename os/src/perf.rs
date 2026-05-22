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
    pub(crate) fd_alloc_bitmap_word_probes: usize,
    pub(crate) fd_alloc_expanded_slots: usize,
    pub(crate) fd_table_len_max: usize,
    pub(crate) fd_install_calls: usize,
    pub(crate) fd_take_calls: usize,
    pub(crate) epoll_ctl_calls: usize,
    pub(crate) epoll_ctl_linear_probes: usize,
    pub(crate) epoll_ctl_tree_lookups: usize,
    pub(crate) epoll_interest_count_max: usize,
    pub(crate) epoll_full_scans: usize,
    pub(crate) epoll_scan_interest_visits: usize,
    pub(crate) epoll_ready_events: usize,
    pub(crate) epoll_backoff_sleeps: usize,
    pub(crate) epoll_backoff_us: usize,
    pub(crate) poll_wait_scans: usize,
    pub(crate) poll_wait_fd_visits: usize,
    pub(crate) poll_wait_ready_events: usize,
    pub(crate) poll_backoff_sleeps: usize,
    pub(crate) poll_backoff_ms: usize,
    pub(crate) vfs_read_cache_hits: usize,
    pub(crate) vfs_read_cache_misses: usize,
    pub(crate) vfs_read_cache_bytes: usize,
    pub(crate) vfs_read_cache_backend_reads: usize,
    pub(crate) vfs_read_cache_invalidated_pages: usize,
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
    pub(crate) copy_file_range_calls: usize,
    pub(crate) copy_file_range_chunks: usize,
    pub(crate) copy_file_range_bytes: usize,
    pub(crate) sendfile_calls: usize,
    pub(crate) sendfile_chunks: usize,
    pub(crate) sendfile_bytes: usize,
    pub(crate) splice_calls: usize,
    pub(crate) splice_chunks: usize,
    pub(crate) splice_bytes: usize,
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
static FD_ALLOC_BITMAP_WORD_PROBES: AtomicUsize = AtomicUsize::new(0);
static FD_ALLOC_EXPANDED_SLOTS: AtomicUsize = AtomicUsize::new(0);
static FD_TABLE_LEN_MAX: AtomicUsize = AtomicUsize::new(0);
static FD_INSTALL_CALLS: AtomicUsize = AtomicUsize::new(0);
static FD_TAKE_CALLS: AtomicUsize = AtomicUsize::new(0);

static EPOLL_CTL_CALLS: AtomicUsize = AtomicUsize::new(0);
static EPOLL_CTL_LINEAR_PROBES: AtomicUsize = AtomicUsize::new(0);
static EPOLL_CTL_TREE_LOOKUPS: AtomicUsize = AtomicUsize::new(0);
static EPOLL_INTEREST_COUNT_MAX: AtomicUsize = AtomicUsize::new(0);
static EPOLL_FULL_SCANS: AtomicUsize = AtomicUsize::new(0);
static EPOLL_SCAN_INTEREST_VISITS: AtomicUsize = AtomicUsize::new(0);
static EPOLL_READY_EVENTS: AtomicUsize = AtomicUsize::new(0);
static EPOLL_BACKOFF_SLEEPS: AtomicUsize = AtomicUsize::new(0);
static EPOLL_BACKOFF_US: AtomicUsize = AtomicUsize::new(0);

static POLL_WAIT_SCANS: AtomicUsize = AtomicUsize::new(0);
static POLL_WAIT_FD_VISITS: AtomicUsize = AtomicUsize::new(0);
static POLL_WAIT_READY_EVENTS: AtomicUsize = AtomicUsize::new(0);
static POLL_BACKOFF_SLEEPS: AtomicUsize = AtomicUsize::new(0);
static POLL_BACKOFF_MS: AtomicUsize = AtomicUsize::new(0);

static VFS_READ_CACHE_HITS: AtomicUsize = AtomicUsize::new(0);
static VFS_READ_CACHE_MISSES: AtomicUsize = AtomicUsize::new(0);
static VFS_READ_CACHE_BYTES: AtomicUsize = AtomicUsize::new(0);
static VFS_READ_CACHE_BACKEND_READS: AtomicUsize = AtomicUsize::new(0);
static VFS_READ_CACHE_INVALIDATED_PAGES: AtomicUsize = AtomicUsize::new(0);

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

static COPY_FILE_RANGE_CALLS: AtomicUsize = AtomicUsize::new(0);
static COPY_FILE_RANGE_CHUNKS: AtomicUsize = AtomicUsize::new(0);
static COPY_FILE_RANGE_BYTES: AtomicUsize = AtomicUsize::new(0);
static SENDFILE_CALLS: AtomicUsize = AtomicUsize::new(0);
static SENDFILE_CHUNKS: AtomicUsize = AtomicUsize::new(0);
static SENDFILE_BYTES: AtomicUsize = AtomicUsize::new(0);
static SPLICE_CALLS: AtomicUsize = AtomicUsize::new(0);
static SPLICE_CHUNKS: AtomicUsize = AtomicUsize::new(0);
static SPLICE_BYTES: AtomicUsize = AtomicUsize::new(0);

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

pub(crate) fn record_fd_bitmap_word_probes(words: usize) {
    FD_ALLOC_BITMAP_WORD_PROBES.fetch_add(words, Ordering::Relaxed);
}

pub(crate) fn record_fd_install(table_len: usize) {
    FD_INSTALL_CALLS.fetch_add(1, Ordering::Relaxed);
    update_max(&FD_TABLE_LEN_MAX, table_len);
}

pub(crate) fn record_fd_take() {
    FD_TAKE_CALLS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_epoll_ctl(linear_probes: usize, tree_lookups: usize, interest_count: usize) {
    EPOLL_CTL_CALLS.fetch_add(1, Ordering::Relaxed);
    EPOLL_CTL_LINEAR_PROBES.fetch_add(linear_probes, Ordering::Relaxed);
    EPOLL_CTL_TREE_LOOKUPS.fetch_add(tree_lookups, Ordering::Relaxed);
    update_max(&EPOLL_INTEREST_COUNT_MAX, interest_count);
}

pub(crate) fn record_epoll_scan(interest_visits: usize, ready_events: usize) {
    EPOLL_FULL_SCANS.fetch_add(1, Ordering::Relaxed);
    EPOLL_SCAN_INTEREST_VISITS.fetch_add(interest_visits, Ordering::Relaxed);
    EPOLL_READY_EVENTS.fetch_add(ready_events, Ordering::Relaxed);
}

pub(crate) fn record_epoll_backoff_sleep(duration_us: usize) {
    EPOLL_BACKOFF_SLEEPS.fetch_add(1, Ordering::Relaxed);
    EPOLL_BACKOFF_US.fetch_add(duration_us, Ordering::Relaxed);
}

pub(crate) fn record_poll_scan(fd_visits: usize, ready_events: usize) {
    POLL_WAIT_SCANS.fetch_add(1, Ordering::Relaxed);
    POLL_WAIT_FD_VISITS.fetch_add(fd_visits, Ordering::Relaxed);
    POLL_WAIT_READY_EVENTS.fetch_add(ready_events, Ordering::Relaxed);
}

pub(crate) fn record_poll_backoff_sleep(duration_ms: usize) {
    POLL_BACKOFF_SLEEPS.fetch_add(1, Ordering::Relaxed);
    POLL_BACKOFF_MS.fetch_add(duration_ms, Ordering::Relaxed);
}

pub(crate) fn record_vfs_read_cache_hit(bytes: usize) {
    VFS_READ_CACHE_HITS.fetch_add(1, Ordering::Relaxed);
    VFS_READ_CACHE_BYTES.fetch_add(bytes, Ordering::Relaxed);
}

pub(crate) fn record_vfs_read_cache_miss() {
    VFS_READ_CACHE_MISSES.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_vfs_read_cache_backend_read() {
    VFS_READ_CACHE_BACKEND_READS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_vfs_read_cache_invalidation(pages: usize) {
    VFS_READ_CACHE_INVALIDATED_PAGES.fetch_add(pages, Ordering::Relaxed);
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

pub(crate) fn record_copy_file_range_call() {
    COPY_FILE_RANGE_CALLS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_copy_file_range_chunk(bytes: usize) {
    COPY_FILE_RANGE_CHUNKS.fetch_add(1, Ordering::Relaxed);
    COPY_FILE_RANGE_BYTES.fetch_add(bytes, Ordering::Relaxed);
}

pub(crate) fn record_sendfile_call() {
    SENDFILE_CALLS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_sendfile_chunk(bytes: usize) {
    SENDFILE_CHUNKS.fetch_add(1, Ordering::Relaxed);
    SENDFILE_BYTES.fetch_add(bytes, Ordering::Relaxed);
}

pub(crate) fn record_splice_call() {
    SPLICE_CALLS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_splice_chunk(bytes: usize) {
    SPLICE_CHUNKS.fetch_add(1, Ordering::Relaxed);
    SPLICE_BYTES.fetch_add(bytes, Ordering::Relaxed);
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
        fd_alloc_bitmap_word_probes: FD_ALLOC_BITMAP_WORD_PROBES.load(Ordering::Relaxed),
        fd_alloc_expanded_slots: FD_ALLOC_EXPANDED_SLOTS.load(Ordering::Relaxed),
        fd_table_len_max: FD_TABLE_LEN_MAX.load(Ordering::Relaxed),
        fd_install_calls: FD_INSTALL_CALLS.load(Ordering::Relaxed),
        fd_take_calls: FD_TAKE_CALLS.load(Ordering::Relaxed),
        epoll_ctl_calls: EPOLL_CTL_CALLS.load(Ordering::Relaxed),
        epoll_ctl_linear_probes: EPOLL_CTL_LINEAR_PROBES.load(Ordering::Relaxed),
        epoll_ctl_tree_lookups: EPOLL_CTL_TREE_LOOKUPS.load(Ordering::Relaxed),
        epoll_interest_count_max: EPOLL_INTEREST_COUNT_MAX.load(Ordering::Relaxed),
        epoll_full_scans: EPOLL_FULL_SCANS.load(Ordering::Relaxed),
        epoll_scan_interest_visits: EPOLL_SCAN_INTEREST_VISITS.load(Ordering::Relaxed),
        epoll_ready_events: EPOLL_READY_EVENTS.load(Ordering::Relaxed),
        epoll_backoff_sleeps: EPOLL_BACKOFF_SLEEPS.load(Ordering::Relaxed),
        epoll_backoff_us: EPOLL_BACKOFF_US.load(Ordering::Relaxed),
        poll_wait_scans: POLL_WAIT_SCANS.load(Ordering::Relaxed),
        poll_wait_fd_visits: POLL_WAIT_FD_VISITS.load(Ordering::Relaxed),
        poll_wait_ready_events: POLL_WAIT_READY_EVENTS.load(Ordering::Relaxed),
        poll_backoff_sleeps: POLL_BACKOFF_SLEEPS.load(Ordering::Relaxed),
        poll_backoff_ms: POLL_BACKOFF_MS.load(Ordering::Relaxed),
        vfs_read_cache_hits: VFS_READ_CACHE_HITS.load(Ordering::Relaxed),
        vfs_read_cache_misses: VFS_READ_CACHE_MISSES.load(Ordering::Relaxed),
        vfs_read_cache_bytes: VFS_READ_CACHE_BYTES.load(Ordering::Relaxed),
        vfs_read_cache_backend_reads: VFS_READ_CACHE_BACKEND_READS.load(Ordering::Relaxed),
        vfs_read_cache_invalidated_pages: VFS_READ_CACHE_INVALIDATED_PAGES.load(Ordering::Relaxed),
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
        copy_file_range_calls: COPY_FILE_RANGE_CALLS.load(Ordering::Relaxed),
        copy_file_range_chunks: COPY_FILE_RANGE_CHUNKS.load(Ordering::Relaxed),
        copy_file_range_bytes: COPY_FILE_RANGE_BYTES.load(Ordering::Relaxed),
        sendfile_calls: SENDFILE_CALLS.load(Ordering::Relaxed),
        sendfile_chunks: SENDFILE_CHUNKS.load(Ordering::Relaxed),
        sendfile_bytes: SENDFILE_BYTES.load(Ordering::Relaxed),
        splice_calls: SPLICE_CALLS.load(Ordering::Relaxed),
        splice_chunks: SPLICE_CHUNKS.load(Ordering::Relaxed),
        splice_bytes: SPLICE_BYTES.load(Ordering::Relaxed),
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
         fd_alloc_bitmap_word_probes {}\n\
         fd_alloc_expanded_slots {}\n\
         fd_table_len_max {}\n\
         fd_install_calls {}\n\
         fd_take_calls {}\n\
         epoll_ctl_calls {}\n\
         epoll_ctl_linear_probes {}\n\
         epoll_ctl_tree_lookups {}\n\
         epoll_interest_count_max {}\n\
         epoll_full_scans {}\n\
         epoll_scan_interest_visits {}\n\
         epoll_ready_events {}\n\
         epoll_backoff_sleeps {}\n\
         epoll_backoff_us {}\n\
         poll_wait_scans {}\n\
         poll_wait_fd_visits {}\n\
         poll_wait_ready_events {}\n\
         poll_backoff_sleeps {}\n\
         poll_backoff_ms {}\n\
         vfs_read_cache_hits {}\n\
         vfs_read_cache_misses {}\n\
         vfs_read_cache_bytes {}\n\
         vfs_read_cache_backend_reads {}\n\
         vfs_read_cache_invalidated_pages {}\n\
         pipe_read_calls {}\n\
         pipe_write_calls {}\n\
         pipe_read_bytes {}\n\
         pipe_write_bytes {}\n\
         pipe_read_byte_copy_bytes {}\n\
         pipe_write_byte_copy_bytes {}\n\
         pipe_read_chunk_copy_bytes {}\n\
         pipe_write_chunk_copy_bytes {}\n\
         pipe_reader_sleeps {}\n\
         pipe_writer_sleeps {}\n\
         copy_file_range_calls {}\n\
         copy_file_range_chunks {}\n\
         copy_file_range_bytes {}\n\
         sendfile_calls {}\n\
         sendfile_chunks {}\n\
         sendfile_bytes {}\n\
         splice_calls {}\n\
         splice_chunks {}\n\
         splice_bytes {}\n",
        stats.scheduler_fetch_calls,
        stats.scheduler_scanned_tasks,
        stats.scheduler_pruned_exited_tasks,
        stats.scheduler_ready_queue_len_max,
        stats.wakeup_front_successes,
        stats.wakeup_back_successes,
        stats.fd_alloc_calls,
        stats.fd_alloc_failures,
        stats.fd_alloc_probe_slots,
        stats.fd_alloc_bitmap_word_probes,
        stats.fd_alloc_expanded_slots,
        stats.fd_table_len_max,
        stats.fd_install_calls,
        stats.fd_take_calls,
        stats.epoll_ctl_calls,
        stats.epoll_ctl_linear_probes,
        stats.epoll_ctl_tree_lookups,
        stats.epoll_interest_count_max,
        stats.epoll_full_scans,
        stats.epoll_scan_interest_visits,
        stats.epoll_ready_events,
        stats.epoll_backoff_sleeps,
        stats.epoll_backoff_us,
        stats.poll_wait_scans,
        stats.poll_wait_fd_visits,
        stats.poll_wait_ready_events,
        stats.poll_backoff_sleeps,
        stats.poll_backoff_ms,
        stats.vfs_read_cache_hits,
        stats.vfs_read_cache_misses,
        stats.vfs_read_cache_bytes,
        stats.vfs_read_cache_backend_reads,
        stats.vfs_read_cache_invalidated_pages,
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
        stats.copy_file_range_calls,
        stats.copy_file_range_chunks,
        stats.copy_file_range_bytes,
        stats.sendfile_calls,
        stats.sendfile_chunks,
        stats.sendfile_bytes,
        stats.splice_calls,
        stats.splice_chunks,
        stats.splice_bytes,
    )
}
