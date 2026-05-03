# pthread signal/cancel and futex staged bringup

Date: 2026-05-03

## Scope

Finish P2.7 items 5-8 for the RISC-V musl pthread path:

1. signal/cancel foundation: `tkill(130)`, `tgkill(131)`, `rt_sigaction(134)`, `rt_sigprocmask(135)`, `rt_sigreturn(139)`, per-thread pending/mask movement, and interruptible futex waits.
2. pthread cancel validation: `pthread_cancel` then `pthread_cancel_points`.
3. PI futex minimal compatibility: `FUTEX_LOCK_PI`, `FUTEX_UNLOCK_PI`, `FUTEX_TRYLOCK_PI`.
4. robust futex: `set_robust_list(99)`, `get_robust_list(100)`, and owner-death processing on thread exit.

## References Checked

Official docs:

- Linux man-pages `tkill(2)`: `tkill` targets a thread by TID; `tgkill` also validates the thread group ID.
- Linux man-pages `sigprocmask(2)`: signal masks are per-thread; `SIGKILL` and `SIGSTOP` cannot be blocked; `rt_sigprocmask` validates the architecture-specific kernel sigset size.
- Linux man-pages `sigaction(2)`: dispositions are process-wide; `SIGKILL` and `SIGSTOP` cannot be changed; `SA_SIGINFO` handlers receive `(sig, siginfo_t *, ucontext_t *)`.
- Linux man-pages `sigreturn(2)`: the kernel saves register context and mask in a user signal frame, then `rt_sigreturn` restores them.
- Linux man-pages `nptl(7)`: NPTL uses real-time signals 32 and 33 internally, including thread cancellation.
- Linux man-pages `futex(2)` and `get_robust_list(2)`: PI futex command numbers and robust-list owner-death behavior.

Reference projects:

- `../reference-project/RocketOS`: signal syscall validation, per-task masks, `tkill`/`tgkill`, robust-list traversal.
- `../reference-project/NighthawkOS`: signal action/mask structure and thread-directed signal lookup.
- `../reference-project/starry-mix`: compact signal/futex syscall structure and robust-list syscall storage.

Local ABI evidence:

- The local musl RISC-V static objects use `tkill(130)` for `pthread_kill`.
- musl `pthread_cancel` installs and sends signal 33.
- musl `__restore` invokes syscall 139.
- musl cancel handlers inspect the interrupted PC from the `ucontext_t` passed as the third signal-handler argument.

## Stage Checklist

### Stage A - signal data model and syscall surface

- Extend signal sets to cover Linux real-time signal 33.
- Move pending signal bits and masks into `TaskControlBlockInner`.
- Keep signal actions process-wide.
- Add dispatch and syscall implementations for `tkill`, `tgkill`, `rt_sigaction`, `rt_sigprocmask`, and a stub boundary for `rt_sigreturn` until Stage B.
- Wake the target thread when a directed signal is queued.
- Recheck: `make kernel-rv`; then item boot/log probe for `pthread_cancel` reaching signal setup if runnable.
- Commit: `feat(signal): add thread signal syscall foundation`.

### Stage B - RISC-V signal frame and cancel delivery

- Add a RISC-V signal-frame builder/restorer that preserves the interrupted trap context and signal mask.
- Pass handler args as `a0=signum`, `a1=siginfo`, `a2=ucontext`.
- Place saved PC at the musl RISC-V `ucontext_t` offset used by `pthread_cancel`.
- Implement `rt_sigreturn` restore for RISC-V; keep LoongArch as an explicit `// UNFINISHED:` runtime boundary if needed.
- Make futex waits return `EINTR` when an unblocked signal becomes deliverable.
- Recheck: `pthread_cancel`; commit after the narrow check.

### Stage C - pthread cancel points

- Recheck deferred cancellation behavior after Stage B.
- Patch only the syscall/wait boundary that explains the first failing cancel-point evidence.
- Recheck: `pthread_cancel-points`; commit after the narrow check.

### Stage D - PI futex minimal compatibility

- Implement `FUTEX_TRYLOCK_PI`, `FUTEX_LOCK_PI`, and `FUTEX_UNLOCK_PI` around the current futex wait queue.
- Preserve Linux owner bits: `FUTEX_WAITERS`, `FUTEX_OWNER_DIED`, and `FUTEX_TID_MASK`.
- Mark real priority inheritance with `// UNFINISHED:`.
- Recheck: `pthread_mutex_pi`; commit after the narrow check.

### Stage E - robust futex

- Store robust-list head per thread.
- Implement `set_robust_list(99)` and `get_robust_list(100)`.
- On thread exit, traverse the robust list and `list_op_pending`; if the exiting Linux TID owns the futex, set `FUTEX_OWNER_DIED`, clear owner TID, preserve `FUTEX_WAITERS`, and wake one waiter.
- Recheck: `pthread_robust`; commit after the narrow check.

## Move Log

- 2026-05-03: Created this plan before code movement. No source movement yet.
- 2026-05-03 Stage A: moved pending signal bits, signal info slots, and signal
  mask into `TaskControlBlockInner`; replaced PCB pending signal storage with a
  process-wide `signal_actions` table; added syscall dispatch and basic
  implementations for `tkill`, `tgkill`, `rt_sigaction`, `rt_sigprocmask`, and
  the `rt_sigreturn` Stage B boundary.
