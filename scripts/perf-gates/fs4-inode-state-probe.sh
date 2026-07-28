#!/musl/busybox ash

set -u

RUN_ID='@RUN_ID@'
ARCH='@ARCH@'
SMP='@SMP@'
MEM='@MEM@'
BLOCK_IO='@BLOCK_IO_MODE@'
PERF='@PERF_COUNTERS@'
BB=/musl/busybox
PROBE=/x1/fs4_inode_state_probe
BASE=/x1/fs4-inode-state-data
PERF_PATH=/proc/oskernel/perf

fail()
{
    echo "FS4_INODE_STATE_FAIL run_id=$RUN_ID arch=$ARCH smp=$SMP mem=$MEM block_io=$BLOCK_IO stage=$1 rc=$2"
    exit 1
}

echo "FS4_INODE_STATE_GUEST_START run_id=$RUN_ID arch=$ARCH smp=$SMP mem=$MEM block_io=$BLOCK_IO perf=$PERF"
[ -x "$PROBE" ] || fail probe_missing 127
[ "$($BB nproc)" -eq "$SMP" ] || fail nproc_mismatch 1
echo "FS4_INODE_STATE_PERF_BEGIN point=before"
$BB cat "$PERF_PATH" || fail perf_before "$?"
echo "FS4_INODE_STATE_PERF_END point=before"
"$PROBE" "$BASE"
WORKLOAD_RC=$?
echo "FS4_INODE_STATE_PERF_BEGIN point=after"
$BB cat "$PERF_PATH" || fail perf_after "$?"
echo "FS4_INODE_STATE_PERF_END point=after"
[ "$WORKLOAD_RC" -eq 0 ] || fail workload "$WORKLOAD_RC"
$BB sync || fail sync "$?"
echo "FS4_INODE_STATE_GUEST_PASS run_id=$RUN_ID arch=$ARCH smp=$SMP mem=$MEM block_io=$BLOCK_IO perf=$PERF"
exit 0
