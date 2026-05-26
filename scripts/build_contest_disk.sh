#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
image_path="${CONTEST_SCRIPT_DISK:-${1:-$repo_root/disk.img}}"
image_size="${CONTEST_SCRIPT_DISK_SIZE:-64M}"
script_dir="${CONTEST_SCRIPT_DIR:-$repo_root/contest-case-commands}"
tmp_image="${image_path}.tmp"

python3 "$repo_root/scripts/export_contest_case_scripts.py" \
    --out-dir "$script_dir" \
    --force

rm -f "$tmp_image"
truncate -s "$image_size" "$tmp_image"
mkfs.ext4 -q -F \
    -N 8192 \
    -O ^orphan_file,^metadata_csum_seed,^metadata_csum,^64bit,^has_journal \
    -d "$script_dir" \
    "$tmp_image"
mv -f "$tmp_image" "$image_path"

echo "built contest script disk: $image_path"
