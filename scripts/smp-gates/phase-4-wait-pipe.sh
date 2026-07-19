#!/musl/busybox sh

phase=4
case "${WHUSP_ARCH:-}" in
    rv|la) arch="$WHUSP_ARCH" ;;
    *)
        echo "#### SMP GATE FAIL phase=$phase arch=unknown reason=bad-arch ####"
        exit 2
        ;;
esac

worker="/x1/smp-wait-pipe-$arch"
workers=@WORKERS@
ltp_case="/x1/$arch/musl/ltp-cases/@EPOLL_WAIT01_SCRIPT@"
echo "#### SMP GATE START phase=$phase arch=$arch profile=wait-pipe ####"
if [ "$workers" -ne 8 ] || [ ! -x "$worker" ]; then
    echo "#### SMP GATE FAIL phase=$phase arch=$arch reason=bad-worker-config ####"
    exit 1
fi

worker_id=0
while [ "$worker_id" -lt "$workers" ]; do
    reader_id=$((worker_id + 1))
    "$worker" "$worker_id" | "$worker" "$reader_id" &
    worker_id=$((worker_id + 2))
done
if ! wait; then
    echo "#### SMP GATE FAIL phase=$phase arch=$arch reason=pipe-epoll-worker ####"
    exit 1
fi
if ! "$ltp_case"; then
    echo "#### SMP GATE FAIL phase=$phase arch=$arch reason=epoll-wait01 ####"
    exit 1
fi
echo "#### SMP GATE PASS phase=$phase arch=$arch ####"
