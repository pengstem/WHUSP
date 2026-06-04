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
    pub(crate) syscall_dispatch_calls: usize,
    pub(crate) syscall_identity_fast_paths: usize,
    pub(crate) task_current_calls: usize,
    pub(crate) task_current_process_calls: usize,
    pub(crate) task_current_user_token_calls: usize,
    pub(crate) task_current_trap_cx_calls: usize,
    pub(crate) task_current_trap_cx_user_va_calls: usize,
    pub(crate) task_current_trap_return_context_calls: usize,
    pub(crate) signal_action_table_lock_calls: usize,
    pub(crate) time_nanos_to_timespec_calls: usize,
    pub(crate) time_direct_timespec_calls: usize,
    pub(crate) riscv_return_fence_i_calls: usize,
    pub(crate) la_return_invtlb_calls: usize,
    pub(crate) rv_user_fp_save_calls: usize,
    pub(crate) rv_user_fp_restore_calls: usize,
    pub(crate) rv_user_fp_lazy_init_traps: usize,
    pub(crate) arch_instruction_barrier_calls: usize,
    pub(crate) tid_lookup_calls: usize,
    pub(crate) tid_lookup_process_visits: usize,
    pub(crate) tid_lookup_task_visits: usize,
    pub(crate) tid_lookup_hits: usize,
    pub(crate) tid_lookup_index_hits: usize,
    pub(crate) tid_lookup_stale_index_entries: usize,
    pub(crate) exec_stack_copy_calls: usize,
    pub(crate) exec_stack_copy_bytes: usize,
    pub(crate) wait_child_scan_passes: usize,
    pub(crate) wait_child_scan_slots: usize,
    pub(crate) scheduler_normal_requeue_calls: usize,
    pub(crate) scheduler_normal_vruntime_delta: usize,
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
    pub(crate) epoll_ready_list_checks: usize,
    pub(crate) epoll_ready_list_source_visits: usize,
    pub(crate) epoll_ready_list_hits: usize,
    pub(crate) epoll_backoff_sleeps: usize,
    pub(crate) epoll_backoff_us: usize,
    pub(crate) epoll_waiter_registrations: usize,
    pub(crate) epoll_waiter_sleeps: usize,
    pub(crate) poll_wait_scans: usize,
    pub(crate) poll_wait_fd_visits: usize,
    pub(crate) poll_wait_ready_events: usize,
    pub(crate) poll_backoff_sleeps: usize,
    pub(crate) poll_backoff_ms: usize,
    pub(crate) poll_fd_table_lookups: usize,
    pub(crate) vfs_read_cache_hits: usize,
    pub(crate) vfs_read_cache_misses: usize,
    pub(crate) vfs_read_cache_bytes: usize,
    pub(crate) vfs_read_cache_backend_reads: usize,
    pub(crate) vfs_read_cache_invalidated_pages: usize,
    pub(crate) vfs_read_cache_invalidation_calls: usize,
    pub(crate) vfs_read_cache_invalidation_scan_pages: usize,
    pub(crate) vfs_read_cache_readahead_batches: usize,
    pub(crate) vfs_read_cache_readahead_pages: usize,
    pub(crate) vfs_read_cache_eligible_calls: usize,
    pub(crate) vfs_read_cache_skip_too_large: usize,
    pub(crate) vfs_read_cache_skip_dirty_pages: usize,
    pub(crate) vfs_read_all_calls: usize,
    pub(crate) vfs_read_all_backend_reads: usize,
    pub(crate) vfs_read_all_bytes: usize,
    pub(crate) vfs_read_all_max_chunk: usize,
    pub(crate) vfs_read_backend_calls: usize,
    pub(crate) vfs_read_backend_bytes: usize,
    pub(crate) vfs_read_backend_max_chunk: usize,
    pub(crate) vfs_read_coalesced_calls: usize,
    pub(crate) vfs_read_coalesced_bytes: usize,
    pub(crate) vfs_path_component_scans: usize,
    pub(crate) vfs_path_components: usize,
    pub(crate) vfs_path_component_allocs: usize,
    pub(crate) vfs_visible_path_updates: usize,
    pub(crate) vfs_visible_path_allocs: usize,
    pub(crate) vfs_parent_cursor_clones: usize,
    pub(crate) vfs_dirent_read_calls: usize,
    pub(crate) vfs_dirent_user_buffer_bytes: usize,
    pub(crate) vfs_dirent_scratch_bytes: usize,
    pub(crate) vfs_dirent_returned_bytes: usize,
    pub(crate) vfs_dirent_max_scratch_bytes: usize,
    pub(crate) ext4_dirent_entries: usize,
    pub(crate) ext4_dirent_name_bytes: usize,
    pub(crate) ext4_dirent_name_allocs: usize,
    pub(crate) ext4_dirent_name_alloc_bytes: usize,
    pub(crate) procfs_content_builds: usize,
    pub(crate) procfs_content_bytes: usize,
    pub(crate) procfs_snapshot_hits: usize,
    pub(crate) procfs_snapshot_hit_bytes: usize,
    pub(crate) vfs_write_user_buffer_calls: usize,
    pub(crate) vfs_write_user_buffer_slices: usize,
    pub(crate) vfs_write_backend_calls: usize,
    pub(crate) vfs_write_backend_bytes: usize,
    pub(crate) vfs_write_coalesced_calls: usize,
    pub(crate) vfs_write_coalesced_bytes: usize,
    pub(crate) page_cache_clean_evictions: usize,
    pub(crate) tmpfs_allocated_payload_len_calls: usize,
    pub(crate) tmpfs_allocated_payload_sparse_extents: usize,
    pub(crate) tmpfs_allocated_logical_len_calls: usize,
    pub(crate) tmpfs_allocated_logical_sparse_extents: usize,
    pub(crate) frame_alloc_zeroed_calls: usize,
    pub(crate) frame_alloc_zeroed_bytes: usize,
    pub(crate) frame_alloc_uninit_calls: usize,
    pub(crate) frame_alloc_uninit_saved_bytes: usize,
    pub(crate) frame_dealloc_calls: usize,
    pub(crate) frame_dealloc_released: usize,
    pub(crate) frame_dealloc_refcount_drops: usize,
    pub(crate) frame_dealloc_recycled_scan_slots: usize,
    pub(crate) frame_dealloc_recycled_len_max: usize,
    pub(crate) dev_zero_read_calls: usize,
    pub(crate) dev_zero_read_bytes: usize,
    pub(crate) dev_zero_read_byte_writes: usize,
    pub(crate) dev_zero_read_fill_bytes: usize,
    pub(crate) dev_random_read_calls: usize,
    pub(crate) dev_random_read_bytes: usize,
    pub(crate) dev_random_byte_writes: usize,
    pub(crate) dev_random_word_fill_bytes: usize,
    pub(crate) uart_write_lock_calls: usize,
    pub(crate) uart_write_bytes: usize,
    pub(crate) tlb_flush_all_calls: usize,
    pub(crate) tlb_flush_range_calls: usize,
    pub(crate) tlb_flush_range_pages: usize,
    pub(crate) mount_metadata_calls: usize,
    pub(crate) mount_metadata_source_clone_bytes: usize,
    pub(crate) mount_fast_stat_flags_calls: usize,
    pub(crate) mount_fast_fs_type_calls: usize,
    pub(crate) eventfd_read_calls: usize,
    pub(crate) eventfd_write_calls: usize,
    pub(crate) eventfd_read_block_yields: usize,
    pub(crate) eventfd_write_block_yields: usize,
    pub(crate) eventfd_reader_sleeps: usize,
    pub(crate) eventfd_writer_sleeps: usize,
    pub(crate) eventfd_reader_wakeups: usize,
    pub(crate) eventfd_writer_wakeups: usize,
    pub(crate) local_socket_read_calls: usize,
    pub(crate) local_socket_write_calls: usize,
    pub(crate) local_socket_read_block_yields: usize,
    pub(crate) local_socket_write_block_yields: usize,
    pub(crate) local_socket_reader_sleeps: usize,
    pub(crate) local_socket_writer_sleeps: usize,
    pub(crate) local_socket_reader_wakeups: usize,
    pub(crate) local_socket_writer_wakeups: usize,
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
    pub(crate) vma_range_scans: usize,
    pub(crate) vma_range_area_visits: usize,
    pub(crate) vma_range_index_skips: usize,
    pub(crate) user_c_string_calls: usize,
    pub(crate) user_c_string_page_chunks: usize,
    pub(crate) user_c_string_scanned_bytes: usize,
    pub(crate) user_c_string_ascii_fast_bytes: usize,
    pub(crate) user_c_string_fallback_bytes: usize,
    pub(crate) usercopy_same_page_read_hits: usize,
    pub(crate) usercopy_same_page_write_hits: usize,
    pub(crate) usercopy_same_page_fast_bytes: usize,
    pub(crate) usercopy_leaf_pte_cache_hits: usize,
    pub(crate) usercopy_leaf_pte_cache_misses: usize,
    pub(crate) usercopy_leaf_pte_cache_invalidations: usize,
    pub(crate) usercopy_slow_paths: usize,
    pub(crate) usercopy_slow_pages: usize,
    pub(crate) usercopy_checked_range_calls: usize,
    pub(crate) usercopy_checked_range_pages: usize,
    pub(crate) usercopy_checked_range_bytes: usize,
    pub(crate) usercopy_range_reuse_hits: usize,
    pub(crate) usercopy_range_reuse_pages: usize,
    pub(crate) usercopy_range_reuse_bytes: usize,
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
    pub(crate) futex_wake_calls: usize,
    pub(crate) futex_wake_key_hits: usize,
    pub(crate) futex_wake_tasks: usize,
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
    static SYSCALL_DISPATCH_CALLS: AtomicUsize = AtomicUsize::new(0);
    static SYSCALL_IDENTITY_FAST_PATHS: AtomicUsize = AtomicUsize::new(0);
    static TASK_CURRENT_CALLS: AtomicUsize = AtomicUsize::new(0);
    static TASK_CURRENT_PROCESS_CALLS: AtomicUsize = AtomicUsize::new(0);
    static TASK_CURRENT_USER_TOKEN_CALLS: AtomicUsize = AtomicUsize::new(0);
    static TASK_CURRENT_TRAP_CX_CALLS: AtomicUsize = AtomicUsize::new(0);
    static TASK_CURRENT_TRAP_CX_USER_VA_CALLS: AtomicUsize = AtomicUsize::new(0);
    static TASK_CURRENT_TRAP_RETURN_CONTEXT_CALLS: AtomicUsize = AtomicUsize::new(0);
    static SIGNAL_ACTION_TABLE_LOCK_CALLS: AtomicUsize = AtomicUsize::new(0);
    static TIME_NANOS_TO_TIMESPEC_CALLS: AtomicUsize = AtomicUsize::new(0);
    static TIME_DIRECT_TIMESPEC_CALLS: AtomicUsize = AtomicUsize::new(0);
    static RISCV_RETURN_FENCE_I_CALLS: AtomicUsize = AtomicUsize::new(0);
    static LA_RETURN_INVTLB_CALLS: AtomicUsize = AtomicUsize::new(0);
    static RV_USER_FP_SAVE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static RV_USER_FP_RESTORE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static RV_USER_FP_LAZY_INIT_TRAPS: AtomicUsize = AtomicUsize::new(0);
    static ARCH_INSTRUCTION_BARRIER_CALLS: AtomicUsize = AtomicUsize::new(0);
    static TID_LOOKUP_CALLS: AtomicUsize = AtomicUsize::new(0);
    static TID_LOOKUP_PROCESS_VISITS: AtomicUsize = AtomicUsize::new(0);
    static TID_LOOKUP_TASK_VISITS: AtomicUsize = AtomicUsize::new(0);
    static TID_LOOKUP_HITS: AtomicUsize = AtomicUsize::new(0);
    static TID_LOOKUP_INDEX_HITS: AtomicUsize = AtomicUsize::new(0);
    static TID_LOOKUP_STALE_INDEX_ENTRIES: AtomicUsize = AtomicUsize::new(0);
    static EXEC_STACK_COPY_CALLS: AtomicUsize = AtomicUsize::new(0);
    static EXEC_STACK_COPY_BYTES: AtomicUsize = AtomicUsize::new(0);
    static WAIT_CHILD_SCAN_PASSES: AtomicUsize = AtomicUsize::new(0);
    static WAIT_CHILD_SCAN_SLOTS: AtomicUsize = AtomicUsize::new(0);
    static SCHEDULER_NORMAL_REQUEUE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static SCHEDULER_NORMAL_VRUNTIME_DELTA: AtomicUsize = AtomicUsize::new(0);

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
    static EPOLL_READY_LIST_CHECKS: AtomicUsize = AtomicUsize::new(0);
    static EPOLL_READY_LIST_SOURCE_VISITS: AtomicUsize = AtomicUsize::new(0);
    static EPOLL_READY_LIST_HITS: AtomicUsize = AtomicUsize::new(0);
    static EPOLL_BACKOFF_SLEEPS: AtomicUsize = AtomicUsize::new(0);
    static EPOLL_BACKOFF_US: AtomicUsize = AtomicUsize::new(0);
    static EPOLL_WAITER_REGISTRATIONS: AtomicUsize = AtomicUsize::new(0);
    static EPOLL_WAITER_SLEEPS: AtomicUsize = AtomicUsize::new(0);

    static POLL_WAIT_SCANS: AtomicUsize = AtomicUsize::new(0);
    static POLL_WAIT_FD_VISITS: AtomicUsize = AtomicUsize::new(0);
    static POLL_WAIT_READY_EVENTS: AtomicUsize = AtomicUsize::new(0);
    static POLL_BACKOFF_SLEEPS: AtomicUsize = AtomicUsize::new(0);
    static POLL_BACKOFF_MS: AtomicUsize = AtomicUsize::new(0);
    static POLL_FD_TABLE_LOOKUPS: AtomicUsize = AtomicUsize::new(0);

    static VFS_READ_CACHE_HITS: AtomicUsize = AtomicUsize::new(0);
    static VFS_READ_CACHE_MISSES: AtomicUsize = AtomicUsize::new(0);
    static VFS_READ_CACHE_BYTES: AtomicUsize = AtomicUsize::new(0);
    static VFS_READ_CACHE_BACKEND_READS: AtomicUsize = AtomicUsize::new(0);
    static VFS_READ_CACHE_INVALIDATED_PAGES: AtomicUsize = AtomicUsize::new(0);
    static VFS_READ_CACHE_INVALIDATION_CALLS: AtomicUsize = AtomicUsize::new(0);
    static VFS_READ_CACHE_INVALIDATION_SCAN_PAGES: AtomicUsize = AtomicUsize::new(0);
    static VFS_READ_CACHE_READAHEAD_BATCHES: AtomicUsize = AtomicUsize::new(0);
    static VFS_READ_CACHE_READAHEAD_PAGES: AtomicUsize = AtomicUsize::new(0);
    static VFS_READ_CACHE_ELIGIBLE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static VFS_READ_CACHE_SKIP_TOO_LARGE: AtomicUsize = AtomicUsize::new(0);
    static VFS_READ_CACHE_SKIP_DIRTY_PAGES: AtomicUsize = AtomicUsize::new(0);
    static VFS_READ_ALL_CALLS: AtomicUsize = AtomicUsize::new(0);
    static VFS_READ_ALL_BACKEND_READS: AtomicUsize = AtomicUsize::new(0);
    static VFS_READ_ALL_BYTES: AtomicUsize = AtomicUsize::new(0);
    static VFS_READ_ALL_MAX_CHUNK: AtomicUsize = AtomicUsize::new(0);
    static VFS_READ_BACKEND_CALLS: AtomicUsize = AtomicUsize::new(0);
    static VFS_READ_BACKEND_BYTES: AtomicUsize = AtomicUsize::new(0);
    static VFS_READ_BACKEND_MAX_CHUNK: AtomicUsize = AtomicUsize::new(0);
    static VFS_READ_COALESCED_CALLS: AtomicUsize = AtomicUsize::new(0);
    static VFS_READ_COALESCED_BYTES: AtomicUsize = AtomicUsize::new(0);
    static VFS_PATH_COMPONENT_SCANS: AtomicUsize = AtomicUsize::new(0);
    static VFS_PATH_COMPONENTS: AtomicUsize = AtomicUsize::new(0);
    static VFS_PATH_COMPONENT_ALLOCS: AtomicUsize = AtomicUsize::new(0);
    static VFS_VISIBLE_PATH_UPDATES: AtomicUsize = AtomicUsize::new(0);
    static VFS_VISIBLE_PATH_ALLOCS: AtomicUsize = AtomicUsize::new(0);
    static VFS_PARENT_CURSOR_CLONES: AtomicUsize = AtomicUsize::new(0);
    static VFS_DIRENT_READ_CALLS: AtomicUsize = AtomicUsize::new(0);
    static VFS_DIRENT_USER_BUFFER_BYTES: AtomicUsize = AtomicUsize::new(0);
    static VFS_DIRENT_SCRATCH_BYTES: AtomicUsize = AtomicUsize::new(0);
    static VFS_DIRENT_RETURNED_BYTES: AtomicUsize = AtomicUsize::new(0);
    static VFS_DIRENT_MAX_SCRATCH_BYTES: AtomicUsize = AtomicUsize::new(0);
    static EXT4_DIRENT_ENTRIES: AtomicUsize = AtomicUsize::new(0);
    static EXT4_DIRENT_NAME_BYTES: AtomicUsize = AtomicUsize::new(0);
    static EXT4_DIRENT_NAME_ALLOCS: AtomicUsize = AtomicUsize::new(0);
    static EXT4_DIRENT_NAME_ALLOC_BYTES: AtomicUsize = AtomicUsize::new(0);
    static PROCFS_CONTENT_BUILDS: AtomicUsize = AtomicUsize::new(0);
    static PROCFS_CONTENT_BYTES: AtomicUsize = AtomicUsize::new(0);
    static PROCFS_SNAPSHOT_HITS: AtomicUsize = AtomicUsize::new(0);
    static PROCFS_SNAPSHOT_HIT_BYTES: AtomicUsize = AtomicUsize::new(0);
    static VFS_WRITE_USER_BUFFER_CALLS: AtomicUsize = AtomicUsize::new(0);
    static VFS_WRITE_USER_BUFFER_SLICES: AtomicUsize = AtomicUsize::new(0);
    static VFS_WRITE_BACKEND_CALLS: AtomicUsize = AtomicUsize::new(0);
    static VFS_WRITE_BACKEND_BYTES: AtomicUsize = AtomicUsize::new(0);
    static VFS_WRITE_COALESCED_CALLS: AtomicUsize = AtomicUsize::new(0);
    static VFS_WRITE_COALESCED_BYTES: AtomicUsize = AtomicUsize::new(0);
    static PAGE_CACHE_CLEAN_EVICTIONS: AtomicUsize = AtomicUsize::new(0);
    static TMPFS_ALLOCATED_PAYLOAD_LEN_CALLS: AtomicUsize = AtomicUsize::new(0);
    static TMPFS_ALLOCATED_PAYLOAD_SPARSE_EXTENTS: AtomicUsize = AtomicUsize::new(0);
    static TMPFS_ALLOCATED_LOGICAL_LEN_CALLS: AtomicUsize = AtomicUsize::new(0);
    static TMPFS_ALLOCATED_LOGICAL_SPARSE_EXTENTS: AtomicUsize = AtomicUsize::new(0);
    static FRAME_ALLOC_ZEROED_CALLS: AtomicUsize = AtomicUsize::new(0);
    static FRAME_ALLOC_ZEROED_BYTES: AtomicUsize = AtomicUsize::new(0);
    static FRAME_ALLOC_UNINIT_CALLS: AtomicUsize = AtomicUsize::new(0);
    static FRAME_ALLOC_UNINIT_SAVED_BYTES: AtomicUsize = AtomicUsize::new(0);
    static FRAME_DEALLOC_CALLS: AtomicUsize = AtomicUsize::new(0);
    static FRAME_DEALLOC_RELEASED: AtomicUsize = AtomicUsize::new(0);
    static FRAME_DEALLOC_REFCOUNT_DROPS: AtomicUsize = AtomicUsize::new(0);
    static FRAME_DEALLOC_RECYCLED_SCAN_SLOTS: AtomicUsize = AtomicUsize::new(0);
    static FRAME_DEALLOC_RECYCLED_LEN_MAX: AtomicUsize = AtomicUsize::new(0);
    static DEV_ZERO_READ_CALLS: AtomicUsize = AtomicUsize::new(0);
    static DEV_ZERO_READ_BYTES: AtomicUsize = AtomicUsize::new(0);
    static DEV_ZERO_READ_BYTE_WRITES: AtomicUsize = AtomicUsize::new(0);
    static DEV_ZERO_READ_FILL_BYTES: AtomicUsize = AtomicUsize::new(0);
    static DEV_RANDOM_READ_CALLS: AtomicUsize = AtomicUsize::new(0);
    static DEV_RANDOM_READ_BYTES: AtomicUsize = AtomicUsize::new(0);
    static DEV_RANDOM_BYTE_WRITES: AtomicUsize = AtomicUsize::new(0);
    static DEV_RANDOM_WORD_FILL_BYTES: AtomicUsize = AtomicUsize::new(0);
    static UART_WRITE_LOCK_CALLS: AtomicUsize = AtomicUsize::new(0);
    static UART_WRITE_BYTES: AtomicUsize = AtomicUsize::new(0);
    static TLB_FLUSH_ALL_CALLS: AtomicUsize = AtomicUsize::new(0);
    static TLB_FLUSH_RANGE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static TLB_FLUSH_RANGE_PAGES: AtomicUsize = AtomicUsize::new(0);
    static MOUNT_METADATA_CALLS: AtomicUsize = AtomicUsize::new(0);
    static MOUNT_METADATA_SOURCE_CLONE_BYTES: AtomicUsize = AtomicUsize::new(0);
    static MOUNT_FAST_STAT_FLAGS_CALLS: AtomicUsize = AtomicUsize::new(0);
    static MOUNT_FAST_FS_TYPE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static EVENTFD_READ_CALLS: AtomicUsize = AtomicUsize::new(0);
    static EVENTFD_WRITE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static EVENTFD_READ_BLOCK_YIELDS: AtomicUsize = AtomicUsize::new(0);
    static EVENTFD_WRITE_BLOCK_YIELDS: AtomicUsize = AtomicUsize::new(0);
    static EVENTFD_READER_SLEEPS: AtomicUsize = AtomicUsize::new(0);
    static EVENTFD_WRITER_SLEEPS: AtomicUsize = AtomicUsize::new(0);
    static EVENTFD_READER_WAKEUPS: AtomicUsize = AtomicUsize::new(0);
    static EVENTFD_WRITER_WAKEUPS: AtomicUsize = AtomicUsize::new(0);
    static LOCAL_SOCKET_READ_CALLS: AtomicUsize = AtomicUsize::new(0);
    static LOCAL_SOCKET_WRITE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static LOCAL_SOCKET_READ_BLOCK_YIELDS: AtomicUsize = AtomicUsize::new(0);
    static LOCAL_SOCKET_WRITE_BLOCK_YIELDS: AtomicUsize = AtomicUsize::new(0);
    static LOCAL_SOCKET_READER_SLEEPS: AtomicUsize = AtomicUsize::new(0);
    static LOCAL_SOCKET_WRITER_SLEEPS: AtomicUsize = AtomicUsize::new(0);
    static LOCAL_SOCKET_READER_WAKEUPS: AtomicUsize = AtomicUsize::new(0);
    static LOCAL_SOCKET_WRITER_WAKEUPS: AtomicUsize = AtomicUsize::new(0);

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
    static VMA_RANGE_SCANS: AtomicUsize = AtomicUsize::new(0);
    static VMA_RANGE_AREA_VISITS: AtomicUsize = AtomicUsize::new(0);
    static VMA_RANGE_INDEX_SKIPS: AtomicUsize = AtomicUsize::new(0);
    static USER_C_STRING_CALLS: AtomicUsize = AtomicUsize::new(0);
    static USER_C_STRING_PAGE_CHUNKS: AtomicUsize = AtomicUsize::new(0);
    static USER_C_STRING_SCANNED_BYTES: AtomicUsize = AtomicUsize::new(0);
    static USER_C_STRING_ASCII_FAST_BYTES: AtomicUsize = AtomicUsize::new(0);
    static USER_C_STRING_FALLBACK_BYTES: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_SAME_PAGE_READ_HITS: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_SAME_PAGE_WRITE_HITS: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_SAME_PAGE_FAST_BYTES: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_LEAF_PTE_CACHE_HITS: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_LEAF_PTE_CACHE_MISSES: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_LEAF_PTE_CACHE_INVALIDATIONS: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_SLOW_PATHS: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_SLOW_PAGES: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_CHECKED_RANGE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_CHECKED_RANGE_PAGES: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_CHECKED_RANGE_BYTES: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_RANGE_REUSE_HITS: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_RANGE_REUSE_PAGES: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_RANGE_REUSE_BYTES: AtomicUsize = AtomicUsize::new(0);
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
    static FUTEX_WAKE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static FUTEX_WAKE_KEY_HITS: AtomicUsize = AtomicUsize::new(0);
    static FUTEX_WAKE_TASKS: AtomicUsize = AtomicUsize::new(0);
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

    pub(crate) fn record_syscall_dispatch_call() {
        SYSCALL_DISPATCH_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_syscall_identity_fast_path() {
        SYSCALL_IDENTITY_FAST_PATHS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_task_current_call() {
        TASK_CURRENT_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_task_current_process_call() {
        TASK_CURRENT_PROCESS_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_task_current_user_token_call() {
        TASK_CURRENT_USER_TOKEN_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_task_current_trap_cx_call() {
        TASK_CURRENT_TRAP_CX_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_task_current_trap_return_context_call() {
        TASK_CURRENT_TRAP_RETURN_CONTEXT_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_signal_action_table_lock_call() {
        SIGNAL_ACTION_TABLE_LOCK_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_time_nanos_to_timespec_call() {
        TIME_NANOS_TO_TIMESPEC_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_time_direct_timespec_call() {
        TIME_DIRECT_TIMESPEC_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub(crate) fn record_riscv_return_fence_i_call() {
        RISCV_RETURN_FENCE_I_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub(crate) fn record_la_return_invtlb_call() {
        LA_RETURN_INVTLB_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub(crate) fn record_rv_user_fp_save_call() {
        RV_USER_FP_SAVE_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub(crate) fn record_rv_user_fp_restore_call() {
        RV_USER_FP_RESTORE_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub(crate) fn record_rv_user_fp_lazy_init_trap() {
        RV_USER_FP_LAZY_INIT_TRAPS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_arch_instruction_barrier_call() {
        ARCH_INSTRUCTION_BARRIER_CALLS.fetch_add(1, Ordering::Relaxed);
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

    pub(crate) fn record_epoll_ready_list(source_visits: usize, ready_events: usize) {
        EPOLL_READY_LIST_CHECKS.fetch_add(1, Ordering::Relaxed);
        EPOLL_READY_LIST_SOURCE_VISITS.fetch_add(source_visits, Ordering::Relaxed);
        EPOLL_READY_LIST_HITS.fetch_add(ready_events, Ordering::Relaxed);
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

    pub(crate) fn record_poll_fd_table_lookup() {
        POLL_FD_TABLE_LOOKUPS.fetch_add(1, Ordering::Relaxed);
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

    pub(crate) fn record_vfs_read_cache_invalidation(pages: usize, scanned_pages: usize) {
        VFS_READ_CACHE_INVALIDATION_CALLS.fetch_add(1, Ordering::Relaxed);
        VFS_READ_CACHE_INVALIDATED_PAGES.fetch_add(pages, Ordering::Relaxed);
        VFS_READ_CACHE_INVALIDATION_SCAN_PAGES.fetch_add(scanned_pages, Ordering::Relaxed);
    }

    pub(crate) fn record_vfs_read_cache_readahead(pages: usize) {
        VFS_READ_CACHE_READAHEAD_BATCHES.fetch_add(1, Ordering::Relaxed);
        VFS_READ_CACHE_READAHEAD_PAGES.fetch_add(pages, Ordering::Relaxed);
    }

    pub(crate) fn record_vfs_read_cache_eligible() {
        VFS_READ_CACHE_ELIGIBLE_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_vfs_read_cache_skip_too_large() {
        VFS_READ_CACHE_SKIP_TOO_LARGE.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_vfs_read_cache_skip_dirty_pages() {
        VFS_READ_CACHE_SKIP_DIRTY_PAGES.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_vfs_read_all_call() {
        VFS_READ_ALL_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_vfs_read_all_backend_read(bytes: usize) {
        VFS_READ_ALL_BACKEND_READS.fetch_add(1, Ordering::Relaxed);
        VFS_READ_ALL_BYTES.fetch_add(bytes, Ordering::Relaxed);
        update_max(&VFS_READ_ALL_MAX_CHUNK, bytes);
    }

    pub(crate) fn record_vfs_read_backend(bytes: usize) {
        VFS_READ_BACKEND_CALLS.fetch_add(1, Ordering::Relaxed);
        VFS_READ_BACKEND_BYTES.fetch_add(bytes, Ordering::Relaxed);
        update_max(&VFS_READ_BACKEND_MAX_CHUNK, bytes);
    }

    pub(crate) fn record_vfs_read_coalesced(bytes: usize) {
        VFS_READ_COALESCED_CALLS.fetch_add(1, Ordering::Relaxed);
        VFS_READ_COALESCED_BYTES.fetch_add(bytes, Ordering::Relaxed);
    }

    pub(crate) fn record_vfs_path_components(components: usize, allocations: usize) {
        VFS_PATH_COMPONENT_SCANS.fetch_add(1, Ordering::Relaxed);
        VFS_PATH_COMPONENTS.fetch_add(components, Ordering::Relaxed);
        VFS_PATH_COMPONENT_ALLOCS.fetch_add(allocations, Ordering::Relaxed);
    }

    pub(crate) fn record_vfs_visible_path_update(allocations: usize) {
        VFS_VISIBLE_PATH_UPDATES.fetch_add(1, Ordering::Relaxed);
        VFS_VISIBLE_PATH_ALLOCS.fetch_add(allocations, Ordering::Relaxed);
    }

    pub(crate) fn record_vfs_visible_path_allocation() {
        VFS_VISIBLE_PATH_ALLOCS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_vfs_dirent_read(
        user_buffer_bytes: usize,
        scratch_bytes: usize,
        returned_bytes: usize,
    ) {
        VFS_DIRENT_READ_CALLS.fetch_add(1, Ordering::Relaxed);
        VFS_DIRENT_USER_BUFFER_BYTES.fetch_add(user_buffer_bytes, Ordering::Relaxed);
        VFS_DIRENT_SCRATCH_BYTES.fetch_add(scratch_bytes, Ordering::Relaxed);
        VFS_DIRENT_RETURNED_BYTES.fetch_add(returned_bytes, Ordering::Relaxed);
        update_max(&VFS_DIRENT_MAX_SCRATCH_BYTES, scratch_bytes);
    }

    pub(crate) fn record_ext4_dirent_name(name_len: usize, allocated: bool) {
        EXT4_DIRENT_ENTRIES.fetch_add(1, Ordering::Relaxed);
        EXT4_DIRENT_NAME_BYTES.fetch_add(name_len, Ordering::Relaxed);
        if allocated {
            EXT4_DIRENT_NAME_ALLOCS.fetch_add(1, Ordering::Relaxed);
            EXT4_DIRENT_NAME_ALLOC_BYTES.fetch_add(name_len, Ordering::Relaxed);
        }
    }

    pub(crate) fn record_procfs_content_build(bytes: usize) {
        PROCFS_CONTENT_BUILDS.fetch_add(1, Ordering::Relaxed);
        PROCFS_CONTENT_BYTES.fetch_add(bytes, Ordering::Relaxed);
    }

    pub(crate) fn record_procfs_snapshot_hit(bytes: usize) {
        PROCFS_SNAPSHOT_HITS.fetch_add(1, Ordering::Relaxed);
        PROCFS_SNAPSHOT_HIT_BYTES.fetch_add(bytes, Ordering::Relaxed);
    }

    pub(crate) fn record_vfs_write_user_buffer(slices: usize) {
        VFS_WRITE_USER_BUFFER_CALLS.fetch_add(1, Ordering::Relaxed);
        VFS_WRITE_USER_BUFFER_SLICES.fetch_add(slices, Ordering::Relaxed);
    }

    pub(crate) fn record_vfs_write_backend(bytes: usize) {
        VFS_WRITE_BACKEND_CALLS.fetch_add(1, Ordering::Relaxed);
        VFS_WRITE_BACKEND_BYTES.fetch_add(bytes, Ordering::Relaxed);
    }

    pub(crate) fn record_vfs_write_coalesced(bytes: usize) {
        VFS_WRITE_COALESCED_CALLS.fetch_add(1, Ordering::Relaxed);
        VFS_WRITE_COALESCED_BYTES.fetch_add(bytes, Ordering::Relaxed);
    }

    pub(crate) fn record_page_cache_clean_eviction(pages: usize) {
        PAGE_CACHE_CLEAN_EVICTIONS.fetch_add(pages, Ordering::Relaxed);
    }

    pub(crate) fn record_tmpfs_allocated_payload_len(sparse_extents: usize) {
        TMPFS_ALLOCATED_PAYLOAD_LEN_CALLS.fetch_add(1, Ordering::Relaxed);
        TMPFS_ALLOCATED_PAYLOAD_SPARSE_EXTENTS.fetch_add(sparse_extents, Ordering::Relaxed);
    }

    pub(crate) fn record_tmpfs_allocated_logical_len(sparse_extents: usize) {
        TMPFS_ALLOCATED_LOGICAL_LEN_CALLS.fetch_add(1, Ordering::Relaxed);
        TMPFS_ALLOCATED_LOGICAL_SPARSE_EXTENTS.fetch_add(sparse_extents, Ordering::Relaxed);
    }

    pub(crate) fn record_frame_alloc(zeroed: bool) {
        if zeroed {
            FRAME_ALLOC_ZEROED_CALLS.fetch_add(1, Ordering::Relaxed);
            FRAME_ALLOC_ZEROED_BYTES.fetch_add(crate::config::PAGE_SIZE, Ordering::Relaxed);
        } else {
            FRAME_ALLOC_UNINIT_CALLS.fetch_add(1, Ordering::Relaxed);
            FRAME_ALLOC_UNINIT_SAVED_BYTES.fetch_add(crate::config::PAGE_SIZE, Ordering::Relaxed);
        }
    }

    pub(crate) fn record_frame_dealloc(
        released: bool,
        refcount_drop: bool,
        recycled_scan_slots: usize,
        recycled_len: usize,
    ) {
        FRAME_DEALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        if released {
            FRAME_DEALLOC_RELEASED.fetch_add(1, Ordering::Relaxed);
        }
        if refcount_drop {
            FRAME_DEALLOC_REFCOUNT_DROPS.fetch_add(1, Ordering::Relaxed);
        }
        FRAME_DEALLOC_RECYCLED_SCAN_SLOTS.fetch_add(recycled_scan_slots, Ordering::Relaxed);
        update_max(&FRAME_DEALLOC_RECYCLED_LEN_MAX, recycled_len);
    }

    pub(crate) fn record_dev_zero_read(bytes: usize, byte_writes: usize, fill_bytes: usize) {
        DEV_ZERO_READ_CALLS.fetch_add(1, Ordering::Relaxed);
        DEV_ZERO_READ_BYTES.fetch_add(bytes, Ordering::Relaxed);
        DEV_ZERO_READ_BYTE_WRITES.fetch_add(byte_writes, Ordering::Relaxed);
        DEV_ZERO_READ_FILL_BYTES.fetch_add(fill_bytes, Ordering::Relaxed);
    }

    pub(crate) fn record_dev_random_read(bytes: usize, byte_writes: usize, word_fill_bytes: usize) {
        DEV_RANDOM_READ_CALLS.fetch_add(1, Ordering::Relaxed);
        DEV_RANDOM_READ_BYTES.fetch_add(bytes, Ordering::Relaxed);
        DEV_RANDOM_BYTE_WRITES.fetch_add(byte_writes, Ordering::Relaxed);
        DEV_RANDOM_WORD_FILL_BYTES.fetch_add(word_fill_bytes, Ordering::Relaxed);
    }

    pub(crate) fn record_uart_write(bytes: usize) {
        UART_WRITE_LOCK_CALLS.fetch_add(1, Ordering::Relaxed);
        UART_WRITE_BYTES.fetch_add(bytes, Ordering::Relaxed);
    }

    pub(crate) fn record_tlb_flush_all() {
        TLB_FLUSH_ALL_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_tlb_flush_range(pages: usize) {
        TLB_FLUSH_RANGE_CALLS.fetch_add(1, Ordering::Relaxed);
        TLB_FLUSH_RANGE_PAGES.fetch_add(pages, Ordering::Relaxed);
    }

    pub(crate) fn record_mount_metadata(source_len: usize) {
        MOUNT_METADATA_CALLS.fetch_add(1, Ordering::Relaxed);
        MOUNT_METADATA_SOURCE_CLONE_BYTES.fetch_add(source_len, Ordering::Relaxed);
    }

    pub(crate) fn record_mount_fast_stat_flags() {
        MOUNT_FAST_STAT_FLAGS_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_mount_fast_fs_type() {
        MOUNT_FAST_FS_TYPE_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_eventfd_read_call() {
        EVENTFD_READ_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_eventfd_write_call() {
        EVENTFD_WRITE_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_eventfd_reader_sleep() {
        EVENTFD_READER_SLEEPS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_eventfd_writer_sleep() {
        EVENTFD_WRITER_SLEEPS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_eventfd_reader_wakeup() {
        EVENTFD_READER_WAKEUPS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_eventfd_writer_wakeup() {
        EVENTFD_WRITER_WAKEUPS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_local_socket_read_call() {
        LOCAL_SOCKET_READ_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_local_socket_write_call() {
        LOCAL_SOCKET_WRITE_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_local_socket_reader_sleep() {
        LOCAL_SOCKET_READER_SLEEPS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_local_socket_writer_sleep() {
        LOCAL_SOCKET_WRITER_SLEEPS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_local_socket_reader_wakeup() {
        LOCAL_SOCKET_READER_WAKEUPS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_local_socket_writer_wakeup() {
        LOCAL_SOCKET_WRITER_WAKEUPS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_tid_lookup(
        process_visits: usize,
        task_visits: usize,
        hit: bool,
        index_hit: bool,
        stale_index_entry: bool,
    ) {
        TID_LOOKUP_CALLS.fetch_add(1, Ordering::Relaxed);
        TID_LOOKUP_PROCESS_VISITS.fetch_add(process_visits, Ordering::Relaxed);
        TID_LOOKUP_TASK_VISITS.fetch_add(task_visits, Ordering::Relaxed);
        if hit {
            TID_LOOKUP_HITS.fetch_add(1, Ordering::Relaxed);
        }
        if index_hit {
            TID_LOOKUP_INDEX_HITS.fetch_add(1, Ordering::Relaxed);
        }
        if stale_index_entry {
            TID_LOOKUP_STALE_INDEX_ENTRIES.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn record_exec_stack_copy(bytes: usize) {
        EXEC_STACK_COPY_CALLS.fetch_add(1, Ordering::Relaxed);
        EXEC_STACK_COPY_BYTES.fetch_add(bytes, Ordering::Relaxed);
    }

    pub(crate) fn record_wait_child_scan(child_slots: usize) {
        WAIT_CHILD_SCAN_PASSES.fetch_add(1, Ordering::Relaxed);
        WAIT_CHILD_SCAN_SLOTS.fetch_add(child_slots, Ordering::Relaxed);
    }

    pub(crate) fn record_scheduler_normal_requeue(vruntime_delta: usize) {
        SCHEDULER_NORMAL_REQUEUE_CALLS.fetch_add(1, Ordering::Relaxed);
        SCHEDULER_NORMAL_VRUNTIME_DELTA.fetch_add(vruntime_delta, Ordering::Relaxed);
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

    pub(crate) fn record_vma_range_scan(area_visits: usize, index_skips: usize) {
        VMA_RANGE_SCANS.fetch_add(1, Ordering::Relaxed);
        VMA_RANGE_AREA_VISITS.fetch_add(area_visits, Ordering::Relaxed);
        VMA_RANGE_INDEX_SKIPS.fetch_add(index_skips, Ordering::Relaxed);
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

    pub(crate) fn record_usercopy_leaf_pte_cache_hit() {
        USERCOPY_LEAF_PTE_CACHE_HITS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_usercopy_leaf_pte_cache_miss() {
        USERCOPY_LEAF_PTE_CACHE_MISSES.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_usercopy_leaf_pte_cache_invalidation() {
        USERCOPY_LEAF_PTE_CACHE_INVALIDATIONS.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_usercopy_slow_path(page_count: usize) {
        USERCOPY_SLOW_PATHS.fetch_add(1, Ordering::Relaxed);
        USERCOPY_SLOW_PAGES.fetch_add(page_count, Ordering::Relaxed);
    }

    pub(crate) fn record_usercopy_checked_range(pages: usize, bytes: usize) {
        USERCOPY_CHECKED_RANGE_CALLS.fetch_add(1, Ordering::Relaxed);
        USERCOPY_CHECKED_RANGE_PAGES.fetch_add(pages, Ordering::Relaxed);
        USERCOPY_CHECKED_RANGE_BYTES.fetch_add(bytes, Ordering::Relaxed);
    }

    pub(crate) fn record_usercopy_range_reuse(chunks: usize, pages: usize, bytes: usize) {
        USERCOPY_RANGE_REUSE_HITS.fetch_add(chunks, Ordering::Relaxed);
        USERCOPY_RANGE_REUSE_PAGES.fetch_add(pages, Ordering::Relaxed);
        USERCOPY_RANGE_REUSE_BYTES.fetch_add(bytes, Ordering::Relaxed);
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

    pub(crate) fn record_futex_wake(key_hit: bool, tasks: usize) {
        FUTEX_WAKE_CALLS.fetch_add(1, Ordering::Relaxed);
        if key_hit {
            FUTEX_WAKE_KEY_HITS.fetch_add(1, Ordering::Relaxed);
        }
        FUTEX_WAKE_TASKS.fetch_add(tasks, Ordering::Relaxed);
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
            syscall_dispatch_calls: SYSCALL_DISPATCH_CALLS.load(Ordering::Relaxed),
            syscall_identity_fast_paths: SYSCALL_IDENTITY_FAST_PATHS.load(Ordering::Relaxed),
            task_current_calls: TASK_CURRENT_CALLS.load(Ordering::Relaxed),
            task_current_process_calls: TASK_CURRENT_PROCESS_CALLS.load(Ordering::Relaxed),
            task_current_user_token_calls: TASK_CURRENT_USER_TOKEN_CALLS.load(Ordering::Relaxed),
            task_current_trap_cx_calls: TASK_CURRENT_TRAP_CX_CALLS.load(Ordering::Relaxed),
            task_current_trap_cx_user_va_calls: TASK_CURRENT_TRAP_CX_USER_VA_CALLS
                .load(Ordering::Relaxed),
            task_current_trap_return_context_calls: TASK_CURRENT_TRAP_RETURN_CONTEXT_CALLS
                .load(Ordering::Relaxed),
            signal_action_table_lock_calls: SIGNAL_ACTION_TABLE_LOCK_CALLS.load(Ordering::Relaxed),
            time_nanos_to_timespec_calls: TIME_NANOS_TO_TIMESPEC_CALLS.load(Ordering::Relaxed),
            time_direct_timespec_calls: TIME_DIRECT_TIMESPEC_CALLS.load(Ordering::Relaxed),
            riscv_return_fence_i_calls: RISCV_RETURN_FENCE_I_CALLS.load(Ordering::Relaxed),
            la_return_invtlb_calls: LA_RETURN_INVTLB_CALLS.load(Ordering::Relaxed),
            rv_user_fp_save_calls: RV_USER_FP_SAVE_CALLS.load(Ordering::Relaxed),
            rv_user_fp_restore_calls: RV_USER_FP_RESTORE_CALLS.load(Ordering::Relaxed),
            rv_user_fp_lazy_init_traps: RV_USER_FP_LAZY_INIT_TRAPS.load(Ordering::Relaxed),
            arch_instruction_barrier_calls: ARCH_INSTRUCTION_BARRIER_CALLS.load(Ordering::Relaxed),
            tid_lookup_calls: TID_LOOKUP_CALLS.load(Ordering::Relaxed),
            tid_lookup_process_visits: TID_LOOKUP_PROCESS_VISITS.load(Ordering::Relaxed),
            tid_lookup_task_visits: TID_LOOKUP_TASK_VISITS.load(Ordering::Relaxed),
            tid_lookup_hits: TID_LOOKUP_HITS.load(Ordering::Relaxed),
            tid_lookup_index_hits: TID_LOOKUP_INDEX_HITS.load(Ordering::Relaxed),
            tid_lookup_stale_index_entries: TID_LOOKUP_STALE_INDEX_ENTRIES.load(Ordering::Relaxed),
            exec_stack_copy_calls: EXEC_STACK_COPY_CALLS.load(Ordering::Relaxed),
            exec_stack_copy_bytes: EXEC_STACK_COPY_BYTES.load(Ordering::Relaxed),
            wait_child_scan_passes: WAIT_CHILD_SCAN_PASSES.load(Ordering::Relaxed),
            wait_child_scan_slots: WAIT_CHILD_SCAN_SLOTS.load(Ordering::Relaxed),
            scheduler_normal_requeue_calls: SCHEDULER_NORMAL_REQUEUE_CALLS.load(Ordering::Relaxed),
            scheduler_normal_vruntime_delta: SCHEDULER_NORMAL_VRUNTIME_DELTA
                .load(Ordering::Relaxed),
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
            epoll_ready_list_checks: EPOLL_READY_LIST_CHECKS.load(Ordering::Relaxed),
            epoll_ready_list_source_visits: EPOLL_READY_LIST_SOURCE_VISITS.load(Ordering::Relaxed),
            epoll_ready_list_hits: EPOLL_READY_LIST_HITS.load(Ordering::Relaxed),
            epoll_backoff_sleeps: EPOLL_BACKOFF_SLEEPS.load(Ordering::Relaxed),
            epoll_backoff_us: EPOLL_BACKOFF_US.load(Ordering::Relaxed),
            epoll_waiter_registrations: EPOLL_WAITER_REGISTRATIONS.load(Ordering::Relaxed),
            epoll_waiter_sleeps: EPOLL_WAITER_SLEEPS.load(Ordering::Relaxed),
            poll_wait_scans: POLL_WAIT_SCANS.load(Ordering::Relaxed),
            poll_wait_fd_visits: POLL_WAIT_FD_VISITS.load(Ordering::Relaxed),
            poll_wait_ready_events: POLL_WAIT_READY_EVENTS.load(Ordering::Relaxed),
            poll_backoff_sleeps: POLL_BACKOFF_SLEEPS.load(Ordering::Relaxed),
            poll_backoff_ms: POLL_BACKOFF_MS.load(Ordering::Relaxed),
            poll_fd_table_lookups: POLL_FD_TABLE_LOOKUPS.load(Ordering::Relaxed),
            vfs_read_cache_hits: VFS_READ_CACHE_HITS.load(Ordering::Relaxed),
            vfs_read_cache_misses: VFS_READ_CACHE_MISSES.load(Ordering::Relaxed),
            vfs_read_cache_bytes: VFS_READ_CACHE_BYTES.load(Ordering::Relaxed),
            vfs_read_cache_backend_reads: VFS_READ_CACHE_BACKEND_READS.load(Ordering::Relaxed),
            vfs_read_cache_invalidated_pages: VFS_READ_CACHE_INVALIDATED_PAGES
                .load(Ordering::Relaxed),
            vfs_read_cache_invalidation_calls: VFS_READ_CACHE_INVALIDATION_CALLS
                .load(Ordering::Relaxed),
            vfs_read_cache_invalidation_scan_pages: VFS_READ_CACHE_INVALIDATION_SCAN_PAGES
                .load(Ordering::Relaxed),
            vfs_read_cache_readahead_batches: VFS_READ_CACHE_READAHEAD_BATCHES
                .load(Ordering::Relaxed),
            vfs_read_cache_readahead_pages: VFS_READ_CACHE_READAHEAD_PAGES.load(Ordering::Relaxed),
            vfs_read_cache_eligible_calls: VFS_READ_CACHE_ELIGIBLE_CALLS.load(Ordering::Relaxed),
            vfs_read_cache_skip_too_large: VFS_READ_CACHE_SKIP_TOO_LARGE.load(Ordering::Relaxed),
            vfs_read_cache_skip_dirty_pages: VFS_READ_CACHE_SKIP_DIRTY_PAGES
                .load(Ordering::Relaxed),
            vfs_read_all_calls: VFS_READ_ALL_CALLS.load(Ordering::Relaxed),
            vfs_read_all_backend_reads: VFS_READ_ALL_BACKEND_READS.load(Ordering::Relaxed),
            vfs_read_all_bytes: VFS_READ_ALL_BYTES.load(Ordering::Relaxed),
            vfs_read_all_max_chunk: VFS_READ_ALL_MAX_CHUNK.load(Ordering::Relaxed),
            vfs_read_backend_calls: VFS_READ_BACKEND_CALLS.load(Ordering::Relaxed),
            vfs_read_backend_bytes: VFS_READ_BACKEND_BYTES.load(Ordering::Relaxed),
            vfs_read_backend_max_chunk: VFS_READ_BACKEND_MAX_CHUNK.load(Ordering::Relaxed),
            vfs_read_coalesced_calls: VFS_READ_COALESCED_CALLS.load(Ordering::Relaxed),
            vfs_read_coalesced_bytes: VFS_READ_COALESCED_BYTES.load(Ordering::Relaxed),
            vfs_path_component_scans: VFS_PATH_COMPONENT_SCANS.load(Ordering::Relaxed),
            vfs_path_components: VFS_PATH_COMPONENTS.load(Ordering::Relaxed),
            vfs_path_component_allocs: VFS_PATH_COMPONENT_ALLOCS.load(Ordering::Relaxed),
            vfs_visible_path_updates: VFS_VISIBLE_PATH_UPDATES.load(Ordering::Relaxed),
            vfs_visible_path_allocs: VFS_VISIBLE_PATH_ALLOCS.load(Ordering::Relaxed),
            vfs_parent_cursor_clones: VFS_PARENT_CURSOR_CLONES.load(Ordering::Relaxed),
            vfs_dirent_read_calls: VFS_DIRENT_READ_CALLS.load(Ordering::Relaxed),
            vfs_dirent_user_buffer_bytes: VFS_DIRENT_USER_BUFFER_BYTES.load(Ordering::Relaxed),
            vfs_dirent_scratch_bytes: VFS_DIRENT_SCRATCH_BYTES.load(Ordering::Relaxed),
            vfs_dirent_returned_bytes: VFS_DIRENT_RETURNED_BYTES.load(Ordering::Relaxed),
            vfs_dirent_max_scratch_bytes: VFS_DIRENT_MAX_SCRATCH_BYTES.load(Ordering::Relaxed),
            ext4_dirent_entries: EXT4_DIRENT_ENTRIES.load(Ordering::Relaxed),
            ext4_dirent_name_bytes: EXT4_DIRENT_NAME_BYTES.load(Ordering::Relaxed),
            ext4_dirent_name_allocs: EXT4_DIRENT_NAME_ALLOCS.load(Ordering::Relaxed),
            ext4_dirent_name_alloc_bytes: EXT4_DIRENT_NAME_ALLOC_BYTES.load(Ordering::Relaxed),
            procfs_content_builds: PROCFS_CONTENT_BUILDS.load(Ordering::Relaxed),
            procfs_content_bytes: PROCFS_CONTENT_BYTES.load(Ordering::Relaxed),
            procfs_snapshot_hits: PROCFS_SNAPSHOT_HITS.load(Ordering::Relaxed),
            procfs_snapshot_hit_bytes: PROCFS_SNAPSHOT_HIT_BYTES.load(Ordering::Relaxed),
            vfs_write_user_buffer_calls: VFS_WRITE_USER_BUFFER_CALLS.load(Ordering::Relaxed),
            vfs_write_user_buffer_slices: VFS_WRITE_USER_BUFFER_SLICES.load(Ordering::Relaxed),
            vfs_write_backend_calls: VFS_WRITE_BACKEND_CALLS.load(Ordering::Relaxed),
            vfs_write_backend_bytes: VFS_WRITE_BACKEND_BYTES.load(Ordering::Relaxed),
            vfs_write_coalesced_calls: VFS_WRITE_COALESCED_CALLS.load(Ordering::Relaxed),
            vfs_write_coalesced_bytes: VFS_WRITE_COALESCED_BYTES.load(Ordering::Relaxed),
            page_cache_clean_evictions: PAGE_CACHE_CLEAN_EVICTIONS.load(Ordering::Relaxed),
            tmpfs_allocated_payload_len_calls: TMPFS_ALLOCATED_PAYLOAD_LEN_CALLS
                .load(Ordering::Relaxed),
            tmpfs_allocated_payload_sparse_extents: TMPFS_ALLOCATED_PAYLOAD_SPARSE_EXTENTS
                .load(Ordering::Relaxed),
            tmpfs_allocated_logical_len_calls: TMPFS_ALLOCATED_LOGICAL_LEN_CALLS
                .load(Ordering::Relaxed),
            tmpfs_allocated_logical_sparse_extents: TMPFS_ALLOCATED_LOGICAL_SPARSE_EXTENTS
                .load(Ordering::Relaxed),
            frame_alloc_zeroed_calls: FRAME_ALLOC_ZEROED_CALLS.load(Ordering::Relaxed),
            frame_alloc_zeroed_bytes: FRAME_ALLOC_ZEROED_BYTES.load(Ordering::Relaxed),
            frame_alloc_uninit_calls: FRAME_ALLOC_UNINIT_CALLS.load(Ordering::Relaxed),
            frame_alloc_uninit_saved_bytes: FRAME_ALLOC_UNINIT_SAVED_BYTES.load(Ordering::Relaxed),
            frame_dealloc_calls: FRAME_DEALLOC_CALLS.load(Ordering::Relaxed),
            frame_dealloc_released: FRAME_DEALLOC_RELEASED.load(Ordering::Relaxed),
            frame_dealloc_refcount_drops: FRAME_DEALLOC_REFCOUNT_DROPS.load(Ordering::Relaxed),
            frame_dealloc_recycled_scan_slots: FRAME_DEALLOC_RECYCLED_SCAN_SLOTS
                .load(Ordering::Relaxed),
            frame_dealloc_recycled_len_max: FRAME_DEALLOC_RECYCLED_LEN_MAX.load(Ordering::Relaxed),
            dev_zero_read_calls: DEV_ZERO_READ_CALLS.load(Ordering::Relaxed),
            dev_zero_read_bytes: DEV_ZERO_READ_BYTES.load(Ordering::Relaxed),
            dev_zero_read_byte_writes: DEV_ZERO_READ_BYTE_WRITES.load(Ordering::Relaxed),
            dev_zero_read_fill_bytes: DEV_ZERO_READ_FILL_BYTES.load(Ordering::Relaxed),
            dev_random_read_calls: DEV_RANDOM_READ_CALLS.load(Ordering::Relaxed),
            dev_random_read_bytes: DEV_RANDOM_READ_BYTES.load(Ordering::Relaxed),
            dev_random_byte_writes: DEV_RANDOM_BYTE_WRITES.load(Ordering::Relaxed),
            dev_random_word_fill_bytes: DEV_RANDOM_WORD_FILL_BYTES.load(Ordering::Relaxed),
            uart_write_lock_calls: UART_WRITE_LOCK_CALLS.load(Ordering::Relaxed),
            uart_write_bytes: UART_WRITE_BYTES.load(Ordering::Relaxed),
            tlb_flush_all_calls: TLB_FLUSH_ALL_CALLS.load(Ordering::Relaxed),
            tlb_flush_range_calls: TLB_FLUSH_RANGE_CALLS.load(Ordering::Relaxed),
            tlb_flush_range_pages: TLB_FLUSH_RANGE_PAGES.load(Ordering::Relaxed),
            mount_metadata_calls: MOUNT_METADATA_CALLS.load(Ordering::Relaxed),
            mount_metadata_source_clone_bytes: MOUNT_METADATA_SOURCE_CLONE_BYTES
                .load(Ordering::Relaxed),
            mount_fast_stat_flags_calls: MOUNT_FAST_STAT_FLAGS_CALLS.load(Ordering::Relaxed),
            mount_fast_fs_type_calls: MOUNT_FAST_FS_TYPE_CALLS.load(Ordering::Relaxed),
            eventfd_read_calls: EVENTFD_READ_CALLS.load(Ordering::Relaxed),
            eventfd_write_calls: EVENTFD_WRITE_CALLS.load(Ordering::Relaxed),
            eventfd_read_block_yields: EVENTFD_READ_BLOCK_YIELDS.load(Ordering::Relaxed),
            eventfd_write_block_yields: EVENTFD_WRITE_BLOCK_YIELDS.load(Ordering::Relaxed),
            eventfd_reader_sleeps: EVENTFD_READER_SLEEPS.load(Ordering::Relaxed),
            eventfd_writer_sleeps: EVENTFD_WRITER_SLEEPS.load(Ordering::Relaxed),
            eventfd_reader_wakeups: EVENTFD_READER_WAKEUPS.load(Ordering::Relaxed),
            eventfd_writer_wakeups: EVENTFD_WRITER_WAKEUPS.load(Ordering::Relaxed),
            local_socket_read_calls: LOCAL_SOCKET_READ_CALLS.load(Ordering::Relaxed),
            local_socket_write_calls: LOCAL_SOCKET_WRITE_CALLS.load(Ordering::Relaxed),
            local_socket_read_block_yields: LOCAL_SOCKET_READ_BLOCK_YIELDS.load(Ordering::Relaxed),
            local_socket_write_block_yields: LOCAL_SOCKET_WRITE_BLOCK_YIELDS
                .load(Ordering::Relaxed),
            local_socket_reader_sleeps: LOCAL_SOCKET_READER_SLEEPS.load(Ordering::Relaxed),
            local_socket_writer_sleeps: LOCAL_SOCKET_WRITER_SLEEPS.load(Ordering::Relaxed),
            local_socket_reader_wakeups: LOCAL_SOCKET_READER_WAKEUPS.load(Ordering::Relaxed),
            local_socket_writer_wakeups: LOCAL_SOCKET_WRITER_WAKEUPS.load(Ordering::Relaxed),
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
            vma_range_scans: VMA_RANGE_SCANS.load(Ordering::Relaxed),
            vma_range_area_visits: VMA_RANGE_AREA_VISITS.load(Ordering::Relaxed),
            vma_range_index_skips: VMA_RANGE_INDEX_SKIPS.load(Ordering::Relaxed),
            user_c_string_calls: USER_C_STRING_CALLS.load(Ordering::Relaxed),
            user_c_string_page_chunks: USER_C_STRING_PAGE_CHUNKS.load(Ordering::Relaxed),
            user_c_string_scanned_bytes: USER_C_STRING_SCANNED_BYTES.load(Ordering::Relaxed),
            user_c_string_ascii_fast_bytes: USER_C_STRING_ASCII_FAST_BYTES.load(Ordering::Relaxed),
            user_c_string_fallback_bytes: USER_C_STRING_FALLBACK_BYTES.load(Ordering::Relaxed),
            usercopy_same_page_read_hits: USERCOPY_SAME_PAGE_READ_HITS.load(Ordering::Relaxed),
            usercopy_same_page_write_hits: USERCOPY_SAME_PAGE_WRITE_HITS.load(Ordering::Relaxed),
            usercopy_same_page_fast_bytes: USERCOPY_SAME_PAGE_FAST_BYTES.load(Ordering::Relaxed),
            usercopy_leaf_pte_cache_hits: USERCOPY_LEAF_PTE_CACHE_HITS.load(Ordering::Relaxed),
            usercopy_leaf_pte_cache_misses: USERCOPY_LEAF_PTE_CACHE_MISSES.load(Ordering::Relaxed),
            usercopy_leaf_pte_cache_invalidations: USERCOPY_LEAF_PTE_CACHE_INVALIDATIONS
                .load(Ordering::Relaxed),
            usercopy_slow_paths: USERCOPY_SLOW_PATHS.load(Ordering::Relaxed),
            usercopy_slow_pages: USERCOPY_SLOW_PAGES.load(Ordering::Relaxed),
            usercopy_checked_range_calls: USERCOPY_CHECKED_RANGE_CALLS.load(Ordering::Relaxed),
            usercopy_checked_range_pages: USERCOPY_CHECKED_RANGE_PAGES.load(Ordering::Relaxed),
            usercopy_checked_range_bytes: USERCOPY_CHECKED_RANGE_BYTES.load(Ordering::Relaxed),
            usercopy_range_reuse_hits: USERCOPY_RANGE_REUSE_HITS.load(Ordering::Relaxed),
            usercopy_range_reuse_pages: USERCOPY_RANGE_REUSE_PAGES.load(Ordering::Relaxed),
            usercopy_range_reuse_bytes: USERCOPY_RANGE_REUSE_BYTES.load(Ordering::Relaxed),
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
            futex_wake_calls: FUTEX_WAKE_CALLS.load(Ordering::Relaxed),
            futex_wake_key_hits: FUTEX_WAKE_KEY_HITS.load(Ordering::Relaxed),
            futex_wake_tasks: FUTEX_WAKE_TASKS.load(Ordering::Relaxed),
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
         syscall_dispatch_calls {}\n\
         syscall_identity_fast_paths {}\n\
         task_current_calls {}\n\
         task_current_process_calls {}\n\
         task_current_user_token_calls {}\n\
         task_current_trap_cx_calls {}\n\
         task_current_trap_cx_user_va_calls {}\n\
         task_current_trap_return_context_calls {}\n\
         signal_action_table_lock_calls {}\n\
         time_nanos_to_timespec_calls {}\n\
         time_direct_timespec_calls {}\n\
         riscv_return_fence_i_calls {}\n\
         la_return_invtlb_calls {}\n\
         rv_user_fp_save_calls {}\n\
         rv_user_fp_restore_calls {}\n\
         rv_user_fp_lazy_init_traps {}\n\
         arch_instruction_barrier_calls {}\n\
         tid_lookup_calls {}\n\
         tid_lookup_process_visits {}\n\
         tid_lookup_task_visits {}\n\
         tid_lookup_hits {}\n\
         tid_lookup_index_hits {}\n\
         tid_lookup_stale_index_entries {}\n\
         exec_stack_copy_calls {}\n\
         exec_stack_copy_bytes {}\n\
         wait_child_scan_passes {}\n\
         wait_child_scan_slots {}\n\
         scheduler_normal_requeue_calls {}\n\
         scheduler_normal_vruntime_delta {}\n\
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
         epoll_ready_list_checks {}\n\
         epoll_ready_list_source_visits {}\n\
         epoll_ready_list_hits {}\n\
         epoll_backoff_sleeps {}\n\
         epoll_backoff_us {}\n\
         epoll_waiter_registrations {}\n\
         epoll_waiter_sleeps {}\n\
         poll_wait_scans {}\n\
         poll_wait_fd_visits {}\n\
         poll_wait_ready_events {}\n\
         poll_backoff_sleeps {}\n\
         poll_backoff_ms {}\n\
         poll_fd_table_lookups {}\n\
         vfs_read_cache_hits {}\n\
         vfs_read_cache_misses {}\n\
         vfs_read_cache_bytes {}\n\
         vfs_read_cache_backend_reads {}\n\
         vfs_read_cache_invalidated_pages {}\n\
         vfs_read_cache_invalidation_calls {}\n\
         vfs_read_cache_invalidation_scan_pages {}\n\
         vfs_read_cache_readahead_batches {}\n\
         vfs_read_cache_readahead_pages {}\n\
         vfs_read_cache_eligible_calls {}\n\
         vfs_read_cache_skip_too_large {}\n\
         vfs_read_cache_skip_dirty_pages {}\n\
         vfs_read_all_calls {}\n\
         vfs_read_all_backend_reads {}\n\
         vfs_read_all_bytes {}\n\
         vfs_read_all_max_chunk {}\n\
         vfs_read_backend_calls {}\n\
         vfs_read_backend_bytes {}\n\
         vfs_read_backend_max_chunk {}\n\
         vfs_read_coalesced_calls {}\n\
         vfs_read_coalesced_bytes {}\n\
         vfs_path_component_scans {}\n\
         vfs_path_components {}\n\
         vfs_path_component_allocs {}\n\
         vfs_visible_path_updates {}\n\
         vfs_visible_path_allocs {}\n\
         vfs_parent_cursor_clones {}\n\
         vfs_dirent_read_calls {}\n\
         vfs_dirent_user_buffer_bytes {}\n\
         vfs_dirent_scratch_bytes {}\n\
         vfs_dirent_returned_bytes {}\n\
         vfs_dirent_max_scratch_bytes {}\n\
         ext4_dirent_entries {}\n\
         ext4_dirent_name_bytes {}\n\
         ext4_dirent_name_allocs {}\n\
         ext4_dirent_name_alloc_bytes {}\n\
         procfs_content_builds {}\n\
         procfs_content_bytes {}\n\
         procfs_snapshot_hits {}\n\
         procfs_snapshot_hit_bytes {}\n\
         vfs_write_user_buffer_calls {}\n\
         vfs_write_user_buffer_slices {}\n\
         vfs_write_backend_calls {}\n\
         vfs_write_backend_bytes {}\n\
         vfs_write_coalesced_calls {}\n\
         vfs_write_coalesced_bytes {}\n\
         page_cache_clean_evictions {}\n\
         tmpfs_allocated_payload_len_calls {}\n\
         tmpfs_allocated_payload_sparse_extents {}\n\
         tmpfs_allocated_logical_len_calls {}\n\
         tmpfs_allocated_logical_sparse_extents {}\n\
         frame_alloc_zeroed_calls {}\n\
         frame_alloc_zeroed_bytes {}\n\
         frame_alloc_uninit_calls {}\n\
         frame_alloc_uninit_saved_bytes {}\n\
         frame_dealloc_calls {}\n\
         frame_dealloc_released {}\n\
         frame_dealloc_refcount_drops {}\n\
         frame_dealloc_recycled_scan_slots {}\n\
         frame_dealloc_recycled_len_max {}\n\
         dev_zero_read_calls {}\n\
         dev_zero_read_bytes {}\n\
         dev_zero_read_byte_writes {}\n\
         dev_zero_read_fill_bytes {}\n\
         dev_random_read_calls {}\n\
         dev_random_read_bytes {}\n\
         dev_random_byte_writes {}\n\
         dev_random_word_fill_bytes {}\n\
         uart_write_lock_calls {}\n\
         uart_write_bytes {}\n\
         tlb_flush_all_calls {}\n\
         tlb_flush_range_calls {}\n\
         tlb_flush_range_pages {}\n\
         mount_metadata_calls {}\n\
         mount_metadata_source_clone_bytes {}\n\
         mount_fast_stat_flags_calls {}\n\
         mount_fast_fs_type_calls {}\n\
         eventfd_read_calls {}\n\
         eventfd_write_calls {}\n\
         eventfd_read_block_yields {}\n\
         eventfd_write_block_yields {}\n\
         eventfd_reader_sleeps {}\n\
         eventfd_writer_sleeps {}\n\
         eventfd_reader_wakeups {}\n\
         eventfd_writer_wakeups {}\n\
         local_socket_read_calls {}\n\
         local_socket_write_calls {}\n\
         local_socket_read_block_yields {}\n\
         local_socket_write_block_yields {}\n\
         local_socket_reader_sleeps {}\n\
         local_socket_writer_sleeps {}\n\
         local_socket_reader_wakeups {}\n\
         local_socket_writer_wakeups {}\n\
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
         vma_range_scans {}\n\
         vma_range_area_visits {}\n\
         vma_range_index_skips {}\n\
         user_c_string_calls {}\n\
         user_c_string_page_chunks {}\n\
         user_c_string_scanned_bytes {}\n\
         user_c_string_ascii_fast_bytes {}\n\
         user_c_string_fallback_bytes {}\n\
         usercopy_same_page_read_hits {}\n\
         usercopy_same_page_write_hits {}\n\
         usercopy_same_page_fast_bytes {}\n\
         usercopy_leaf_pte_cache_hits {}\n\
         usercopy_leaf_pte_cache_misses {}\n\
         usercopy_leaf_pte_cache_invalidations {}\n\
         usercopy_slow_paths {}\n\
         usercopy_slow_pages {}\n\
         usercopy_checked_range_calls {}\n\
         usercopy_checked_range_pages {}\n\
         usercopy_checked_range_bytes {}\n\
         usercopy_range_reuse_hits {}\n\
         usercopy_range_reuse_pages {}\n\
         usercopy_range_reuse_bytes {}\n\
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
         futex_wake_calls {}\n\
         futex_wake_key_hits {}\n\
         futex_wake_tasks {}\n\
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
            stats.syscall_dispatch_calls,
            stats.syscall_identity_fast_paths,
            stats.task_current_calls,
            stats.task_current_process_calls,
            stats.task_current_user_token_calls,
            stats.task_current_trap_cx_calls,
            stats.task_current_trap_cx_user_va_calls,
            stats.task_current_trap_return_context_calls,
            stats.signal_action_table_lock_calls,
            stats.time_nanos_to_timespec_calls,
            stats.time_direct_timespec_calls,
            stats.riscv_return_fence_i_calls,
            stats.la_return_invtlb_calls,
            stats.rv_user_fp_save_calls,
            stats.rv_user_fp_restore_calls,
            stats.rv_user_fp_lazy_init_traps,
            stats.arch_instruction_barrier_calls,
            stats.tid_lookup_calls,
            stats.tid_lookup_process_visits,
            stats.tid_lookup_task_visits,
            stats.tid_lookup_hits,
            stats.tid_lookup_index_hits,
            stats.tid_lookup_stale_index_entries,
            stats.exec_stack_copy_calls,
            stats.exec_stack_copy_bytes,
            stats.wait_child_scan_passes,
            stats.wait_child_scan_slots,
            stats.scheduler_normal_requeue_calls,
            stats.scheduler_normal_vruntime_delta,
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
            stats.epoll_ready_list_checks,
            stats.epoll_ready_list_source_visits,
            stats.epoll_ready_list_hits,
            stats.epoll_backoff_sleeps,
            stats.epoll_backoff_us,
            stats.epoll_waiter_registrations,
            stats.epoll_waiter_sleeps,
            stats.poll_wait_scans,
            stats.poll_wait_fd_visits,
            stats.poll_wait_ready_events,
            stats.poll_backoff_sleeps,
            stats.poll_backoff_ms,
            stats.poll_fd_table_lookups,
            stats.vfs_read_cache_hits,
            stats.vfs_read_cache_misses,
            stats.vfs_read_cache_bytes,
            stats.vfs_read_cache_backend_reads,
            stats.vfs_read_cache_invalidated_pages,
            stats.vfs_read_cache_invalidation_calls,
            stats.vfs_read_cache_invalidation_scan_pages,
            stats.vfs_read_cache_readahead_batches,
            stats.vfs_read_cache_readahead_pages,
            stats.vfs_read_cache_eligible_calls,
            stats.vfs_read_cache_skip_too_large,
            stats.vfs_read_cache_skip_dirty_pages,
            stats.vfs_read_all_calls,
            stats.vfs_read_all_backend_reads,
            stats.vfs_read_all_bytes,
            stats.vfs_read_all_max_chunk,
            stats.vfs_read_backend_calls,
            stats.vfs_read_backend_bytes,
            stats.vfs_read_backend_max_chunk,
            stats.vfs_read_coalesced_calls,
            stats.vfs_read_coalesced_bytes,
            stats.vfs_path_component_scans,
            stats.vfs_path_components,
            stats.vfs_path_component_allocs,
            stats.vfs_visible_path_updates,
            stats.vfs_visible_path_allocs,
            stats.vfs_parent_cursor_clones,
            stats.vfs_dirent_read_calls,
            stats.vfs_dirent_user_buffer_bytes,
            stats.vfs_dirent_scratch_bytes,
            stats.vfs_dirent_returned_bytes,
            stats.vfs_dirent_max_scratch_bytes,
            stats.ext4_dirent_entries,
            stats.ext4_dirent_name_bytes,
            stats.ext4_dirent_name_allocs,
            stats.ext4_dirent_name_alloc_bytes,
            stats.procfs_content_builds,
            stats.procfs_content_bytes,
            stats.procfs_snapshot_hits,
            stats.procfs_snapshot_hit_bytes,
            stats.vfs_write_user_buffer_calls,
            stats.vfs_write_user_buffer_slices,
            stats.vfs_write_backend_calls,
            stats.vfs_write_backend_bytes,
            stats.vfs_write_coalesced_calls,
            stats.vfs_write_coalesced_bytes,
            stats.page_cache_clean_evictions,
            stats.tmpfs_allocated_payload_len_calls,
            stats.tmpfs_allocated_payload_sparse_extents,
            stats.tmpfs_allocated_logical_len_calls,
            stats.tmpfs_allocated_logical_sparse_extents,
            stats.frame_alloc_zeroed_calls,
            stats.frame_alloc_zeroed_bytes,
            stats.frame_alloc_uninit_calls,
            stats.frame_alloc_uninit_saved_bytes,
            stats.frame_dealloc_calls,
            stats.frame_dealloc_released,
            stats.frame_dealloc_refcount_drops,
            stats.frame_dealloc_recycled_scan_slots,
            stats.frame_dealloc_recycled_len_max,
            stats.dev_zero_read_calls,
            stats.dev_zero_read_bytes,
            stats.dev_zero_read_byte_writes,
            stats.dev_zero_read_fill_bytes,
            stats.dev_random_read_calls,
            stats.dev_random_read_bytes,
            stats.dev_random_byte_writes,
            stats.dev_random_word_fill_bytes,
            stats.uart_write_lock_calls,
            stats.uart_write_bytes,
            stats.tlb_flush_all_calls,
            stats.tlb_flush_range_calls,
            stats.tlb_flush_range_pages,
            stats.mount_metadata_calls,
            stats.mount_metadata_source_clone_bytes,
            stats.mount_fast_stat_flags_calls,
            stats.mount_fast_fs_type_calls,
            stats.eventfd_read_calls,
            stats.eventfd_write_calls,
            stats.eventfd_read_block_yields,
            stats.eventfd_write_block_yields,
            stats.eventfd_reader_sleeps,
            stats.eventfd_writer_sleeps,
            stats.eventfd_reader_wakeups,
            stats.eventfd_writer_wakeups,
            stats.local_socket_read_calls,
            stats.local_socket_write_calls,
            stats.local_socket_read_block_yields,
            stats.local_socket_write_block_yields,
            stats.local_socket_reader_sleeps,
            stats.local_socket_writer_sleeps,
            stats.local_socket_reader_wakeups,
            stats.local_socket_writer_wakeups,
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
            stats.vma_range_scans,
            stats.vma_range_area_visits,
            stats.vma_range_index_skips,
            stats.user_c_string_calls,
            stats.user_c_string_page_chunks,
            stats.user_c_string_scanned_bytes,
            stats.user_c_string_ascii_fast_bytes,
            stats.user_c_string_fallback_bytes,
            stats.usercopy_same_page_read_hits,
            stats.usercopy_same_page_write_hits,
            stats.usercopy_same_page_fast_bytes,
            stats.usercopy_leaf_pte_cache_hits,
            stats.usercopy_leaf_pte_cache_misses,
            stats.usercopy_leaf_pte_cache_invalidations,
            stats.usercopy_slow_paths,
            stats.usercopy_slow_pages,
            stats.usercopy_checked_range_calls,
            stats.usercopy_checked_range_pages,
            stats.usercopy_checked_range_bytes,
            stats.usercopy_range_reuse_hits,
            stats.usercopy_range_reuse_pages,
            stats.usercopy_range_reuse_bytes,
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
            stats.futex_wake_calls,
            stats.futex_wake_key_hits,
            stats.futex_wake_tasks,
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
    pub(crate) fn record_syscall_dispatch_call() {}

    #[inline(always)]
    pub(crate) fn record_syscall_identity_fast_path() {}

    #[inline(always)]
    pub(crate) fn record_task_current_call() {}

    #[inline(always)]
    pub(crate) fn record_task_current_process_call() {}

    #[inline(always)]
    pub(crate) fn record_task_current_user_token_call() {}

    #[inline(always)]
    pub(crate) fn record_task_current_trap_cx_call() {}

    #[inline(always)]
    pub(crate) fn record_task_current_trap_return_context_call() {}

    #[inline(always)]
    pub(crate) fn record_signal_action_table_lock_call() {}

    #[inline(always)]
    pub(crate) fn record_time_nanos_to_timespec_call() {}

    #[inline(always)]
    pub(crate) fn record_time_direct_timespec_call() {}

    #[inline(always)]
    #[allow(dead_code)]
    pub(crate) fn record_riscv_return_fence_i_call() {}

    #[inline(always)]
    #[allow(dead_code)]
    pub(crate) fn record_la_return_invtlb_call() {}

    #[inline(always)]
    #[allow(dead_code)]
    pub(crate) fn record_rv_user_fp_save_call() {}

    #[inline(always)]
    #[allow(dead_code)]
    pub(crate) fn record_rv_user_fp_restore_call() {}

    #[inline(always)]
    #[allow(dead_code)]
    pub(crate) fn record_rv_user_fp_lazy_init_trap() {}

    #[inline(always)]
    pub(crate) fn record_arch_instruction_barrier_call() {}

    #[inline(always)]
    pub(crate) fn record_tid_lookup(
        _process_visits: usize,
        _task_visits: usize,
        _hit: bool,
        _index_hit: bool,
        _stale_index_entry: bool,
    ) {
    }

    #[inline(always)]
    pub(crate) fn record_exec_stack_copy(_bytes: usize) {}

    #[inline(always)]
    pub(crate) fn record_wait_child_scan(_child_slots: usize) {}

    #[inline(always)]
    pub(crate) fn record_scheduler_normal_requeue(_vruntime_delta: usize) {}

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
    pub(crate) fn record_epoll_ready_list(_source_visits: usize, _ready_events: usize) {}

    #[inline(always)]
    pub(crate) fn record_epoll_backoff_sleep(_duration_us: usize) {}

    #[inline(always)]
    pub(crate) fn record_epoll_waiter_registrations(_count: usize) {}

    #[inline(always)]
    pub(crate) fn record_epoll_waiter_sleep() {}

    #[inline(always)]
    pub(crate) fn record_poll_scan(_fd_visits: usize, _ready_events: usize) {}

    #[inline(always)]
    pub(crate) fn record_poll_fd_table_lookup() {}

    #[inline(always)]
    pub(crate) fn record_vfs_read_cache_hit(_bytes: usize) {}

    #[inline(always)]
    pub(crate) fn record_vfs_read_cache_miss() {}

    #[inline(always)]
    pub(crate) fn record_vfs_read_cache_backend_read() {}

    #[inline(always)]
    pub(crate) fn record_vfs_read_cache_invalidation(_pages: usize, _scanned_pages: usize) {}

    #[inline(always)]
    pub(crate) fn record_vfs_read_cache_readahead(_pages: usize) {}

    #[inline(always)]
    pub(crate) fn record_vfs_read_cache_eligible() {}

    #[inline(always)]
    pub(crate) fn record_vfs_read_cache_skip_too_large() {}

    #[inline(always)]
    pub(crate) fn record_vfs_read_cache_skip_dirty_pages() {}

    #[inline(always)]
    pub(crate) fn record_vfs_read_all_call() {}

    #[inline(always)]
    pub(crate) fn record_vfs_read_all_backend_read(_bytes: usize) {}

    #[inline(always)]
    pub(crate) fn record_vfs_read_backend(_bytes: usize) {}

    #[inline(always)]
    pub(crate) fn record_vfs_read_coalesced(_bytes: usize) {}

    #[inline(always)]
    pub(crate) fn record_vfs_path_components(_components: usize, _allocations: usize) {}

    #[inline(always)]
    pub(crate) fn record_vfs_visible_path_update(_allocations: usize) {}

    #[inline(always)]
    pub(crate) fn record_vfs_visible_path_allocation() {}

    #[inline(always)]
    pub(crate) fn record_vfs_dirent_read(
        _user_buffer_bytes: usize,
        _scratch_bytes: usize,
        _returned_bytes: usize,
    ) {
    }

    #[inline(always)]
    pub(crate) fn record_ext4_dirent_name(_name_len: usize, _allocated: bool) {}

    #[inline(always)]
    pub(crate) fn record_procfs_content_build(_bytes: usize) {}

    #[inline(always)]
    pub(crate) fn record_procfs_snapshot_hit(_bytes: usize) {}

    #[inline(always)]
    pub(crate) fn record_vfs_write_user_buffer(_slices: usize) {}

    #[inline(always)]
    pub(crate) fn record_vfs_write_backend(_bytes: usize) {}

    #[inline(always)]
    pub(crate) fn record_vfs_write_coalesced(_bytes: usize) {}

    #[inline(always)]
    pub(crate) fn record_page_cache_clean_eviction(_pages: usize) {}

    #[inline(always)]
    pub(crate) fn record_tmpfs_allocated_payload_len(_sparse_extents: usize) {}

    #[inline(always)]
    pub(crate) fn record_tmpfs_allocated_logical_len(_sparse_extents: usize) {}

    #[inline(always)]
    pub(crate) fn record_frame_alloc(_zeroed: bool) {}

    #[inline(always)]
    pub(crate) fn record_frame_dealloc(
        _released: bool,
        _refcount_drop: bool,
        _recycled_scan_slots: usize,
        _recycled_len: usize,
    ) {
    }

    #[inline(always)]
    pub(crate) fn record_dev_zero_read(_bytes: usize, _byte_writes: usize, _fill_bytes: usize) {}

    #[inline(always)]
    pub(crate) fn record_dev_random_read(
        _bytes: usize,
        _byte_writes: usize,
        _word_fill_bytes: usize,
    ) {
    }

    #[inline(always)]
    pub(crate) fn record_uart_write(_bytes: usize) {}

    #[inline(always)]
    pub(crate) fn record_tlb_flush_all() {}

    #[inline(always)]
    pub(crate) fn record_tlb_flush_range(_pages: usize) {}

    #[inline(always)]
    pub(crate) fn record_mount_metadata(_source_len: usize) {}

    #[inline(always)]
    pub(crate) fn record_mount_fast_stat_flags() {}

    #[inline(always)]
    pub(crate) fn record_mount_fast_fs_type() {}

    #[inline(always)]
    pub(crate) fn record_eventfd_read_call() {}

    #[inline(always)]
    pub(crate) fn record_eventfd_write_call() {}

    #[inline(always)]
    pub(crate) fn record_eventfd_reader_sleep() {}

    #[inline(always)]
    pub(crate) fn record_eventfd_writer_sleep() {}

    #[inline(always)]
    pub(crate) fn record_eventfd_reader_wakeup() {}

    #[inline(always)]
    pub(crate) fn record_eventfd_writer_wakeup() {}

    #[inline(always)]
    pub(crate) fn record_local_socket_read_call() {}

    #[inline(always)]
    pub(crate) fn record_local_socket_write_call() {}

    #[inline(always)]
    pub(crate) fn record_local_socket_reader_sleep() {}

    #[inline(always)]
    pub(crate) fn record_local_socket_writer_sleep() {}

    #[inline(always)]
    pub(crate) fn record_local_socket_reader_wakeup() {}

    #[inline(always)]
    pub(crate) fn record_local_socket_writer_wakeup() {}

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
    pub(crate) fn record_vma_range_scan(_area_visits: usize, _index_skips: usize) {}

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
    pub(crate) fn record_usercopy_leaf_pte_cache_hit() {}

    #[inline(always)]
    pub(crate) fn record_usercopy_leaf_pte_cache_miss() {}

    #[inline(always)]
    pub(crate) fn record_usercopy_leaf_pte_cache_invalidation() {}

    #[inline(always)]
    pub(crate) fn record_usercopy_slow_path(_page_count: usize) {}

    #[inline(always)]
    pub(crate) fn record_usercopy_checked_range(_pages: usize, _bytes: usize) {}

    #[inline(always)]
    pub(crate) fn record_usercopy_range_reuse(_chunks: usize, _pages: usize, _bytes: usize) {}

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
    pub(crate) fn record_futex_wake(_key_hit: bool, _tasks: usize) {}

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
