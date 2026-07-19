#!/musl/busybox sh

phase=4
case "${WHUSP_ARCH:-}" in
    rv|la) arch="$WHUSP_ARCH" ;;
    *)
        echo "#### SMP GATE FAIL phase=$phase arch=unknown reason=bad-arch ####"
        exit 2
        ;;
esac

worker="/x1/@TIMER_WORKER_BASENAME@-$arch"
workers=@WORKERS@
ltp_case="/x1/$arch/musl/ltp-cases/@CLOCK_NANOSLEEP_SCRIPT@"
echo "#### SMP GATE START phase=$phase arch=$arch profile=wait-timer ####"
if [ ! -x "$worker" ]; then
    echo "#### SMP GATE FAIL phase=$phase arch=$arch reason=missing-worker ####"
    exit 1
fi

worker_id=0
while [ "$worker_id" -lt "$workers" ]; do
    "$worker" "$worker_id" &
    worker_id=$((worker_id + 1))
done
if ! wait; then
    echo "#### SMP GATE FAIL phase=$phase arch=$arch reason=timer-worker ####"
    exit 1
fi

if ! "$ltp_case"; then
    echo "#### SMP GATE FAIL phase=$phase arch=$arch reason=clock-nanosleep01 ####"
    exit 1
fi
echo "#### SMP GATE PASS phase=$phase arch=$arch ####"
