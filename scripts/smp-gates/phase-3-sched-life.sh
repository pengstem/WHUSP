#!/musl/busybox sh

phase=3
case "${WHUSP_ARCH:-}" in
    rv|la) arch="$WHUSP_ARCH" ;;
    *)
        echo "#### SMP GATE FAIL phase=$phase arch=unknown reason=bad-arch ####"
        exit 2
        ;;
esac

echo "#### SMP GATE START phase=$phase arch=$arch profile=sched-life ####"
worker="/x1/smp-sched-life-$arch"
if [ ! -x "$worker" ]; then
    echo "#### SMP GATE FAIL phase=$phase arch=$arch reason=missing-worker ####"
    exit 1
fi

# Prime the auxiliary-disk file cache on CPU 0. Phase 3 intentionally opens
# only scheduler/task lifecycle concurrency; Phase 4 is the first gate that
# admits concurrent VFS and block-device traffic.
if ! /musl/busybox cat "$worker" >/dev/null; then
    echo "#### SMP GATE FAIL phase=$phase arch=$arch reason=worker-prime ####"
    exit 1
fi

pids=""
worker_index=0
while [ "$worker_index" -lt 8 ]; do
    "$worker" &
    pids="$pids $!"
    worker_index=$((worker_index + 1))
done

if ! wait; then
    echo "#### SMP GATE FAIL phase=$phase arch=$arch reason=worker-exit ####"
    exit 1
fi

waitpid_case=""
for candidate in /x1/"$arch"/musl/ltp-cases/*-waitpid01.sh; do
    waitpid_case="$candidate"
done
if [ ! -f "$waitpid_case" ] || ! /musl/busybox sh "$waitpid_case"; then
    echo "#### SMP GATE FAIL phase=$phase arch=$arch reason=waitpid01 ####"
    exit 1
fi

echo "#### SMP GATE PASS phase=$phase arch=$arch ####"
