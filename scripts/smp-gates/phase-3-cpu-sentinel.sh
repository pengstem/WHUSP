#!/musl/busybox sh

phase=3
case "${WHUSP_ARCH:-}" in
    rv|la) arch="$WHUSP_ARCH" ;;
    *)
        echo "#### SMP PERF FAIL phase=$phase arch=unknown reason=bad-arch ####"
        exit 2
        ;;
esac

workers=@WORKERS@
sentinel="/x1/smp-cpu-sentinel-$arch"

echo "#### SMP PERF START phase=$phase arch=$arch profile=cpu-sentinel workers=$workers ####"
if [ ! -x "$sentinel" ]; then
    echo "#### SMP PERF FAIL phase=$phase arch=$arch reason=missing-sentinel ####"
    exit 1
fi

# Keep executable loading outside the measured CPU interval. Concurrent VFS and
# VirtIO are admitted by Phase 4, not by this scheduler-only sentinel.
if ! /musl/busybox cat "$sentinel" >/dev/null; then
    echo "#### SMP PERF FAIL phase=$phase arch=$arch reason=sentinel-prime ####"
    exit 1
fi

sample=0
while [ "$sample" -lt 4 ]; do
    worker_index=0
    while [ "$worker_index" -lt "$workers" ]; do
        "$sentinel" "$sample" &
        worker_index=$((worker_index + 1))
    done
    if ! wait; then
        echo "#### SMP PERF FAIL phase=$phase arch=$arch reason=worker-exit sample=$sample ####"
        exit 1
    fi
    # wait(2) can observe the zombie before the remote CPU has completed its
    # idle-stack switch bookkeeping. Leave a bounded gap between counter epochs.
    /musl/busybox sleep 1
    sample=$((sample + 1))
done

echo "#### SMP PERF PASS phase=$phase arch=$arch profile=cpu-sentinel workers=$workers ####"
