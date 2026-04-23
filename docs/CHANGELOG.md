# Changelog

## 2026-04-23

### Achievements
- `9311dc4` fix(mm): Map ELF files with offsets.

### Shortcomings & Unresolved Issues
- In `lwext4_rust`, extending file length is not fully robust. Specifically, the implementation in `set_len` for handling an increased length requires correction (marked with a TODO).
  - *Suggested solution*: Implement the complete logic for allocating new blocks, zeroing out newly allocated portions correctly, and robustly handling block boundaries when extending file size in `lwext4_rust`.
