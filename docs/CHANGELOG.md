## 2026-05-07

### Achievements
- fix(trap): quiet signal exits for lmbench (6fa885f8f6da19be267ee16fa101afb2e8a96aaf)

### Shortcomings / Unresolved Issues
- Hard-coded config values (e.g. `USER_STACK_SIZE`, `USER_HEAP_SIZE`) need tuning.
  - *Suggested solution*: Implement configuration parsing from a boot argument, device tree, or a central configuration file.
- Potential performance loss in `d_reclen > buf.len().saturating_sub(written)` check.
  - *Suggested solution*: Profile the directory reading code to verify if this check is a bottleneck, and refactor the loop to avoid redundant boundary checks.
- Unnecessary devices (e.g., GPU) could be removed.
  - *Suggested solution*: Remove the GPU initialization code or conditionalize it with feature flags.
- Consider using `core::sync::LazyLock` instead of `lazy_static!` for `FRAME_ALLOCATOR`.
  - *Suggested solution*: Update the code to use the standard library's `LazyLock` (or `OnceLock`) once available, replacing the `lazy_static` crate dependency.
- Replace `Vec` in `MemorySet` with a higher-performance data structure.
  - *Suggested solution*: Investigate using a `BTreeMap` or an intrusive linked list to manage memory areas more efficiently.
- `UserBuffer` could potentially be replaced or optimized.
  - *Suggested solution*: Analyze memory copy overhead and consider direct slice mappings or more optimized buffer abstractions.
- Re-evaluate the name of `sys_mmap_impl` parameter `prot`.
  - *Suggested solution*: Rename `prot` to something more descriptive like `permissions` or `flags`.
- Map permission in `sys_mmap_impl` does not contain shared and writable.
  - *Suggested solution*: Update `sys_mmap_impl` to properly parse and handle `MAP_SHARED` and `PROT_WRITE` flags.
- `translated_byte_buffer_checked_with_fault` taking the responsibility of the mm module.
  - *Suggested solution*: Refactor to move the checking logic back to the `mm` module and provide a cleaner abstraction for user space translation.
- `current_user_token` uses multiple `unwrap`s; consider using `expect`.
  - *Suggested solution*: Replace `unwrap()` with `.expect("Failed to get current user token")` or handle the error gracefully to prevent kernel panics.
- Why separate `impl TaskControlBlock`?
  - *Suggested solution*: Consolidate all `impl TaskControlBlock` blocks into a single block for better readability.
- Missing implementations or optimizations in vendored `lwext4_rust`:
  - Update timestamps of the parent when we have wall-clock time.
    - *Suggested solution*: Hook into the system's RTC or monotonic clock to update `mtime` and `ctime`.
  - Update timestamp for inode.
    - *Suggested solution*: Similarly, use the system time implementation to update inode timestamps on modifications.
  - Flexible tree reduction is missing.
    - *Suggested solution*: Port or implement the extent tree reduction logic from ext4.
  - Missing journal device support.
    - *Suggested solution*: Implement JBD2 (Journaling Block Device) support for `lwext4_rust`.
  - Features incompatible or read-only to implement (e.g. `EXT4_FINCOM_INLINE_DATA`, `EXT4_FRO_COM_BIGALLOC`).
    - *Suggested solution*: Prioritize implementation of widely used read-only compatible flags.
  - Need to handle features like `EXT4_FINCOM_META_BG`, `EXT4_FINCOM_FLEX_BG` some day.
    - *Suggested solution*: Plan future iterations to support flex block groups and meta block groups.
  - Optimize `unlink`.
    - *Suggested solution*: Refactor directory entry removal to minimize disk I/O.
  - Support symlink correctly.
    - *Suggested solution*: Add proper handling for fast and slow symlinks, including path resolution limit checking.
  - Correct implementation for `set_len` when extending.
    - *Suggested solution*: Replace the naive block-by-block zeroing with more efficient `fallocate` style sparse allocation.
