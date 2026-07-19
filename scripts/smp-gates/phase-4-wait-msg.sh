#!/musl/busybox sh

phase=4
case "${WHUSP_ARCH:-}" in
    rv|la) arch="$WHUSP_ARCH" ;;
    *)
        echo "#### SMP GATE FAIL phase=$phase arch=unknown reason=bad-arch ####"
        exit 2
        ;;
esac

worker="/x1/smp-wait-msg-$arch"
msgrcv_case="/x1/$arch/musl/ltp-cases/@MSGRCV01_SCRIPT@"
echo "#### SMP GATE START phase=$phase arch=$arch profile=wait-msg ####"
if [ ! -x "$worker" ] || ! "$worker"; then
    echo "#### SMP GATE FAIL phase=$phase arch=$arch reason=msg-worker ####"
    exit 1
fi
if ! "$msgrcv_case"; then
    echo "#### SMP GATE FAIL phase=$phase arch=$arch reason=msgrcv01 ####"
    exit 1
fi
echo "#### SMP GATE PASS phase=$phase arch=$arch ####"
