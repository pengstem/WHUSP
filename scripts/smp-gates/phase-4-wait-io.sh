#!/musl/busybox sh

phase=4
case "${WHUSP_ARCH:-}" in
    rv|la) arch="$WHUSP_ARCH" ;;
    *)
        echo "#### SMP GATE FAIL phase=$phase arch=unknown reason=bad-arch ####"
        exit 2
        ;;
esac

worker="/x1/smp-wait-io-$arch"
workers=@WORKERS@
echo "#### SMP GATE START phase=$phase arch=$arch profile=wait-io ####"
if [ ! -x "$worker" ]; then
    echo "#### SMP GATE FAIL phase=$phase arch=$arch reason=missing-worker ####"
    exit 1
fi

# Keep executable loading out of the concurrent I/O interval. The workload
# itself writes eight disjoint files through the real x1 filesystem/device.
if ! /musl/busybox cat "$worker" >/dev/null; then
    echo "#### SMP GATE FAIL phase=$phase arch=$arch reason=worker-prime ####"
    exit 1
fi

worker_id=0
while [ "$worker_id" -lt "$workers" ]; do
    "$worker" "$worker_id" &
    worker_id=$((worker_id + 1))
done
if ! wait; then
    echo "#### SMP GATE FAIL phase=$phase arch=$arch reason=block-worker ####"
    exit 1
fi

# Reap can precede the remote idle-stack exit accounting by a short interval.
/musl/busybox sleep 1
echo "#### SMP GATE PASS phase=$phase arch=$arch ####"
