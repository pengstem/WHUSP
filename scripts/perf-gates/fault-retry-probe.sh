#!/musl/busybox ash

set -u

RUN_ID='@RUN_ID@'
ARCH='@ARCH@'
SMP='@SMP@'
MEM='@MEM@'
BB=/musl/busybox
PROBE=/x1/fault_retry_probe
BASE=/x1/fault-retry-data
PERF_PATH=/proc/oskernel/perf

fail()
{
    echo "FAULT_RETRY_GUEST_FAIL run_id=$RUN_ID arch=$ARCH stage=$1 rc=$2"
    exit 1
}

echo "FAULT_RETRY_GUEST_START run_id=$RUN_ID arch=$ARCH smp=$SMP mem=$MEM"
[ -x "$PROBE" ] || fail probe_missing 127
[ "$($BB nproc)" -eq "$SMP" ] || fail nproc_mismatch 1
echo "FAULT_RETRY_PERF_BEGIN point=before"
$BB cat "$PERF_PATH" || fail perf_before "$?"
echo "FAULT_RETRY_PERF_END point=before"
"$PROBE" "$BASE"
WORKLOAD_RC=$?
echo "FAULT_RETRY_PERF_BEGIN point=after"
$BB cat "$PERF_PATH" || fail perf_after "$?"
echo "FAULT_RETRY_PERF_END point=after"
[ "$WORKLOAD_RC" -eq 0 ] || fail workload "$WORKLOAD_RC"
$BB sync || fail sync "$?"
echo "FAULT_RETRY_GUEST_PASS run_id=$RUN_ID arch=$ARCH smp=$SMP mem=$MEM"
exit 0
