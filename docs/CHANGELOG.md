# Changelog

## 2026-06-30

### Achievements
- Updated readme (Commit: `cedb8ef68313b9181dd93b81cbb9165a24c4feb9`)

### Current shortcomings or unresolved issues
- `vendor/lwext4_rust/src/fs.rs` contains a TODO to "optimize".
  - Suggested solution: Optimize the related code section.
- `vendor/lwext4_rust/src/inode/file.rs` contains a TODO "symlink?".
  - Suggested solution: Address symlink handling logic.
- `vendor/lwext4_rust/c/lwext4/src/ext4_extent.c` contains a TODO "flexible tree reduction should be here".
  - Suggested solution: Implement flexible tree reduction algorithm.
- `vendor/lwext4_rust/c/lwext4/src/ext4_mkfs.c` contains a TODO "handle this features some day...".
  - Suggested solution: Add support for these ext4 features.
- `vendor/lwext4_rust/c/lwext4/src/ext4_journal.c` contains a TODO "journal device.".
  - Suggested solution: Implement journal device support.
