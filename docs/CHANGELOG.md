# Changelog

## 2026-05-01

### Achievements
- feat(symlinkat): support for the symlinkat syscall (012f85c88aba53ff5dc2af6ffd5183584222e2ea)

### Shortcomings and Unresolved Issues
- The sys_symlinkat syscall only creates real EXT4 symlink nodes. Linux path lookup follows symlink targets, and readlinkat() returns link contents.
  - Solution: Update path lookup logic to follow symlink targets and implement readlinkat() syscall to return link contents.
- Linux linkat supports AT_SYMLINK_FOLLOW and AT_EMPTY_PATH; this kernel currently implements pathname hard links only.
  - Solution: Implement AT_SYMLINK_FOLLOW and AT_EMPTY_PATH support in linkat syscall.
- Linux RENAME_EXCHANGE atomically swaps two existing pathnames. The current EXT4/VFS wrapper only supports one-way rename.
  - Solution: Add support for RENAME_EXCHANGE in the EXT4/VFS wrapper to swap two existing pathnames atomically.
- Linux RENAME_WHITEOUT creates an overlay/union whiteout device while renaming. This kernel has no overlay filesystem support.
  - Solution: Add support for overlay filesystem or create a mechanism to handle RENAME_WHITEOUT appropriately without full overlay fs support.
- In `vendor/lwext4_rust/src/inode/file.rs`, the `write_at` function has a `// TODO: symlink?` comment.
  - Solution: Verify if `write_at` should handle symlinks differently or if symlinks can be written to directly. If symlinks cannot be written to, return an error.
- In `vendor/lwext4_rust/src/inode/file.rs`, the `set_len` function has a `// TODO: correct implementation` comment when `len > cur_len`. The code writes zeros block by block.
  - Solution: Optimize the implementation by using unwritten extents or other sparse file features to extend the file without explicitly writing zeros block by block if possible.
- In `os/src/fs/ext4.rs`, the `unlink` function has a `// TODO: optimize` comment, where it ignores ENOENT errors and does nothing when the destination file exists but cannot be unlinked.
  - Solution: Investigate if there is a more optimal way to handle `unlink` without incurring additional overhead or failing silently.
