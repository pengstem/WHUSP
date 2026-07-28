#!/musl/busybox ash

set -u

RUN_ID='@RUN_ID@'
ARCH='@ARCH@'
SMP='@SMP@'
MEM='@MEM@'
BLOCK_IO='@BLOCK_IO_MODE@'
BB=/musl/busybox
PROBE=/x1/fs4_inode_state_probe
BASE=/x1/fs4-inode-state-data

fail()
{
    echo "FS4_INODE_STATE_FAIL run_id=$RUN_ID arch=$ARCH smp=$SMP mem=$MEM block_io=$BLOCK_IO stage=$1 rc=$2"
    exit 1
}

echo "FS4_INODE_STATE_GUEST_START run_id=$RUN_ID arch=$ARCH smp=$SMP mem=$MEM block_io=$BLOCK_IO"
[ -x "$PROBE" ] || fail probe_missing 127
[ "$($BB nproc)" -eq "$SMP" ] || fail nproc_mismatch 1
"$PROBE" "$BASE" || fail workload "$?"
$BB sync || fail sync "$?"
echo "FS4_INODE_STATE_GUEST_PASS run_id=$RUN_ID arch=$ARCH smp=$SMP mem=$MEM block_io=$BLOCK_IO"
exit 0
