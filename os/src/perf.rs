#[derive(Clone, Copy, Debug, Default)]
#[cfg_attr(not(feature = "perf-counters"), allow(dead_code))]
pub(crate) struct KernelPerfSnapshot {
    pub(crate) scheduler_fetch_calls: usize,
    pub(crate) scheduler_scanned_tasks: usize,
    pub(crate) scheduler_pruned_exited_tasks: usize,
    pub(crate) scheduler_ready_queue_len_max: usize,
    pub(crate) scheduler_rt_priority_probes: usize,
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
    pub(crate) epoll_waiter_registrations: usize,
    pub(crate) epoll_waiter_sleeps: usize,
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
    pub(crate) vfs_read_cache_readahead_batches: usize,
    pub(crate) vfs_read_cache_readahead_pages: usize,
    pub(crate) page_cache_clean_evictions: usize,
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
    pub(crate) mmap_hole_searches: usize,
    pub(crate) mmap_hole_page_probes: usize,
    pub(crate) mmap_hole_gap_checks: usize,
    pub(crate) mmap_hole_area_visits: usize,
    pub(crate) mmap_vma_count_max: usize,
    pub(crate) vma_lookup_calls: usize,
    pub(crate) vma_lookup_hits: usize,
    pub(crate) vma_lookup_area_probes: usize,
    pub(crate) user_c_string_calls: usize,
    pub(crate) user_c_string_page_chunks: usize,
    pub(crate) user_c_string_scanned_bytes: usize,
    pub(crate) user_c_string_ascii_fast_bytes: usize,
    pub(crate) user_c_string_fallback_bytes: usize,
    pub(crate) usercopy_same_page_read_hits: usize,
    pub(crate) usercopy_same_page_write_hits: usize,
    pub(crate) usercopy_same_page_fast_bytes: usize,
    pub(crate) usercopy_slow_paths: usize,
    pub(crate) usercopy_slow_pages: usize,
    pub(crate) usercopy_read_value_calls: usize,
    pub(crate) usercopy_read_value_bytes: usize,
    pub(crate) usercopy_read_usize_calls: usize,
    pub(crate) usercopy_read_usize_bytes: usize,
    pub(crate) usercopy_read_array_item_calls: usize,
    pub(crate) usercopy_read_array_item_bytes: usize,
    pub(crate) usercopy_write_value_calls: usize,
    pub(crate) usercopy_write_value_bytes: usize,
    pub(crate) usercopy_write_array_item_calls: usize,
    pub(crate) usercopy_write_array_item_bytes: usize,
    pub(crate) usercopy_copy_to_user_calls: usize,
    pub(crate) usercopy_copy_to_user_bytes: usize,
    pub(crate) usercopy_copy_to_user_in_memory_set_calls: usize,
    pub(crate) usercopy_copy_to_user_in_memory_set_bytes: usize,
    pub(crate) inotify_no_live_group_fast_paths: usize,
    pub(crate) inotify_live_group_scans: usize,
    pub(crate) inotify_node_name_remember_calls: usize,
    pub(crate) inotify_unlinked_node_updates: usize,
    pub(crate) fanotify_no_live_group_fast_paths: usize,
    pub(crate) fanotify_live_group_scans: usize,
    pub(crate) fanotify_node_name_remember_calls: usize,
    pub(crate) fanotify_node_name_lookup_calls: usize,
    pub(crate) futex_cleanup_calls: usize,
    pub(crate) futex_cleanup_direct_hits: usize,
    pub(crate) futex_cleanup_already_unqueued: usize,
    pub(crate) futex_cleanup_fallback_scans: usize,
    pub(crate) futex_cleanup_fallback_queue_visits: usize,
    pub(crate) futex_cleanup_fallback_waiter_visits: usize,
    pub(crate) futex_queue_count_max: usize,
    pub(crate) futex_waiter_count_max: usize,
    pub(crate) futex_bucket_queue_count_max: usize,
    pub(crate) futex_bucket_waiter_count_max: usize,
}

#[cfg(feature = "perf-counters")]
mod enabled {
    use super::KernelPerfSnapshot;
    use alloc::format;
    use alloc::string::String;
    use core::sync::atomic::{AtomicUsize, Ordering};

    static SCHEDULER_FETCH_CALLS: AtomicUsize = AtomicUsize::new(0);
    static SCHEDULER_SCANNED_TASKS: AtomicUsize = AtomicUsize::new(0);
    static SCHEDULER_PRUNED_EXITED_TASKS: AtomicUsize = AtomicUsize::new(0);
    static SCHEDULER_READY_QUEUE_LEN_MAX: AtomicUsize = AtomicUsize::new(0);
    static SCHEDULER_RT_PRIORITY_PROBES: AtomicUsize = AtomicUsize::new(0);
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
    static EPOLL_WAITER_REGISTRATIONS: AtomicUsize = AtomicUsize::new(0);
    static EPOLL_WAITER_SLEEPS: AtomicUsize = AtomicUsize::new(0);

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
    static VFS_READ_CACHE_READAHEAD_BATCHES: AtomicUsize = AtomicUsize::new(0);
    static VFS_READ_CACHE_READAHEAD_PAGES: AtomicUsize = AtomicUsize::new(0);
    static PAGE_CACHE_CLEAN_EVICTIONS: AtomicUsize = AtomicUsize::new(0);

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

