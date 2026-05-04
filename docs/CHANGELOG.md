## 2026-05-05

### Achievements
* chore(bench): refresh the bench marks (commit 97a3ab9425f68d2ead3c81b77b3c2a6dd62f82ec)

### Shortcomings / Unresolved Issues
* `vendor/lwext4_rust/src/inode/file.rs`: Correct implementation for `set_len` when extending file size.
* `vendor/lwext4_rust/src/fs.rs`: Optimize `rename` operation.
* `vendor/lwext4_rust/c/lwext4/src/ext4_fs.c` (and others): Implement journal device.
* `vendor/lwext4_rust/c/lwext4/src/ext4_dir_idx.c`: Implement flexible tree reduction.
* `vendor/lwext4_rust/c/lwext4/src/ext4_inode.c`: Update timestamps of the parent and inode.
