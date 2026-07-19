#!/musl/busybox sh

phase=4
case "${WHUSP_ARCH:-}" in
    rv|la) arch="$WHUSP_ARCH" ;;
    *)
        echo "#### SMP GATE FAIL phase=$phase arch=unknown reason=bad-arch ####"
        exit 2
        ;;
esac

worker="/x1/smp-wait-fd-$arch"
eventfd_case="/x1/$arch/musl/ltp-cases/@EVENTFD2_02_SCRIPT@"
echo "#### SMP GATE START phase=$phase arch=$arch profile=wait-fd ####"
if [ ! -x "$worker" ] || ! "$worker"; then
    echo "#### SMP GATE FAIL phase=$phase arch=$arch reason=eventfd-timerfd-worker ####"
    exit 1
fi
if ! "$eventfd_case"; then
    echo "#### SMP GATE FAIL phase=$phase arch=$arch reason=eventfd2-02 ####"
    exit 1
fi
echo "#### SMP GATE PASS phase=$phase arch=$arch ####"
