# Changelog

## 2026-05-21

### Achievements
- `d07b8db` fix(block): update cache on writes. Implemented basic read/write/append operations to files, block caching operations, and fs inode manipulation.

### Shortcomings and Unresolved Issues
- **Sparse file extension behavior is incomplete:** `ext4_inode_set_size` sets the new size, but sparse extension only records the new file size and zeroes the old tail block. It does not yet implement the full Linux sparse-file behavior for every indirect-block layout and read-back edge case.
  - *Suggested Solution:* Implement sparse-file block allocation tracking handling indirect, double-indirect and triple-indirect blocks or extent trees explicitly in read and write operations.
- **Symlink logic during writes:** The `write_at` function in `inode/file.rs` notes `// TODO: symlink?`, which implies that inline data writing for symlinks is either unhandled or incomplete.
  - *Suggested Solution:* Implement specific handler for symlinks to allow writing target path inline into the inode blocks buffer if the length is small enough, or allocate data blocks otherwise.
- **Directory rename optimization:** The `rename` function in `fs.rs` includes a `// TODO: optimize` when unlinking the destination directory name. Currently it sequentially delegates to `unlink` which may be expensive or trigger extra reads.
  - *Suggested Solution:* Directly overwrite or replace the existing directory entry if present instead of a full `unlink` followed by an `add_entry`.
