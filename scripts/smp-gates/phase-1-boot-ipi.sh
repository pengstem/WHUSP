#!/musl/busybox sh

phase=1
case "${WHUSP_ARCH:-}" in
    rv|la) arch="$WHUSP_ARCH" ;;
    *)
        echo "#### SMP GATE FAIL phase=$phase arch=unknown reason=bad-arch ####"
        exit 2
        ;;
esac

echo "#### SMP GATE START phase=$phase arch=$arch profile=boot-ipi ####"
if ! /musl/busybox true; then
    echo "#### SMP GATE FAIL phase=$phase arch=$arch reason=busybox-true ####"
    exit 1
fi
echo "#### SMP GATE PASS phase=$phase arch=$arch ####"