    static MMAP_HOLE_SEARCHES: AtomicUsize = AtomicUsize::new(0);
    static MMAP_HOLE_PAGE_PROBES: AtomicUsize = AtomicUsize::new(0);
    static MMAP_HOLE_GAP_CHECKS: AtomicUsize = AtomicUsize::new(0);
    static MMAP_HOLE_AREA_VISITS: AtomicUsize = AtomicUsize::new(0);
    static MMAP_VMA_COUNT_MAX: AtomicUsize = AtomicUsize::new(0);
    static VMA_LOOKUP_CALLS: AtomicUsize = AtomicUsize::new(0);
    static VMA_LOOKUP_HITS: AtomicUsize = AtomicUsize::new(0);
    static VMA_LOOKUP_AREA_PROBES: AtomicUsize = AtomicUsize::new(0);
    static USER_C_STRING_CALLS: AtomicUsize = AtomicUsize::new(0);
    static USER_C_STRING_PAGE_CHUNKS: AtomicUsize = AtomicUsize::new(0);
    static USER_C_STRING_SCANNED_BYTES: AtomicUsize = AtomicUsize::new(0);
    static USER_C_STRING_ASCII_FAST_BYTES: AtomicUsize = AtomicUsize::new(0);
    static USER_C_STRING_FALLBACK_BYTES: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_SAME_PAGE_READ_HITS: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_SAME_PAGE_WRITE_HITS: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_SAME_PAGE_FAST_BYTES: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_SLOW_PATHS: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_SLOW_PAGES: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_READ_VALUE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_READ_VALUE_BYTES: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_READ_USIZE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_READ_USIZE_BYTES: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_READ_ARRAY_ITEM_CALLS: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_READ_ARRAY_ITEM_BYTES: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_WRITE_VALUE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_WRITE_VALUE_BYTES: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_WRITE_ARRAY_ITEM_CALLS: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_WRITE_ARRAY_ITEM_BYTES: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_COPY_TO_USER_CALLS: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_COPY_TO_USER_BYTES: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_COPY_TO_USER_IN_MEMORY_SET_CALLS: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_COPY_TO_USER_IN_MEMORY_SET_BYTES: AtomicUsize = AtomicUsize::new(0);

    static INOTIFY_NO_LIVE_GROUP_FAST_PATHS: AtomicUsize = AtomicUsize::new(0);
    static INOTIFY_LIVE_GROUP_SCANS: AtomicUsize = AtomicUsize::new(0);
    static INOTIFY_NODE_NAME_REMEMBER_CALLS: AtomicUsize = AtomicUsize::new(0);
    static INOTIFY_UNLINKED_NODE_UPDATES: AtomicUsize = AtomicUsize::new(0);
    static FANOTIFY_NO_LIVE_GROUP_FAST_PATHS: AtomicUsize = AtomicUsize::new(0);
    static FANOTIFY_LIVE_GROUP_SCANS: AtomicUsize = AtomicUsize::new(0);
    static FANOTIFY_NODE_NAME_REMEMBER_CALLS: AtomicUsize = AtomicUsize::new(0);
    static FANOTIFY_NODE_NAME_LOOKUP_CALLS: AtomicUsize = AtomicUsize::new(0);

    static FUTEX_CLEANUP_CALLS: AtomicUsize = AtomicUsize::new(0);
    static FUTEX_CLEANUP_DIRECT_HITS: AtomicUsize = AtomicUsize::new(0);
    static FUTEX_CLEANUP_ALREADY_UNQUEUED: AtomicUsize = AtomicUsize::new(0);
    static FUTEX_CLEANUP_FALLBACK_SCANS: AtomicUsize = AtomicUsize::new(0);
    static FUTEX_CLEANUP_FALLBACK_QUEUE_VISITS: AtomicUsize = AtomicUsize::new(0);
    static FUTEX_CLEANUP_FALLBACK_WAITER_VISITS: AtomicUsize = AtomicUsize::new(0);
    static FUTEX_QUEUE_COUNT_MAX: AtomicUsize = AtomicUsize::new(0);
    static FUTEX_WAITER_COUNT_MAX: AtomicUsize = AtomicUsize::new(0);
    static FUTEX_BUCKET_QUEUE_COUNT_MAX: AtomicUsize = AtomicUsize::new(0);
    static FUTEX_BUCKET_WAITER_COUNT_MAX: AtomicUsize = AtomicUsize::new(0);

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

