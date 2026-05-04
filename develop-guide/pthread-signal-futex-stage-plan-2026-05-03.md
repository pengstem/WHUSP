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
- musl/riscv64 `cancel_handler` reads the first raw signal-mask word at
  `ucontext + 40`, ORs in raw bit `1 << 32` for signal 33, and reads/writes the
  interrupted PC at `ucontext + 176`.

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
- 2026-05-03 Stage B resume: continue from the uncommitted RISC-V signal-frame
  work. First remove temporary `signal-probe` diagnostics, preserve the raw-bit
  signal-set fixes needed for musl signal 33, then validate the narrow
  `pthread_cancel` path on a fresh copied RV test disk before broadening to
  `pthread_cancel-points`.
- 2026-05-03 Stage B: fixed the RISC-V signal frame so the generated
  `ucontext` carries both the old raw sigmask at offset 40 and the interrupted
  PC at offset 176. `rt_sigreturn` now restores the thread mask from the
  user-modifiable `ucontext` sigmask, not from a private saved-mask field, which
  lets musl's deferred cancel handler block SIGCANCEL before returning to the
  interrupted instruction.
- 2026-05-03 Stage B validation: with a fresh copy of
  `../reference-project/NighthawkOS/sdcard-rv.img` at
  `/tmp/sdcard-rv-signal.img`, `make kernel-rv` passed, and the manual shell
  run `cd /musl && ./runtest.exe -w entry-static.exe pthread_cancel` printed
  `Pass!`. The follow-up probe
  `./runtest.exe -w entry-static.exe pthread_cancel-points` now fails with
  `FAIL pthread_cancel-points [signal Hangup]`, so Stage C starts from that
  first remaining cancel-point evidence rather than the old `rt_sigreturn`
  loop.
- 2026-05-04 Stage C direction: removed the experimental kernel-side musl
  cancel-state filter. That filter left SIGCANCEL pending and unmasked, which
  let earlier cancellation state contaminate the final non-cancel `shm_open`
  scenario. The replacement is the Linux-like wait boundary: keep normal
  SIGCANCEL handler delivery and make `futex_wait` remove its waiter and return
  `EINTR` when it is woken without a futex wake or timeout.
- 2026-05-04 Stage C root cause: `pthread_cancel-points` reached the final
  `shm_open` scenario, but `shm_open("/testshm", O_RDWR|O_CREAT, 0666)` failed
  because the kernel had no `/dev/shm` tmpfs. The test then entered `t_error`,
  whose `write(64)` path is a musl cancellation point, so the pending cancel
  request turned the diagnostic path into `PTHREAD_CANCELED`. Linux man-pages
  document POSIX SHM as normally backed by a tmpfs mounted at `/dev/shm`, so the
  kernel now mounts tmpfs there instead of adding a path rewrite.
- 2026-05-04 Stage C validation: with a temporary initproc command running
  `cd /musl && ./runtest.exe -w entry-static.exe pthread_cancel_points`,
  `make kernel-rv` passed and the QEMU run printed `Pass!` for
  `pthread_cancel_points` after mounting `/dev/shm`.
- 2026-05-04 Stage D direction: implement the Linux PI futex word policy for
  `FUTEX_LOCK_PI`, `FUTEX_TRYLOCK_PI`, and `FUTEX_UNLOCK_PI`: owner TID in the
  low bits, `FUTEX_WAITERS` while the kernel has queued waiters, and
  `EDEADLK`/`EPERM` for self-lock and non-owner unlock. Real scheduler priority
  boosting and priority-ordered waiter selection remain marked `UNFINISHED`.
- 2026-05-04 Stage D validation: current `sdcard-rv.img` does not include
  `pthread_mutex_pi` in `libc-test/static.txt`, so `entry-static.exe
  pthread_mutex_pi` never reaches the testcase table. For validation, compiled
  `../testsuits-for-oskernel/libc-test/src/functional/pthread_mutex_pi.c` as a
  temporary static RISC-V musl binary with `src/common/print.c`, injected it
  into guest `/tmp` via base64 over the interactive shell, and ran it directly.
  The testcase returned to the shell with no `t_error` output.
- 2026-05-04 Stage E direction: store the Linux robust-list head per
  `TaskControlBlock`, expose `set_robust_list(99)` / `get_robust_list(100)`,
  and on thread exit scan the circular list plus `list_op_pending`. If the
  futex owner TID matches the exiting Linux-visible TID, clear the owner bits,
  preserve `FUTEX_WAITERS`, set `FUTEX_OWNER_DIED`, and wake both private and
  shared futex queues for the word.
- 2026-05-04 Stage E validation: current `sdcard-rv.img` also lacks
  `pthread_robust` in the packaged entry table, so compiled
  `../testsuits-for-oskernel/libc-test/src/functional/pthread_robust.c` as a
  temporary static RISC-V musl binary with `src/common/print.c`, injected it
  into the guest through a base64 stdin pipeline, ran it directly, and observed
  `ROBUST_DONE:0` with no `t_error` output. The guest `chmod` command returned
  `Function not implemented`, but the decoded file was executable and the test
  completed.
