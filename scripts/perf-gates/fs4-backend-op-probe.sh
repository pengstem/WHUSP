#!/musl/busybox ash

set -u

RUN_ID='@RUN_ID@'
ARCH='@ARCH@'
SMP='@SMP@'
MEM='@MEM@'
BLOCK_IO='@BLOCK_IO_MODE@'
PERF='@PERF_COUNTERS@'
ITERATIONS='@ITERATIONS@'
BB=/musl/busybox
PROBE=/x1/fs4_backend_op_probe
BASE=/x1/fs4-backend-op-data
PERF_PATH=/proc/oskernel/perf

fail()
{
    echo "FS4_BACKEND_OP_FAIL run_id=$RUN_ID arch=$ARCH smp=$SMP mem=$MEM block_io=$BLOCK_IO perf=$PERF stage=$1 rc=$2"
    exit 1
}

echo "FS4_BACKEND_OP_GUEST_START run_id=$RUN_ID arch=$ARCH smp=$SMP mem=$MEM block_io=$BLOCK_IO perf=$PERF"
[ -x "$PROBE" ] || fail probe_missing 127
[ "$($BB nproc)" -eq "$SMP" ] || fail nproc_mismatch 1
"$PROBE" setup "$BASE" || fail setup "$?"
$BB sync || fail sync "$?"
echo "FS4_BACKEND_OP_PERF_BEGIN point=before"
$BB cat "$PERF_PATH" || fail perf_before "$?"
echo "FS4_BACKEND_OP_PERF_END point=before"
"$PROBE" run "$BASE" "$ITERATIONS" || fail workload "$?"
echo "FS4_BACKEND_OP_PERF_BEGIN point=after"
$BB cat "$PERF_PATH" || fail perf_after "$?"
echo "FS4_BACKEND_OP_PERF_END point=after"
echo "FS4_BACKEND_OP_PASS run_id=$RUN_ID arch=$ARCH smp=$SMP mem=$MEM block_io=$BLOCK_IO perf=$PERF"
exit 0