    pub(crate) fn record_scheduler_rt_priority_probes(probes: usize) {
        SCHEDULER_RT_PRIORITY_PROBES.fetch_add(probes, Ordering::Relaxed);
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

    pub(crate) fn record_epoll_ctl(
        linear_probes: usize,
        tree_lookups: usize,
        interest_count: usize,
    ) {
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

    pub(crate) fn record_epoll_waiter_registrations(count: usize) {
        EPOLL_WAITER_REGISTRATIONS.fetch_add(count, Ordering::Relaxed);
    }

    pub(crate) fn record_epoll_waiter_sleep() {
        EPOLL_WAITER_SLEEPS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_poll_scan(fd_visits: usize, ready_events: usize) {
        POLL_WAIT_SCANS.fetch_add(1, Ordering::Relaxed);
        POLL_WAIT_FD_VISITS.fetch_add(fd_visits, Ordering::Relaxed);
        POLL_WAIT_READY_EVENTS.fetch_add(ready_events, Ordering::Relaxed);
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

    pub(crate) fn record_vfs_read_cache_readahead(pages: usize) {
        VFS_READ_CACHE_READAHEAD_BATCHES.fetch_add(1, Ordering::Relaxed);
        VFS_READ_CACHE_READAHEAD_PAGES.fetch_add(pages, Ordering::Relaxed);
    }

    pub(crate) fn record_page_cache_clean_eviction(pages: usize) {
        PAGE_CACHE_CLEAN_EVICTIONS.fetch_add(pages, Ordering::Relaxed);
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

    pub(crate) fn record_mmap_hole_search(
        page_probes: usize,
        gap_checks: usize,
        area_visits: usize,
        vma_count: usize,
    ) {
        MMAP_HOLE_SEARCHES.fetch_add(1, Ordering::Relaxed);
        MMAP_HOLE_PAGE_PROBES.fetch_add(page_probes, Ordering::Relaxed);
        MMAP_HOLE_GAP_CHECKS.fetch_add(gap_checks, Ordering::Relaxed);
        MMAP_HOLE_AREA_VISITS.fetch_add(area_visits, Ordering::Relaxed);
        update_max(&MMAP_VMA_COUNT_MAX, vma_count);
    }

    pub(crate) fn record_vma_lookup(area_probes: usize, hit: bool) {
        VMA_LOOKUP_CALLS.fetch_add(1, Ordering::Relaxed);
        VMA_LOOKUP_AREA_PROBES.fetch_add(area_probes, Ordering::Relaxed);
        if hit {
            VMA_LOOKUP_HITS.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn record_user_c_string_call() {
        USER_C_STRING_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_user_c_string_chunk(
        scanned_bytes: usize,
        copied_bytes: usize,
        ascii: bool,
    ) {
        USER_C_STRING_PAGE_CHUNKS.fetch_add(1, Ordering::Relaxed);
        USER_C_STRING_SCANNED_BYTES.fetch_add(scanned_bytes, Ordering::Relaxed);
        if ascii {
            USER_C_STRING_ASCII_FAST_BYTES.fetch_add(copied_bytes, Ordering::Relaxed);
        } else {
            USER_C_STRING_FALLBACK_BYTES.fetch_add(copied_bytes, Ordering::Relaxed);
        }
    }

    pub(crate) fn record_usercopy_same_page_fast(access: UsercopyAccess, bytes: usize) {
        match access {
            UsercopyAccess::Read => USERCOPY_SAME_PAGE_READ_HITS.fetch_add(1, Ordering::Relaxed),
            UsercopyAccess::Write => USERCOPY_SAME_PAGE_WRITE_HITS.fetch_add(1, Ordering::Relaxed),
        };
        USERCOPY_SAME_PAGE_FAST_BYTES.fetch_add(bytes, Ordering::Relaxed);
    }

    pub(crate) fn record_usercopy_slow_path(page_count: usize) {
        USERCOPY_SLOW_PATHS.fetch_add(1, Ordering::Relaxed);
        USERCOPY_SLOW_PAGES.fetch_add(page_count, Ordering::Relaxed);
    }

    pub(crate) fn record_usercopy_site(site: UsercopySite, bytes: usize) {
        let (calls, total_bytes) = match site {
            UsercopySite::ReadValue => (&USERCOPY_READ_VALUE_CALLS, &USERCOPY_READ_VALUE_BYTES),
            UsercopySite::ReadUsize => (&USERCOPY_READ_USIZE_CALLS, &USERCOPY_READ_USIZE_BYTES),
            UsercopySite::ReadArrayItem => (
                &USERCOPY_READ_ARRAY_ITEM_CALLS,
                &USERCOPY_READ_ARRAY_ITEM_BYTES,
            ),
            UsercopySite::WriteValue => (&USERCOPY_WRITE_VALUE_CALLS, &USERCOPY_WRITE_VALUE_BYTES),
            UsercopySite::WriteArrayItem => (
                &USERCOPY_WRITE_ARRAY_ITEM_CALLS,
                &USERCOPY_WRITE_ARRAY_ITEM_BYTES,
            ),
            UsercopySite::CopyToUser => {
                (&USERCOPY_COPY_TO_USER_CALLS, &USERCOPY_COPY_TO_USER_BYTES)
            }
            UsercopySite::CopyToUserInMemorySet => (
                &USERCOPY_COPY_TO_USER_IN_MEMORY_SET_CALLS,
                &USERCOPY_COPY_TO_USER_IN_MEMORY_SET_BYTES,
            ),
        };
        calls.fetch_add(1, Ordering::Relaxed);
        total_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub(crate) fn record_inotify_no_live_group_fast_path() {
        INOTIFY_NO_LIVE_GROUP_FAST_PATHS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_inotify_live_group_scan() {
        INOTIFY_LIVE_GROUP_SCANS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_inotify_node_name_remember() {
        INOTIFY_NODE_NAME_REMEMBER_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_inotify_unlinked_node_update() {
        INOTIFY_UNLINKED_NODE_UPDATES.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_fanotify_no_live_group_fast_path() {
        FANOTIFY_NO_LIVE_GROUP_FAST_PATHS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_fanotify_live_group_scan() {
        FANOTIFY_LIVE_GROUP_SCANS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_fanotify_node_name_remember() {
        FANOTIFY_NODE_NAME_REMEMBER_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_fanotify_node_name_lookup() {
        FANOTIFY_NODE_NAME_LOOKUP_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    #[derive(Clone, Copy)]
    pub(crate) enum UsercopyAccess {
        Read,
        Write,
    }

    #[derive(Clone, Copy)]
    pub(crate) enum UsercopySite {
        ReadValue,
        ReadUsize,
        ReadArrayItem,
        WriteValue,
        WriteArrayItem,
        CopyToUser,
        CopyToUserInMemorySet,
    }

    pub(crate) fn record_futex_cleanup(
        direct_hit: bool,
        already_unqueued: bool,
        fallback_queue_visits: usize,
        fallback_waiter_visits: usize,
    ) {
        FUTEX_CLEANUP_CALLS.fetch_add(1, Ordering::Relaxed);
        if direct_hit {
            FUTEX_CLEANUP_DIRECT_HITS.fetch_add(1, Ordering::Relaxed);
        }
        if already_unqueued {
            FUTEX_CLEANUP_ALREADY_UNQUEUED.fetch_add(1, Ordering::Relaxed);
        }
        if fallback_queue_visits > 0 || fallback_waiter_visits > 0 {
            FUTEX_CLEANUP_FALLBACK_SCANS.fetch_add(1, Ordering::Relaxed);
            FUTEX_CLEANUP_FALLBACK_QUEUE_VISITS.fetch_add(fallback_queue_visits, Ordering::Relaxed);
            FUTEX_CLEANUP_FALLBACK_WAITER_VISITS
                .fetch_add(fallback_waiter_visits, Ordering::Relaxed);
        }
    }

    pub(crate) fn record_futex_manager_state(
        queue_count: usize,
        waiter_count: usize,
        bucket_queue_count: usize,
        bucket_waiter_count: usize,
    ) {
        update_max(&FUTEX_QUEUE_COUNT_MAX, queue_count);
        update_max(&FUTEX_WAITER_COUNT_MAX, waiter_count);
        update_max(&FUTEX_BUCKET_QUEUE_COUNT_MAX, bucket_queue_count);
        update_max(&FUTEX_BUCKET_WAITER_COUNT_MAX, bucket_waiter_count);
    }

    pub(crate) fn snapshot() -> KernelPerfSnapshot {
        KernelPerfSnapshot {
            scheduler_fetch_calls: SCHEDULER_FETCH_CALLS.load(Ordering::Relaxed),
            scheduler_scanned_tasks: SCHEDULER_SCANNED_TASKS.load(Ordering::Relaxed),
            scheduler_pruned_exited_tasks: SCHEDULER_PRUNED_EXITED_TASKS.load(Ordering::Relaxed),
            scheduler_ready_queue_len_max: SCHEDULER_READY_QUEUE_LEN_MAX.load(Ordering::Relaxed),
            scheduler_rt_priority_probes: SCHEDULER_RT_PRIORITY_PROBES.load(Ordering::Relaxed),
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
            epoll_waiter_registrations: EPOLL_WAITER_REGISTRATIONS.load(Ordering::Relaxed),
            epoll_waiter_sleeps: EPOLL_WAITER_SLEEPS.load(Ordering::Relaxed),
            poll_wait_scans: POLL_WAIT_SCANS.load(Ordering::Relaxed),
            poll_wait_fd_visits: POLL_WAIT_FD_VISITS.load(Ordering::Relaxed),
            poll_wait_ready_events: POLL_WAIT_READY_EVENTS.load(Ordering::Relaxed),
            poll_backoff_sleeps: POLL_BACKOFF_SLEEPS.load(Ordering::Relaxed),
            poll_backoff_ms: POLL_BACKOFF_MS.load(Ordering::Relaxed),
            vfs_read_cache_hits: VFS_READ_CACHE_HITS.load(Ordering::Relaxed),
            vfs_read_cache_misses: VFS_READ_CACHE_MISSES.load(Ordering::Relaxed),
            vfs_read_cache_bytes: VFS_READ_CACHE_BYTES.load(Ordering::Relaxed),
            vfs_read_cache_backend_reads: VFS_READ_CACHE_BACKEND_READS.load(Ordering::Relaxed),
            vfs_read_cache_invalidated_pages: VFS_READ_CACHE_INVALIDATED_PAGES
                .load(Ordering::Relaxed),
            vfs_read_cache_readahead_batches: VFS_READ_CACHE_READAHEAD_BATCHES
                .load(Ordering::Relaxed),
            vfs_read_cache_readahead_pages: VFS_READ_CACHE_READAHEAD_PAGES.load(Ordering::Relaxed),
            page_cache_clean_evictions: PAGE_CACHE_CLEAN_EVICTIONS.load(Ordering::Relaxed),
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
            mmap_hole_searches: MMAP_HOLE_SEARCHES.load(Ordering::Relaxed),
            mmap_hole_page_probes: MMAP_HOLE_PAGE_PROBES.load(Ordering::Relaxed),
            mmap_hole_gap_checks: MMAP_HOLE_GAP_CHECKS.load(Ordering::Relaxed),
            mmap_hole_area_visits: MMAP_HOLE_AREA_VISITS.load(Ordering::Relaxed),
            mmap_vma_count_max: MMAP_VMA_COUNT_MAX.load(Ordering::Relaxed),
            vma_lookup_calls: VMA_LOOKUP_CALLS.load(Ordering::Relaxed),
            vma_lookup_hits: VMA_LOOKUP_HITS.load(Ordering::Relaxed),
            vma_lookup_area_probes: VMA_LOOKUP_AREA_PROBES.load(Ordering::Relaxed),
            user_c_string_calls: USER_C_STRING_CALLS.load(Ordering::Relaxed),
            user_c_string_page_chunks: USER_C_STRING_PAGE_CHUNKS.load(Ordering::Relaxed),
            user_c_string_scanned_bytes: USER_C_STRING_SCANNED_BYTES.load(Ordering::Relaxed),
            user_c_string_ascii_fast_bytes: USER_C_STRING_ASCII_FAST_BYTES.load(Ordering::Relaxed),
            user_c_string_fallback_bytes: USER_C_STRING_FALLBACK_BYTES.load(Ordering::Relaxed),
            usercopy_same_page_read_hits: USERCOPY_SAME_PAGE_READ_HITS.load(Ordering::Relaxed),
            usercopy_same_page_write_hits: USERCOPY_SAME_PAGE_WRITE_HITS.load(Ordering::Relaxed),
            usercopy_same_page_fast_bytes: USERCOPY_SAME_PAGE_FAST_BYTES.load(Ordering::Relaxed),
            usercopy_slow_paths: USERCOPY_SLOW_PATHS.load(Ordering::Relaxed),
            usercopy_slow_pages: USERCOPY_SLOW_PAGES.load(Ordering::Relaxed),
            usercopy_read_value_calls: USERCOPY_READ_VALUE_CALLS.load(Ordering::Relaxed),
            usercopy_read_value_bytes: USERCOPY_READ_VALUE_BYTES.load(Ordering::Relaxed),
            usercopy_read_usize_calls: USERCOPY_READ_USIZE_CALLS.load(Ordering::Relaxed),
            usercopy_read_usize_bytes: USERCOPY_READ_USIZE_BYTES.load(Ordering::Relaxed),
            usercopy_read_array_item_calls: USERCOPY_READ_ARRAY_ITEM_CALLS.load(Ordering::Relaxed),
            usercopy_read_array_item_bytes: USERCOPY_READ_ARRAY_ITEM_BYTES.load(Ordering::Relaxed),
            usercopy_write_value_calls: USERCOPY_WRITE_VALUE_CALLS.load(Ordering::Relaxed),
            usercopy_write_value_bytes: USERCOPY_WRITE_VALUE_BYTES.load(Ordering::Relaxed),
            usercopy_write_array_item_calls: USERCOPY_WRITE_ARRAY_ITEM_CALLS
                .load(Ordering::Relaxed),
            usercopy_write_array_item_bytes: USERCOPY_WRITE_ARRAY_ITEM_BYTES
                .load(Ordering::Relaxed),
            usercopy_copy_to_user_calls: USERCOPY_COPY_TO_USER_CALLS.load(Ordering::Relaxed),
            usercopy_copy_to_user_bytes: USERCOPY_COPY_TO_USER_BYTES.load(Ordering::Relaxed),
            usercopy_copy_to_user_in_memory_set_calls: USERCOPY_COPY_TO_USER_IN_MEMORY_SET_CALLS
                .load(Ordering::Relaxed),
            usercopy_copy_to_user_in_memory_set_bytes: USERCOPY_COPY_TO_USER_IN_MEMORY_SET_BYTES
                .load(Ordering::Relaxed),
            inotify_no_live_group_fast_paths: INOTIFY_NO_LIVE_GROUP_FAST_PATHS
                .load(Ordering::Relaxed),
            inotify_live_group_scans: INOTIFY_LIVE_GROUP_SCANS.load(Ordering::Relaxed),
            inotify_node_name_remember_calls: INOTIFY_NODE_NAME_REMEMBER_CALLS
                .load(Ordering::Relaxed),
            inotify_unlinked_node_updates: INOTIFY_UNLINKED_NODE_UPDATES.load(Ordering::Relaxed),
            fanotify_no_live_group_fast_paths: FANOTIFY_NO_LIVE_GROUP_FAST_PATHS
                .load(Ordering::Relaxed),
            fanotify_live_group_scans: FANOTIFY_LIVE_GROUP_SCANS.load(Ordering::Relaxed),
            fanotify_node_name_remember_calls: FANOTIFY_NODE_NAME_REMEMBER_CALLS
                .load(Ordering::Relaxed),
            fanotify_node_name_lookup_calls: FANOTIFY_NODE_NAME_LOOKUP_CALLS
                .load(Ordering::Relaxed),
            futex_cleanup_calls: FUTEX_CLEANUP_CALLS.load(Ordering::Relaxed),
            futex_cleanup_direct_hits: FUTEX_CLEANUP_DIRECT_HITS.load(Ordering::Relaxed),
            futex_cleanup_already_unqueued: FUTEX_CLEANUP_ALREADY_UNQUEUED.load(Ordering::Relaxed),
            futex_cleanup_fallback_scans: FUTEX_CLEANUP_FALLBACK_SCANS.load(Ordering::Relaxed),
            futex_cleanup_fallback_queue_visits: FUTEX_CLEANUP_FALLBACK_QUEUE_VISITS
                .load(Ordering::Relaxed),
            futex_cleanup_fallback_waiter_visits: FUTEX_CLEANUP_FALLBACK_WAITER_VISITS
                .load(Ordering::Relaxed),
            futex_queue_count_max: FUTEX_QUEUE_COUNT_MAX.load(Ordering::Relaxed),
            futex_waiter_count_max: FUTEX_WAITER_COUNT_MAX.load(Ordering::Relaxed),
            futex_bucket_queue_count_max: FUTEX_BUCKET_QUEUE_COUNT_MAX.load(Ordering::Relaxed),
            futex_bucket_waiter_count_max: FUTEX_BUCKET_WAITER_COUNT_MAX.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn stats_content() -> String {
        let stats = snapshot();
        format!(
            "perf_counters_enabled 1\n\
         scheduler_fetch_calls {}\n\
         scheduler_scanned_tasks {}\n\
         scheduler_pruned_exited_tasks {}\n\
         scheduler_ready_queue_len_max {}\n\
         scheduler_rt_priority_probes {}\n\
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
         epoll_waiter_registrations {}\n\
         epoll_waiter_sleeps {}\n\
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
         vfs_read_cache_readahead_batches {}\n\
         vfs_read_cache_readahead_pages {}\n\
         page_cache_clean_evictions {}\n\
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
         splice_bytes {}\n\
         mmap_hole_searches {}\n\
         mmap_hole_page_probes {}\n\
         mmap_hole_gap_checks {}\n\
         mmap_hole_area_visits {}\n\
         mmap_vma_count_max {}\n\
         vma_lookup_calls {}\n\
         vma_lookup_hits {}\n\
         vma_lookup_area_probes {}\n\
         user_c_string_calls {}\n\
         user_c_string_page_chunks {}\n\
         user_c_string_scanned_bytes {}\n\
         user_c_string_ascii_fast_bytes {}\n\
         user_c_string_fallback_bytes {}\n\
         usercopy_same_page_read_hits {}\n\
         usercopy_same_page_write_hits {}\n\
         usercopy_same_page_fast_bytes {}\n\
         usercopy_slow_paths {}\n\
         usercopy_slow_pages {}\n\
         usercopy_read_value_calls {}\n\
         usercopy_read_value_bytes {}\n\
         usercopy_read_usize_calls {}\n\
         usercopy_read_usize_bytes {}\n\
         usercopy_read_array_item_calls {}\n\
         usercopy_read_array_item_bytes {}\n\
         usercopy_write_value_calls {}\n\
         usercopy_write_value_bytes {}\n\
         usercopy_write_array_item_calls {}\n\
         usercopy_write_array_item_bytes {}\n\
         usercopy_copy_to_user_calls {}\n\
         usercopy_copy_to_user_bytes {}\n\
         usercopy_copy_to_user_in_memory_set_calls {}\n\
         usercopy_copy_to_user_in_memory_set_bytes {}\n\
         inotify_no_live_group_fast_paths {}\n\
         inotify_live_group_scans {}\n\
         inotify_node_name_remember_calls {}\n\
         inotify_unlinked_node_updates {}\n\
         fanotify_no_live_group_fast_paths {}\n\
         fanotify_live_group_scans {}\n\
         fanotify_node_name_remember_calls {}\n\
         fanotify_node_name_lookup_calls {}\n\
         futex_cleanup_calls {}\n\
         futex_cleanup_direct_hits {}\n\
         futex_cleanup_already_unqueued {}\n\
         futex_cleanup_fallback_scans {}\n\
         futex_cleanup_fallback_queue_visits {}\n\
         futex_cleanup_fallback_waiter_visits {}\n\
         futex_queue_count_max {}\n\
         futex_waiter_count_max {}\n\
         futex_bucket_queue_count_max {}\n\
         futex_bucket_waiter_count_max {}\n",
            stats.scheduler_fetch_calls,
            stats.scheduler_scanned_tasks,
            stats.scheduler_pruned_exited_tasks,
            stats.scheduler_ready_queue_len_max,
            stats.scheduler_rt_priority_probes,
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
            stats.epoll_waiter_registrations,
            stats.epoll_waiter_sleeps,
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
            stats.vfs_read_cache_readahead_batches,
            stats.vfs_read_cache_readahead_pages,
            stats.page_cache_clean_evictions,
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
            stats.mmap_hole_searches,
            stats.mmap_hole_page_probes,
            stats.mmap_hole_gap_checks,
            stats.mmap_hole_area_visits,
            stats.mmap_vma_count_max,
            stats.vma_lookup_calls,
            stats.vma_lookup_hits,
            stats.vma_lookup_area_probes,
            stats.user_c_string_calls,
            stats.user_c_string_page_chunks,
            stats.user_c_string_scanned_bytes,
            stats.user_c_string_ascii_fast_bytes,
            stats.user_c_string_fallback_bytes,
            stats.usercopy_same_page_read_hits,
            stats.usercopy_same_page_write_hits,
            stats.usercopy_same_page_fast_bytes,
            stats.usercopy_slow_paths,
            stats.usercopy_slow_pages,
            stats.usercopy_read_value_calls,
            stats.usercopy_read_value_bytes,
            stats.usercopy_read_usize_calls,
            stats.usercopy_read_usize_bytes,
            stats.usercopy_read_array_item_calls,
            stats.usercopy_read_array_item_bytes,
            stats.usercopy_write_value_calls,
            stats.usercopy_write_value_bytes,
            stats.usercopy_write_array_item_calls,
            stats.usercopy_write_array_item_bytes,
            stats.usercopy_copy_to_user_calls,
            stats.usercopy_copy_to_user_bytes,
            stats.usercopy_copy_to_user_in_memory_set_calls,
            stats.usercopy_copy_to_user_in_memory_set_bytes,
            stats.inotify_no_live_group_fast_paths,
            stats.inotify_live_group_scans,
            stats.inotify_node_name_remember_calls,
            stats.inotify_unlinked_node_updates,
            stats.fanotify_no_live_group_fast_paths,
            stats.fanotify_live_group_scans,
            stats.fanotify_node_name_remember_calls,
            stats.fanotify_node_name_lookup_calls,
            stats.futex_cleanup_calls,
            stats.futex_cleanup_direct_hits,
            stats.futex_cleanup_already_unqueued,
            stats.futex_cleanup_fallback_scans,
            stats.futex_cleanup_fallback_queue_visits,
            stats.futex_cleanup_fallback_waiter_visits,
            stats.futex_queue_count_max,
            stats.futex_waiter_count_max,
            stats.futex_bucket_queue_count_max,
            stats.futex_bucket_waiter_count_max,
        )
    }
}

#[cfg(feature = "perf-counters")]
pub(crate) use enabled::*;

#[cfg(not(feature = "perf-counters"))]
mod disabled {
    use super::KernelPerfSnapshot;
    use alloc::string::String;

    #[derive(Clone, Copy)]
    pub(crate) enum UsercopyAccess {
        Read,
        Write,
    }

    #[derive(Clone, Copy)]
    pub(crate) enum UsercopySite {
        ReadValue,
        ReadUsize,
        ReadArrayItem,
        WriteValue,
        WriteArrayItem,
        CopyToUser,
        CopyToUserInMemorySet,
    }

    #[inline(always)]
    pub(crate) fn record_scheduler_fetch(
        _queue_len: usize,
        _scanned: usize,
        _pruned_exited: usize,
    ) {
    }

    #[inline(always)]
    pub(crate) fn record_scheduler_rt_priority_probes(_probes: usize) {}

    #[inline(always)]
    pub(crate) fn record_task_wakeup(_front: bool) {}

    #[inline(always)]
    pub(crate) fn record_fd_alloc(
        _probed_slots: usize,
        _expanded_slots: usize,
        _table_len: usize,
        _success: bool,
    ) {
    }

    #[inline(always)]
    pub(crate) fn record_fd_bitmap_word_probes(_words: usize) {}

    #[inline(always)]
    pub(crate) fn record_fd_install(_table_len: usize) {}

    #[inline(always)]
    pub(crate) fn record_fd_take() {}

    #[inline(always)]
    pub(crate) fn record_epoll_ctl(
        _linear_probes: usize,
        _tree_lookups: usize,
        _interest_count: usize,
    ) {
    }

    #[inline(always)]
    pub(crate) fn record_epoll_scan(_interest_visits: usize, _ready_events: usize) {}

    #[inline(always)]
    pub(crate) fn record_epoll_backoff_sleep(_duration_us: usize) {}

    #[inline(always)]
    pub(crate) fn record_epoll_waiter_registrations(_count: usize) {}

    #[inline(always)]
    pub(crate) fn record_epoll_waiter_sleep() {}

    #[inline(always)]
    pub(crate) fn record_poll_scan(_fd_visits: usize, _ready_events: usize) {}

    #[inline(always)]
    pub(crate) fn record_vfs_read_cache_hit(_bytes: usize) {}

    #[inline(always)]
    pub(crate) fn record_vfs_read_cache_miss() {}

    #[inline(always)]
    pub(crate) fn record_vfs_read_cache_backend_read() {}

    #[inline(always)]
    pub(crate) fn record_vfs_read_cache_invalidation(_pages: usize) {}

    #[inline(always)]
    pub(crate) fn record_vfs_read_cache_readahead(_pages: usize) {}

    #[inline(always)]
    pub(crate) fn record_page_cache_clean_eviction(_pages: usize) {}

    #[inline(always)]
    pub(crate) fn record_pipe_read_call() {}

    #[inline(always)]
    pub(crate) fn record_pipe_write_call() {}

    #[inline(always)]
    pub(crate) fn record_pipe_read_chunk_copy(_bytes: usize) {}

    #[inline(always)]
    pub(crate) fn record_pipe_write_chunk_copy(_bytes: usize) {}

    #[inline(always)]
    pub(crate) fn record_pipe_reader_sleep() {}

    #[inline(always)]
    pub(crate) fn record_pipe_writer_sleep() {}

    #[inline(always)]
    pub(crate) fn record_copy_file_range_call() {}

    #[inline(always)]
    pub(crate) fn record_copy_file_range_chunk(_bytes: usize) {}

    #[inline(always)]
    pub(crate) fn record_sendfile_call() {}

    #[inline(always)]
    pub(crate) fn record_sendfile_chunk(_bytes: usize) {}

    #[inline(always)]
    pub(crate) fn record_splice_call() {}

    #[inline(always)]
    pub(crate) fn record_splice_chunk(_bytes: usize) {}

    #[inline(always)]
    pub(crate) fn record_mmap_hole_search(
        _page_probes: usize,
        _gap_checks: usize,
        _area_visits: usize,
        _vma_count: usize,
    ) {
    }

    #[inline(always)]
    pub(crate) fn record_vma_lookup(_area_probes: usize, _hit: bool) {}

    #[inline(always)]
    pub(crate) fn record_user_c_string_call() {}

    #[inline(always)]
    pub(crate) fn record_user_c_string_chunk(
        _scanned_bytes: usize,
        _copied_bytes: usize,
        _ascii: bool,
    ) {
    }

    #[inline(always)]
    pub(crate) fn record_usercopy_same_page_fast(_access: UsercopyAccess, _bytes: usize) {}

    #[inline(always)]
    pub(crate) fn record_usercopy_slow_path(_page_count: usize) {}

    #[inline(always)]
    pub(crate) fn record_usercopy_site(_site: UsercopySite, _bytes: usize) {}

    #[inline(always)]
    pub(crate) fn record_inotify_no_live_group_fast_path() {}

    #[inline(always)]
    pub(crate) fn record_inotify_live_group_scan() {}

    #[inline(always)]
    pub(crate) fn record_inotify_node_name_remember() {}

    #[inline(always)]
    pub(crate) fn record_inotify_unlinked_node_update() {}

    #[inline(always)]
    pub(crate) fn record_fanotify_no_live_group_fast_path() {}

    #[inline(always)]
    pub(crate) fn record_fanotify_live_group_scan() {}

    #[inline(always)]
    pub(crate) fn record_fanotify_node_name_remember() {}

    #[inline(always)]
    pub(crate) fn record_fanotify_node_name_lookup() {}

    #[inline(always)]
    pub(crate) fn record_futex_cleanup(
        _direct_hit: bool,
        _already_unqueued: bool,
        _fallback_queue_visits: usize,
        _fallback_waiter_visits: usize,
    ) {
    }

    #[inline(always)]
    pub(crate) fn record_futex_manager_state(
        _queue_count: usize,
        _waiter_count: usize,
        _bucket_queue_count: usize,
        _bucket_waiter_count: usize,
    ) {
    }

    #[allow(dead_code)]
    #[inline(always)]
    pub(crate) fn snapshot() -> KernelPerfSnapshot {
        KernelPerfSnapshot::default()
    }

    pub(crate) fn stats_content() -> String {
        String::from("perf_counters_enabled 0\n")
    }
}

#[cfg(not(feature = "perf-counters"))]
pub(crate) use disabled::*;
