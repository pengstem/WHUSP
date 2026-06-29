# Changelog

## 2026-06-29
- Moved interactive mode out of rust source code to change modes without rebuilding (128424138c0faddff21e936d51f5bc93799dd81e).
  - Shortcomings: UNFINISHED: sparse extension only records the new file size and zeroes the old tail block. It does not yet implement the full Linux sparse-file behavior for every indirect-block layout and read-back edge case. (from `vendor/lwext4_rust/src/inode/file.rs`)
  - Suggested solution: Implement full Linux sparse-file behavior for every indirect-block layout and read-back edge case.
