#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
OUT_DIR="${1:-$(dirname "$SCRIPT_DIR")}"

for name in sdcard-rv sdcard-la; do
    if [ -f "$OUT_DIR/${name}.img" ]; then
        echo "${name}.img already exists, skipping"
        continue
    fi
    echo "Reassembling and decompressing ${name}.img..."
    cat "$SCRIPT_DIR/${name}.img.xz."* | xz -d > "$OUT_DIR/${name}.img"
    echo "  -> $OUT_DIR/${name}.img ($(du -h "$OUT_DIR/${name}.img" | cut -f1))"
done
