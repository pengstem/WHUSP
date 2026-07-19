#!/musl/busybox sh

phase=4
case "${WHUSP_ARCH:-}" in
    rv|la) arch="$WHUSP_ARCH" ;;
    *)
        echo "#### SMP GATE FAIL phase=$phase arch=unknown reason=bad-arch ####"
        exit 2
        ;;
esac

worker="/x1/@FUTEX_WORKER_BASENAME@-$arch"
workers=@WORKERS@
ltp_case="/x1/$arch/musl/ltp-cases/@FUTEX_WAIT01_SCRIPT@"
echo "#### SMP GATE START phase=$phase arch=$arch profile=wait-futex ####"
if [ "$workers" -ne 8 ] || [ ! -x "$worker" ]; then
    echo "#### SMP GATE FAIL phase=$phase arch=$arch reason=bad-worker-config ####"
    exit 1
fi
if ! "$worker" init; then
    echo "#### SMP GATE FAIL phase=$phase arch=$arch reason=futex-init ####"
    exit 1
fi

worker_id=0
while [ "$worker_id" -lt "$workers" ]; do
    "$worker" "$worker_id" &
    worker_id=$((worker_id + 1))
done
if ! wait; then
    /musl/busybox rm -f /x1/.smp-wait-futex
    echo "#### SMP GATE FAIL phase=$phase arch=$arch reason=futex-worker ####"
    exit 1
fi
/musl/busybox rm -f /x1/.smp-wait-futex

if ! "$ltp_case"; then
    echo "#### SMP GATE FAIL phase=$phase arch=$arch reason=futex-wait01 ####"
    exit 1
fi
echo "#### SMP GATE PASS phase=$phase arch=$arch ####"
