#!/usr/bin/env python3
"""Prepare bounded gzip chunks for VisionFive 2 U-Boot ``gzwrite``.

The source image is streamed into independently verifiable gzip members.  A raw
image is never materialized when the input is already compressed, which keeps
the host-side disk-space requirement close to the size of the generated chunks.
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import lzma
import os
import re
import shutil
import subprocess
import tempfile
import zlib
from collections.abc import Iterator
from contextlib import contextmanager
from datetime import datetime
from pathlib import Path
from typing import BinaryIO

SCHEMA = "whusp-starfive-sd-image-v1"
DEFAULT_CHUNK_SIZE = 3584 * 1024 * 1024  # 3.5 GiB; validated on the 4 GiB VF2.
DEFAULT_MAX_COMPRESSED_SIZE = 0x7F000000  # Stay below gzwrite's signed 2 GiB edge.
COPY_BUFFER_SIZE = 4 * 1024 * 1024
BLOCK_SIZE = 512
SAFE_PREFIX = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")
SIZE_PATTERN = re.compile(r"^(\d+)([KMGTP]?i?B)?$", re.IGNORECASE)


def parse_size(value: str) -> int:
    match = SIZE_PATTERN.fullmatch(value.strip())
    if not match:
        raise argparse.ArgumentTypeError(
            f"invalid size {value!r}; use bytes or a suffix such as MiB/GiB"
        )
    amount = int(match.group(1))
    suffix = (match.group(2) or "B").upper()
    powers = {
        "B": 0,
        "KB": 1,
        "KIB": 1,
        "MB": 2,
        "MIB": 2,
        "GB": 3,
        "GIB": 3,
        "TB": 4,
        "TIB": 4,
        "PB": 5,
        "PIB": 5,
    }
    return amount * (1024 ** powers[suffix])


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for data in iter(lambda: source.read(COPY_BUFFER_SIZE), b""):
            digest.update(data)
    return digest.hexdigest()


def detect_format(path: Path, requested: str) -> str:
    if requested != "auto":
        return requested
    lower_name = path.name.lower()
    if lower_name.endswith((".gz", ".gzip")):
        return "gzip"
    if lower_name.endswith((".xz", ".lzma")):
        return "xz"
    if lower_name.endswith(".7z"):
        return "7z"
    return "raw"


def select_compressor(requested: str) -> str:
    if requested == "auto":
        return "pigz" if shutil.which("pigz") else "python"
    if requested == "pigz" and shutil.which("pigz") is None:
        raise RuntimeError("--compressor pigz requires the offline pigz executable")
    return requested


@contextmanager
def open_image_source(
    path: Path, source_format: str, archive_member: str | None
) -> Iterator[BinaryIO]:
    if source_format == "raw":
        with path.open("rb") as source:
            yield source
        return
    if source_format == "gzip":
        with gzip.open(path, "rb") as source:
            yield source
        return
    if source_format == "xz":
        with lzma.open(path, "rb") as source:
            yield source
        return
    if source_format != "7z":
        raise ValueError(f"unsupported source format: {source_format}")

    executable = shutil.which("7z") or shutil.which("7zz")
    if executable is None:
        raise RuntimeError("7z input requires the offline 7z or 7zz executable")
    if not archive_member:
        raise RuntimeError("7z input requires --archive-member with the image filename")
    if archive_member.startswith("-") or "\x00" in archive_member:
        raise RuntimeError("unsafe 7z archive member name")

    with tempfile.TemporaryFile() as error_file:
        process = subprocess.Popen(
            [
                executable,
                "x",
                "-so",
                "-bd",
                "-bb0",
                "--",
                str(path),
                archive_member,
            ],
            stdout=subprocess.PIPE,
            stderr=error_file,
        )
        assert process.stdout is not None
        try:
            yield process.stdout
        except BaseException:
            process.kill()
            process.wait()
            raise
        finally:
            process.stdout.close()
        return_code = process.wait()
        if return_code:
            error_file.seek(0)
            detail = error_file.read().decode("utf-8", errors="replace").strip()
            raise RuntimeError(
                f"7z extraction failed with status {return_code}: {detail}"
            )


@contextmanager
def compressed_writer(
    path: Path, compressor: str, compression_level: int
) -> Iterator[BinaryIO]:
    if compressor == "python":
        with (
            path.open("xb") as output,
            gzip.GzipFile(
                filename="",
                mode="wb",
                compresslevel=compression_level,
                fileobj=output,
                mtime=0,
            ) as encoded,
        ):
            yield encoded
        return

    pigz = shutil.which("pigz")
    if compressor != "pigz" or pigz is None:
        raise RuntimeError("--compressor pigz requires the offline pigz executable")
    with path.open("xb") as output, tempfile.TemporaryFile() as error_file:
        process = subprocess.Popen(
            [pigz, f"-{compression_level}", "-n", "-c"],
            stdin=subprocess.PIPE,
            stdout=output,
            stderr=error_file,
        )
        assert process.stdin is not None
        try:
            yield process.stdin
        except BaseException:
            process.kill()
            process.wait()
            raise
        finally:
            process.stdin.close()
        return_code = process.wait()
        if return_code:
            error_file.seek(0)
            detail = error_file.read().decode("utf-8", errors="replace").strip()
            raise RuntimeError(f"pigz failed with status {return_code}: {detail}")


def verify_gzip_chunk(
    path: Path, raw_size: int, raw_sha256: str, raw_crc32: int
) -> None:
    size = 0
    crc = 0
    digest = hashlib.sha256()
    with gzip.open(path, "rb") as source:
        for data in iter(lambda: source.read(COPY_BUFFER_SIZE), b""):
            size += len(data)
            crc = zlib.crc32(data, crc)
            digest.update(data)
    if size != raw_size:
        raise RuntimeError(
            f"gzip verification size mismatch for {path}: {size} != {raw_size}"
        )
    if digest.hexdigest() != raw_sha256:
        raise RuntimeError(f"gzip verification SHA-256 mismatch for {path}")
    if (crc & 0xFFFFFFFF) != raw_crc32:
        raise RuntimeError(f"gzip verification CRC32 mismatch for {path}")


def write_manifest_atomic(path: Path, manifest: dict[str, object]) -> None:
    partial = path.parent / f".{path.name}.{os.getpid()}.partial"
    try:
        partial.write_text(
            json.dumps(manifest, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        os.link(partial, path)
    finally:
        partial.unlink(missing_ok=True)


def prepare_image(
    source_path: Path,
    output_dir: Path,
    *,
    prefix: str,
    chunk_size: int = DEFAULT_CHUNK_SIZE,
    max_compressed_size: int = DEFAULT_MAX_COMPRESSED_SIZE,
    source_format: str = "auto",
    archive_member: str | None = None,
    compressor: str = "auto",
    compression_level: int = 9,
    verify: bool = True,
) -> Path:
    source_path = source_path.resolve()
    output_dir = output_dir.resolve()
    if not source_path.is_file():
        raise FileNotFoundError(f"source image does not exist: {source_path}")
    if not SAFE_PREFIX.fullmatch(prefix):
        raise ValueError(
            "prefix must contain only ASCII letters, digits, dot, dash, underscore"
        )
    if chunk_size <= 0 or chunk_size % BLOCK_SIZE:
        raise ValueError("chunk size must be a positive multiple of 512 bytes")
    if max_compressed_size <= 0 or max_compressed_size >= 0x80000000:
        raise ValueError("maximum compressed size must be below 2 GiB")
    if not 1 <= compression_level <= 9:
        raise ValueError("compression level must be in 1..9")

    output_dir.mkdir(parents=True, exist_ok=True)
    manifest_path = output_dir / f"{prefix}-manifest.json"
    conflicts = sorted(output_dir.glob(f"{prefix}-part*.img.gz"))
    if manifest_path.exists() or conflicts:
        names = [manifest_path.name] if manifest_path.exists() else []
        names.extend(path.name for path in conflicts[:3])
        raise FileExistsError(
            "refusing to replace an existing prepared image: " + ", ".join(names)
        )

    selected_format = detect_format(source_path, source_format)
    selected_compressor = select_compressor(compressor)
    source_digest = sha256_file(source_path)
    image_digest = hashlib.sha256()
    created_paths: list[Path] = []
    chunks: list[dict[str, object]] = []
    total_size = 0

    try:
        with open_image_source(source_path, selected_format, archive_member) as source:
            index = 0
            while True:
                first = source.read(min(COPY_BUFFER_SIZE, chunk_size))
                if not first:
                    break
                filename = f"{prefix}-part{index:03d}.img.gz"
                final_path = output_dir / filename
                partial_path = output_dir / f".{filename}.{os.getpid()}.partial"
                raw_digest = hashlib.sha256()
                raw_crc = 0
                raw_size = 0
                offset = total_size
                created_paths.append(partial_path)
                with compressed_writer(
                    partial_path, selected_compressor, compression_level
                ) as encoded:
                    data = first
                    while data:
                        encoded.write(data)
                        raw_size += len(data)
                        total_size += len(data)
                        raw_crc = zlib.crc32(data, raw_crc)
                        raw_digest.update(data)
                        image_digest.update(data)
                        remaining = chunk_size - raw_size
                        if remaining == 0:
                            break
                        data = source.read(min(COPY_BUFFER_SIZE, remaining))

                compressed_size = partial_path.stat().st_size
                if compressed_size > max_compressed_size:
                    raise RuntimeError(
                        f"{filename} is {compressed_size} bytes, above the safe gzwrite "
                        f"limit {max_compressed_size}; rerun with a smaller --chunk-size"
                    )
                raw_sha256 = raw_digest.hexdigest()
                raw_crc &= 0xFFFFFFFF
                if verify:
                    verify_gzip_chunk(partial_path, raw_size, raw_sha256, raw_crc)
                compressed_sha256 = sha256_file(partial_path)
                os.link(partial_path, final_path)
                partial_path.unlink()
                created_paths[-1] = final_path
                chunks.append(
                    {
                        "index": index,
                        "filename": filename,
                        "offset": offset,
                        "offset_hex": f"0x{offset:x}",
                        "raw_size": raw_size,
                        "raw_size_hex": f"0x{raw_size:x}",
                        "raw_crc32": f"0x{raw_crc:08x}",
                        "raw_sha256": raw_sha256,
                        "compressed_size": compressed_size,
                        "compressed_sha256": compressed_sha256,
                    }
                )
                print(
                    f"prepared {filename}: raw={raw_size} compressed={compressed_size} "
                    f"crc32=0x{raw_crc:08x}"
                )
                index += 1

        if not chunks:
            raise RuntimeError("source image is empty")
        if total_size % BLOCK_SIZE:
            raise RuntimeError(
                f"raw image size {total_size} is not aligned to a 512-byte MMC block"
            )

        manifest: dict[str, object] = {
            "schema": SCHEMA,
            "created_at": datetime.now().astimezone().isoformat(timespec="seconds"),
            "source": {
                "filename": source_path.name,
                "format": selected_format,
                "archive_member": archive_member,
                "size": source_path.stat().st_size,
                "sha256": source_digest,
            },
            "image": {
                "size": total_size,
                "sha256": image_digest.hexdigest(),
                "block_size": BLOCK_SIZE,
            },
            "chunk_size": chunk_size,
            "max_compressed_size": max_compressed_size,
            "compression": {
                "format": "gzip",
                "level": compression_level,
                "compressor": selected_compressor,
            },
            "uboot": {
                "writer": "gzwrite",
                "write_buffer_hex": "0x100000",
            },
            "chunks": chunks,
        }
        write_manifest_atomic(manifest_path, manifest)
        print(
            f"manifest: {manifest_path}\n"
            f"raw image: {total_size} bytes sha256={image_digest.hexdigest()}"
        )
        return manifest_path
    except BaseException:
        for path in reversed(created_paths):
            path.unlink(missing_ok=True)
        raise


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="stream a raw/gzip/xz/7z SD image into safe U-Boot gzip chunks"
    )
    parser.add_argument("source", type=Path)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--prefix", default="starfive-sd")
    parser.add_argument("--chunk-size", type=parse_size, default=DEFAULT_CHUNK_SIZE)
    parser.add_argument(
        "--max-compressed-size",
        type=parse_size,
        default=DEFAULT_MAX_COMPRESSED_SIZE,
    )
    parser.add_argument(
        "--source-format",
        choices=["auto", "raw", "gzip", "xz", "7z"],
        default="auto",
    )
    parser.add_argument(
        "--archive-member",
        help="image member inside a 7z archive; required for 7z input",
    )
    parser.add_argument(
        "--compressor", choices=["auto", "python", "pigz"], default="auto"
    )
    parser.add_argument("--compression-level", type=int, default=9)
    parser.add_argument(
        "--skip-verify",
        action="store_true",
        help="skip the post-compression decompression check (not recommended)",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    prepare_image(
        args.source,
        args.output_dir,
        prefix=args.prefix,
        chunk_size=args.chunk_size,
        max_compressed_size=args.max_compressed_size,
        source_format=args.source_format,
        archive_member=args.archive_member,
        compressor=args.compressor,
        compression_level=args.compression_level,
        verify=not args.skip_verify,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
