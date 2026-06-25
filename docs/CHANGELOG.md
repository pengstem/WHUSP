
## 2026-06-25

- feat(iozone): allow read dirty (2834f97)

### Shortcomings / Unresolved Issues

- **Issue**: In `vendor/lwext4_rust/src/inode/file.rs`, the sparse extension only records the new file size and zeroes the old tail block. It does not yet implement the full Linux sparse-file behavior for every indirect-block layout and read-back edge case.
  - **Suggested Solution**: Implement the full Linux sparse-file behavior for all indirect-block layouts and edge cases during read-back.
- **Issue**: In `vendor/lwext4_rust/src/inode/file.rs`, there is a missing implementation or checking for symlink handling during file operations.
  - **Suggested Solution**: Implement and verify symlink resolution and handling for file read/write operations.
