# Changelog

## 2026-04-28

### Achievements

-   **feat(loongarch): now it produce kernel-la** (`b2d5e413f54c76c424e788a5d1a27ccbb7f6364f`)

### Shortcomings & Unresolved Issues

-   **Unresolved Issue:** Several incomplete implementations exist within the `lwext4_rust` vendor crate, as noted by `TODO` comments.
    -   *Suggested Solution:* Address these `TODO`s in future commits by providing the missing feature implementations, specifically focusing on `TODO: correct implementation` for `set_len` in `vendor/lwext4_rust/src/inode/file.rs`, `TODO: optimize` for `unlink` in `vendor/lwext4_rust/src/inode/dir.rs`, and implementing the `TODO: symlink?` logic in `vendor/lwext4_rust/src/inode/dir.rs`. (Note: The C parts of `lwext4` also have `TODO`s that should ideally be forwarded to upstream).
