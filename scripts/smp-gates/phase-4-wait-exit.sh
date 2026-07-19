#!/musl/busybox sh

phase=4
case "${WHUSP_ARCH:-}" in
    rv|la) arch="$WHUSP_ARCH" ;;
    *)
        echo "#### SMP GATE FAIL phase=$phase arch=unknown reason=bad-arch ####"
        exit 2
        ;;
esac

worker="/x1/smp-wait-exit-$arch"
waitpid_case="/x1/$arch/musl/ltp-cases/@WAITPID01_SCRIPT@"
echo "#### SMP GATE START phase=$phase arch=$arch profile=wait-exit ####"
if [ ! -x "$worker" ] || ! "$worker" plain || ! "$worker" fd; then
    echo "#### SMP GATE FAIL phase=$phase arch=$arch reason=exit-worker ####"
    exit 1
fi
if ! "$waitpid_case"; then
    echo "#### SMP GATE FAIL phase=$phase arch=$arch reason=waitpid01 ####"
    exit 1
fi
echo "#### SMP GATE PASS phase=$phase arch=$arch ####"
