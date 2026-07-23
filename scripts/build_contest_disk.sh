#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
image_path="${CONTEST_SCRIPT_DISK:-${1:-$repo_root/disk.img}}"
la_image_path="${CONTEST_SCRIPT_DISK_LA:-$repo_root/disk-la.img}"
image_size="${CONTEST_SCRIPT_DISK_SIZE:-64M}"
script_dir="${CONTEST_SCRIPT_DIR:-$repo_root/contest-case-commands}"
tmp_image="${image_path}.tmp"
lock_path="${CONTEST_SCRIPT_LOCK:-/tmp/whusp-build-contest-disk.lock}"
interactive="${CONTEST_INTERACTIVE:-0}"

exec 9>"$lock_path"
flock 9

exporter_args=(
    --out-dir "$script_dir"
    --force
)
case "$interactive" in
    1|yes|true|on)
        exporter_args+=(--interactive)
        ;;
    0|no|false|off|"")
        ;;
    *)
        echo "CONTEST_INTERACTIVE must be one of 0/1, no/yes, false/true, or off/on: $interactive" >&2
        exit 2
        ;;
esac

python3 "$repo_root/scripts/export_contest_case_scripts.py" \
    "${exporter_args[@]}"

rm -f "$tmp_image"
truncate -s "$image_size" "$tmp_image"
mkfs.ext4 -q -F \
    -N 8192 \
    -O ^orphan_file,^metadata_csum_seed,^metadata_csum,^64bit,^has_journal \
    -d "$script_dir" \
    "$tmp_image"
mv -f "$tmp_image" "$image_path"
if [ "$(realpath -m "$image_path")" != "$(realpath -m "$la_image_path")" ]; then
    cp -f "$image_path" "$la_image_path"
fi

echo "built contest script disk: $image_path"
echo "built LoongArch contest script disk: $la_image_path"
