#!/musl/busybox sh
set -eu

echo "FS4E_METADATA_GUEST_START run_id=@RUN_ID@ arch=@ARCH@ smp=@SMP@ mem=@MEM@ block_io=@BLOCK_IO_MODE@ perf=@PERF_COUNTERS@ expectation=@EXPECTATION@ affinity=pinned-sequential timing=barrier-work-only"
/x1/fs4e_metadata_probe @EXPECTATION@
echo "FS4E_METADATA_GUEST_PASS run_id=@RUN_ID@ arch=@ARCH@ smp=@SMP@ mem=@MEM@ block_io=@BLOCK_IO_MODE@ perf=@PERF_COUNTERS@ expectation=@EXPECTATION@ affinity=pinned-sequential timing=barrier-work-only"
/musl/busybox poweroff -f
