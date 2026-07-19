#!/musl/busybox sh

phase=4
case "${WHUSP_ARCH:-}" in
    rv|la) arch="$WHUSP_ARCH" ;;
    *)
        echo "#### SMP GATE FAIL phase=$phase arch=unknown reason=bad-arch ####"
        exit 2
        ;;
esac

worker="/x1/smp-wait-sem-$arch"
semop_case="/x1/$arch/musl/ltp-cases/@SEMOP01_SCRIPT@"
echo "#### SMP GATE START phase=$phase arch=$arch profile=wait-sem ####"
if [ ! -x "$worker" ] || ! "$worker"; then
    echo "#### SMP GATE FAIL phase=$phase arch=$arch reason=sem-worker ####"
    exit 1
fi
if ! "$semop_case"; then
    echo "#### SMP GATE FAIL phase=$phase arch=$arch reason=semop01 ####"
    exit 1
fi
echo "#### SMP GATE PASS phase=$phase arch=$arch ####"
