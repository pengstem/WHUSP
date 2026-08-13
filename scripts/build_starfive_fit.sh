#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
kernel_elf="${STARFIVE_KERNEL_ELF:-$repo_root/kernel-rv}"
runner_disk="${STARFIVE_RUNNER_DISK:-$repo_root/disk.img}"
output="${STARFIVE_FIT_OUTPUT:-/tmp/whusp-starfive-tftp/whusp-cagent.itb}"
template="$repo_root/scripts/starfive-fit.its"

for command in riscv64-linux-gnu-objcopy dtc fdtget sha256sum; do
    command -v "$command" >/dev/null || {
        echo "missing required command: $command" >&2
        exit 1
    }
done
for input in "$kernel_elf" "$runner_disk" "$template"; do
    if [ ! -f "$input" ]; then
        echo "missing StarFive FIT input: $input" >&2
        exit 1
    fi
done

tmp_dir="$(mktemp -d /tmp/whusp-starfive-fit.XXXXXX)"
trap 'rm -rf "$tmp_dir"' EXIT
mkdir -p "$(dirname "$output")"
riscv64-linux-gnu-objcopy -O binary "$kernel_elf" "$tmp_dir/kernel.bin"
cp "$runner_disk" "$tmp_dir/disk.img"
cp "$template" "$tmp_dir/image.its"
(
    cd "$tmp_dir"
    dtc -I dts -O dtb -p 0x1000 -o image.itb image.its
)
mv "$tmp_dir/image.itb" "$output"

test "$(fdtget -t s "$output" /images/kernel type)" = kernel
test "$(fdtget -t s "$output" /images/ramdisk type)" = ramdisk
test "$(fdtget -t s "$output" /configurations default)" = conf
test "$(fdtget -t x "$output" /images/kernel load)" = "0 80200000"
test "$(fdtget -t x "$output" /images/kernel entry)" = "0 80200000"
test "$(fdtget -t x "$output" /images/ramdisk load)" = "0 46100000"

echo "built StarFive FIT: $output"
sha256sum "$output"
