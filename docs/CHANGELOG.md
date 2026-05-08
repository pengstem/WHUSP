## 2026-05-08

### Achievements
- `32ea9db88610602e90c209339100dbb736e04a00`: chore(ltp): add ltp related env when running tests

### Shortcomings & Unresolved Issues
- **TODO:** Hard-coded configuration values in `os/src/config.rs` need tuning. **Suggested Solution:** Review and tune hard-coded configuration values.
- **TODO:** Potential performance loss in directory entry iteration. **Suggested Solution:** Optimize the directory entry reading logic.
- **TODO:** Redundant GPU devices check. **Suggested Solution:** Remove the devices if they are no longer needed.
- **TODO:** `lazy_static!` usage for `FRAME_ALLOCATOR` could be replaced. **Suggested Solution:** Replace `lazy_static!` with `core::sync::LazyLock`.
- **TODO:** `Vec` usage for areas in `MemorySet` should be replaced with a high-performance data structure. **Suggested Solution:** Replace `Vec` with a higher-performance data structure.
- **TODO:** `UserBuffer` could be replaced. **Suggested Solution:** Review and potentially replace or refactor the `UserBuffer` implementation.
- **TODO:** `prot` argument naming is not good. **Suggested Solution:** Rename arguments for clarity.
- **TODO:** Map permission logic missing `shared` and `writable`. **Suggested Solution:** Review map permission flags to properly support shared and writable memory mappings.
- **TODO:** Functions taking the responsibility of the `mm` module. **Suggested Solution:** Refactor these functions to centralize logic within the `mm` module.
- **TODO:** Many `unwrap()` calls, maybe change to `expect()`. **Suggested Solution:** Replace `unwrap()` with `expect()` with descriptive error messages.
- **TODO:** Why separate `impl`? **Suggested Solution:** Consolidate separated `impl` blocks.
- **TODO:** `libctest-musl` manual run added new TODOs. **Suggested Solution:** Address the new TODOs identified during `libctest-musl` manual runs.
