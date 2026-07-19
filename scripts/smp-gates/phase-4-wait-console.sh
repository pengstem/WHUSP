#!/musl/busybox sh

phase=4
case "${WHUSP_ARCH:-}" in
    rv|la) arch="$WHUSP_ARCH" ;;
    *)
        echo "#### SMP GATE FAIL phase=$phase arch=unknown reason=bad-arch ####"
        exit 2
        ;;
esac

worker="/x1/smp-wait-console-$arch"
echo "#### SMP GATE START phase=$phase arch=$arch profile=wait-console ####"
if [ ! -x "$worker" ] || ! "$worker"; then
    echo "#### SMP GATE FAIL phase=$phase arch=$arch reason=console-worker ####"
    exit 1
fi
echo "#### SMP GATE PASS phase=$phase arch=$arch ####"
