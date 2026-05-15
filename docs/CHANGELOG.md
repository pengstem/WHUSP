# Changelog

## May 16 2026

### Achievements
* Run bench (commit `6079968`)

### Shortcomings / Unresolved Issues

#### `os` package
* Hard-coded values in `os/src/config.rs` need tuning.
  - **Suggested solution**: Refactor to allow configurable values or properly tune the hard-coded limits for stack size, heap size, and mmap base.
* Classic performance loss check needed in `os/src/fs/dirent.rs` when `d_reclen > buf.len().saturating_sub(written)`.
  - **Suggested solution**: Investigate performance implications and optimize the directory entry read buffering.
* Unnecessary GPU device initialization in `os/src/main.rs`.
  - **Suggested solution**: Remove the GPU device check and initialization if it is not utilized in the system.
* Use of `lazy_static` for `FRAME_ALLOCATOR` in `os/src/mm/frame_allocator.rs`.
  - **Suggested solution**: Consider replacing `lazy_static` with `core::sync::LazyLock` for cleaner static initialization.

#### `vendor/lwext4_rust` package
* Unoptimized `unlink` operation in `vendor/lwext4_rust/src/inode/dir.rs`.
  - **Suggested solution**: Optimize the logic for unlinking destination names in directories.
* Symlink edge case handling in `vendor/lwext4_rust/src/inode/file.rs` during write operations.
  - **Suggested solution**: Properly handle block starts and allocations when dealing with symlinks during file writes.