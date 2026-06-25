# Changelog

## 2026-05-13

### Achievements
- **feat(memfd_secret)**: Support `memfd_secret` syscall and introduced basic fd-compat stubs (commit `586533be2a3b43d457608fcc1da325b57ca35f8a`).

### Shortcomings & Unresolved Issues
- **`memfd_secret`**: Linux `memfd_secret` backs `mmap()` with secret memory and enforces `RLIMIT_MEMLOCK`; currently, this fd only satisfies generic fd probes. Suggested solution: implement memory isolation for mmap mappings and enforce RLIMIT checks.
- **`signalfd`**: Updating an existing signalfd requires real signalfd state; current coverage only creates new descriptors. Pending-signal delivery through signalfd is not modeled yet. Suggested solution: track signalfd state and route pending signals to signalfd readers.
- **`timerfd`**: Timerfd expiration accounting and read semantics are not implemented. Suggested solution: implement timer queues to handle wakeups and read counts.
- **`inotify`**: Inotify watches and event queues are not implemented. Suggested solution: build watch structures and event broadcasting.
- **`userfaultfd`**: Page-fault registration and event queues are not implemented. Suggested solution: introduce a mechanism to trap page faults and route them to userfaultfd readers.
- **`perf_event_open`**: Perf event sampling/counter state is not implemented. Suggested solution: connect to hardware PMUs or software counters.
- **`io_uring`**: Shared rings and enter/register operations are not implemented. Suggested solution: add SQ/CQ ring memory mapping and request processing logic.
- **`bpf`**: Only `BPF_MAP_CREATE` is accepted for LTP probes. BPF map storage and commands are not implemented. Suggested solution: provide basic map allocations and lookup/update implementations.
