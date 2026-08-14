#!/usr/bin/env python3
"""Safely flash prepared SD-image chunks through VisionFive 2 U-Boot."""

from __future__ import annotations

import argparse
import fcntl
import ipaddress
import json
import os
import re
import sys
from datetime import datetime
from pathlib import Path, PurePosixPath
from typing import Protocol

import prepare_starfive_sd_image as image_prepare
import run_starfive_cagent as starfive_runner

REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_RESULTS_ROOT = REPO_ROOT / "tools" / "starfive_sd_runs"
SAFE_FILE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")
SAFE_GUEST_PATH = re.compile(r"^/[A-Za-z0-9._/-]*$")
SHA256_PATTERN = re.compile(r"^[0-9a-f]{64}$")
CRC32_PATTERN = re.compile(r"^0x[0-9a-f]{8}$")
TFTP_SIZE_PATTERN = re.compile(rb"Bytes transferred = ([0-9]+)\b")
GZWRITE_RESULT_PATTERN = re.compile(
    rb"(?:^|[\r\n\t ])([0-9]+) bytes, crc 0x([0-9a-fA-F]{8})(?:[\r\n]|$)"
)


class UBootSession(Protocol):
    def command(self, command: str, timeout: float = 10.0) -> bytes: ...


def require_int(value: object, context: str, *, minimum: int = 0) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
        raise ValueError(f"{context} must be an integer >= {minimum}")
    return value


def require_string(value: object, context: str) -> str:
    if not isinstance(value, str):
        raise TypeError(f"{context} must be a string")
    return value


def load_manifest(path: Path) -> dict[str, object]:
    try:
        manifest = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError(f"cannot read image manifest {path}: {error}") from error
    if not isinstance(manifest, dict):
        raise TypeError("image manifest must contain a JSON object")
    validate_manifest(manifest)
    return manifest


def validate_manifest(manifest: dict[str, object]) -> None:
    if manifest.get("schema") != image_prepare.SCHEMA:
        raise ValueError(
            f"unsupported image manifest schema: {manifest.get('schema')!r}"
        )

    image = manifest.get("image")
    chunks = manifest.get("chunks")
    if not isinstance(image, dict) or not isinstance(chunks, list) or not chunks:
        raise ValueError("manifest requires image metadata and at least one chunk")
    image_size = require_int(image.get("size"), "image.size", minimum=1)
    image_sha256 = require_string(image.get("sha256"), "image.sha256")
    if not SHA256_PATTERN.fullmatch(image_sha256):
        raise ValueError("image.sha256 must be a lowercase SHA-256 digest")
    if require_int(image.get("block_size"), "image.block_size", minimum=1) != 512:
        raise ValueError("only 512-byte MMC image blocks are supported")
    max_compressed_size = require_int(
        manifest.get("max_compressed_size"), "max_compressed_size", minimum=1
    )
    if max_compressed_size >= 0x80000000:
        raise ValueError("max_compressed_size must stay below 2 GiB")

    expected_offset = 0
    filenames: set[str] = set()
    for expected_index, item in enumerate(chunks):
        if not isinstance(item, dict):
            raise TypeError(f"chunks[{expected_index}] must be an object")
        index = require_int(item.get("index"), f"chunks[{expected_index}].index")
        if index != expected_index:
            raise ValueError("chunk indices must be contiguous and start at zero")
        filename = require_string(
            item.get("filename"), f"chunks[{expected_index}].filename"
        )
        if not SAFE_FILE.fullmatch(filename) or not filename.endswith(".img.gz"):
            raise ValueError(f"unsafe gzip chunk filename: {filename!r}")
        if filename in filenames:
            raise ValueError(f"duplicate gzip chunk filename: {filename}")
        filenames.add(filename)

        offset = require_int(item.get("offset"), f"chunks[{expected_index}].offset")
        raw_size = require_int(
            item.get("raw_size"), f"chunks[{expected_index}].raw_size", minimum=1
        )
        compressed_size = require_int(
            item.get("compressed_size"),
            f"chunks[{expected_index}].compressed_size",
            minimum=1,
        )
        if offset != expected_offset:
            raise ValueError("chunk offsets must describe one contiguous raw image")
        if offset % 512 or raw_size % 512:
            raise ValueError("chunk offsets and raw sizes must be 512-byte aligned")
        if compressed_size > max_compressed_size:
            raise ValueError(f"{filename} exceeds the manifest gzwrite input limit")
        raw_crc32 = require_string(
            item.get("raw_crc32"), f"chunks[{expected_index}].raw_crc32"
        )
        raw_sha256 = require_string(
            item.get("raw_sha256"), f"chunks[{expected_index}].raw_sha256"
        )
        compressed_sha256 = require_string(
            item.get("compressed_sha256"),
            f"chunks[{expected_index}].compressed_sha256",
        )
        if not CRC32_PATTERN.fullmatch(raw_crc32):
            raise ValueError(f"invalid CRC32 for {filename}")
        if not SHA256_PATTERN.fullmatch(raw_sha256):
            raise ValueError(f"invalid raw SHA-256 for {filename}")
        if not SHA256_PATTERN.fullmatch(compressed_sha256):
            raise ValueError(f"invalid compressed SHA-256 for {filename}")
        if item.get("offset_hex") != f"0x{offset:x}":
            raise ValueError(f"offset_hex mismatch for {filename}")
        if item.get("raw_size_hex") != f"0x{raw_size:x}":
            raise ValueError(f"raw_size_hex mismatch for {filename}")
        expected_offset += raw_size

    if expected_offset != image_size:
        raise ValueError(
            f"chunk coverage {expected_offset} does not match image size {image_size}"
        )


def manifest_chunks(manifest: dict[str, object]) -> list[dict[str, object]]:
    chunks = manifest["chunks"]
    assert isinstance(chunks, list)
    return chunks  # validate_manifest already checked every item.


def verify_local_chunks(manifest: dict[str, object], tftp_root: Path) -> None:
    tftp_root = tftp_root.resolve()
    if not tftp_root.is_dir():
        raise FileNotFoundError(f"TFTP root does not exist: {tftp_root}")
    for chunk in manifest_chunks(manifest):
        filename = str(chunk["filename"])
        path = (tftp_root / filename).resolve()
        if path.parent != tftp_root:
            raise ValueError(f"chunk escapes the TFTP root: {filename}")
        if not path.is_file():
            raise FileNotFoundError(f"prepared chunk does not exist: {path}")
        actual_size = path.stat().st_size
        if actual_size != chunk["compressed_size"]:
            raise ValueError(
                f"compressed size mismatch for {filename}: "
                f"{actual_size} != {chunk['compressed_size']}"
            )
        with path.open("rb") as source:
            if source.read(3) != b"\x1f\x8b\x08":
                raise ValueError(f"gzip header mismatch for {filename}")
        actual_sha256 = image_prepare.sha256_file(path)
        if actual_sha256 != chunk["compressed_sha256"]:
            raise ValueError(f"compressed SHA-256 mismatch for {filename}")


def parse_mmc_info(output: bytes) -> tuple[str, float]:
    text = output.decode("ascii", errors="replace").replace("\r", "")
    name_match = re.search(r"^Name:\s*(\S+)\s*$", text, re.MULTILINE)
    capacity_match = re.search(
        r"^Capacity:\s*([0-9]+(?:\.[0-9]+)?)\s+(KiB|MiB|GiB|TiB)\s*$",
        text,
        re.MULTILINE,
    )
    if not name_match or not capacity_match:
        raise RuntimeError(
            "U-Boot mmc info did not report a parseable name and capacity"
        )
    capacity = float(capacity_match.group(1))
    unit = capacity_match.group(2)
    capacity_gib = (
        capacity
        * {
            "KiB": 1 / (1024 * 1024),
            "MiB": 1 / 1024,
            "GiB": 1,
            "TiB": 1024,
        }[unit]
    )
    return name_match.group(1), capacity_gib


def parse_tftp_size(output: bytes) -> int:
    match = TFTP_SIZE_PATTERN.search(output)
    if not match:
        raise RuntimeError("TFTP did not report 'Bytes transferred'")
    return int(match.group(1))


def parse_gzwrite_result(output: bytes) -> tuple[int, str]:
    matches = list(GZWRITE_RESULT_PATTERN.finditer(output))
    if not matches:
        raise RuntimeError("gzwrite did not report a final byte count and CRC")
    match = matches[-1]
    return int(match.group(1)), "0x" + match.group(2).decode("ascii").lower()


def validate_guest_entry(path: str) -> tuple[str, str]:
    if not SAFE_GUEST_PATH.fullmatch(path) or "//" in path:
        raise ValueError(f"unsafe verification path: {path!r}")
    pure_path = PurePosixPath(path)
    if path == "/" or ".." in pure_path.parts:
        raise ValueError("--verify-entry must name a file below the root directory")
    return str(pure_path.parent), pure_path.name


def build_flash_plan(manifest: dict[str, object], mmc_device: int) -> list[str]:
    commands: list[str] = []
    for chunk in manifest_chunks(manifest):
        commands.append(f"tftpboot ${{kernel_addr_r}} {chunk['filename']}")
        commands.append("md.b ${fileaddr} 3")
        commands.append(
            f"gzwrite mmc {mmc_device} ${{fileaddr}} ${{filesize}} 100000 "
            f"{int(chunk['offset']):x} {int(chunk['raw_size']):x}"
        )
    return commands


def require_gzip_header(output: bytes) -> None:
    normalized = output.lower().replace(b"\r", b" ").replace(b"\n", b" ")
    if not re.search(rb"(?:^|\s)[0-9a-f]+:\s+1f\s+8b\s+08(?:\s|$)", normalized):
        raise RuntimeError(
            "downloaded chunk does not start with the gzip magic 1f 8b 08"
        )


def download_chunk(
    session: UBootSession,
    chunk: dict[str, object],
    *,
    timeout: float,
) -> None:
    filename = str(chunk["filename"])
    transfer = session.command(
        f"tftpboot ${{kernel_addr_r}} {filename}", timeout=timeout
    )
    transferred = parse_tftp_size(transfer)
    if transferred != chunk["compressed_size"]:
        raise RuntimeError(
            f"TFTP size mismatch for {filename}: "
            f"{transferred} != {chunk['compressed_size']}"
        )
    header = session.command("md.b ${fileaddr} 3")
    require_gzip_header(header)


def preflight_board(
    session: UBootSession,
    manifest: dict[str, object],
    *,
    host_ip: str,
    board_ip: str,
    mmc_device: int,
    expected_mmc_name: str,
    expected_capacity_gib: float,
    capacity_tolerance_gib: float,
) -> tuple[str, float]:
    version = session.command("version")
    starfive_runner.require_text(version, b"U-Boot", "U-Boot version")
    help_output = session.command("help gzwrite")
    starfive_runner.require_text(
        help_output, b"unzip and write", "gzwrite availability"
    )
    session.command(f"setenv ipaddr {board_ip}")
    session.command(f"setenv serverip {host_ip}")
    ping = session.command(f"ping {host_ip}", timeout=15.0)
    starfive_runner.require_text(ping, b"is alive", "U-Boot network preflight")

    selected = session.command(f"mmc dev {mmc_device}", timeout=15.0)
    starfive_runner.require_text(
        selected, f"mmc{mmc_device} is current device".encode(), "MMC selection"
    )
    rescan = session.command("mmc rescan", timeout=30.0)
    lowered = rescan.lower()
    if b"no card present" in lowered or b"card did not respond" in lowered:
        raise RuntimeError("U-Boot did not detect the selected SD card")
    info = session.command("mmc info", timeout=15.0)
    actual_name, actual_capacity_gib = parse_mmc_info(info)
    if actual_name != expected_mmc_name:
        raise RuntimeError(
            f"refusing to write unexpected MMC name {actual_name!r}; "
            f"expected {expected_mmc_name!r}"
        )
    if abs(actual_capacity_gib - expected_capacity_gib) > capacity_tolerance_gib:
        raise RuntimeError(
            f"refusing to write unexpected MMC capacity {actual_capacity_gib:.3f} GiB; "
            f"expected {expected_capacity_gib:.3f} +/- {capacity_tolerance_gib:.3f} GiB"
        )
    image = manifest["image"]
    assert isinstance(image, dict)
    image_gib = int(image["size"]) / (1024**3)
    if image_gib > actual_capacity_gib:
        raise RuntimeError(
            f"raw image is {image_gib:.3f} GiB but card reports {actual_capacity_gib:.3f} GiB"
        )
    return actual_name, actual_capacity_gib


def run_board_session(
    session: UBootSession,
    manifest: dict[str, object],
    *,
    host_ip: str,
    board_ip: str,
    mmc_device: int,
    expected_mmc_name: str,
    expected_capacity_gib: float,
    capacity_tolerance_gib: float,
    execute: bool,
    tftp_timeout: float,
    write_timeout: float,
    fs_partition: int,
    verify_entries: list[str],
) -> dict[str, object]:
    actual_name, actual_capacity_gib = preflight_board(
        session,
        manifest,
        host_ip=host_ip,
        board_ip=board_ip,
        mmc_device=mmc_device,
        expected_mmc_name=expected_mmc_name,
        expected_capacity_gib=expected_capacity_gib,
        capacity_tolerance_gib=capacity_tolerance_gib,
    )
    chunks = manifest_chunks(manifest)
    if not execute:
        probe = min(chunks, key=lambda item: int(item["compressed_size"]))
        download_chunk(session, probe, timeout=tftp_timeout)
        return {
            "mode": "preflight",
            "mmc_name": actual_name,
            "mmc_capacity_gib": actual_capacity_gib,
            "probe_chunk": probe["filename"],
        }

    written: list[dict[str, object]] = []
    for chunk in chunks:
        filename = str(chunk["filename"])
        print(f"\nFLASH chunk={chunk['index']} file={filename}")
        download_chunk(session, chunk, timeout=tftp_timeout)
        command = (
            f"gzwrite mmc {mmc_device} ${{fileaddr}} ${{filesize}} 100000 "
            f"{int(chunk['offset']):x} {int(chunk['raw_size']):x}"
        )
        output = session.command(command, timeout=write_timeout)
        raw_size, raw_crc32 = parse_gzwrite_result(output)
        if raw_size != chunk["raw_size"] or raw_crc32 != chunk["raw_crc32"]:
            raise RuntimeError(
                f"gzwrite verification failed for {filename}: "
                f"size={raw_size} crc={raw_crc32}, expected "
                f"size={chunk['raw_size']} crc={chunk['raw_crc32']}"
            )
        written.append(
            {
                "index": chunk["index"],
                "filename": filename,
                "raw_size": raw_size,
                "raw_crc32": raw_crc32,
            }
        )

    session.command(f"mmc dev {mmc_device}", timeout=15.0)
    session.command("mmc rescan", timeout=30.0)
    root = session.command(f"ext4ls mmc {mmc_device}:{fs_partition} /", timeout=60.0)
    for failure in (
        b"** Unrecognized filesystem type **",
        b"Failed to mount ext2 filesystem",
    ):
        if failure in root:
            raise RuntimeError("flashed root filesystem is not readable by U-Boot")
    for guest_entry in verify_entries:
        parent, name = validate_guest_entry(guest_entry)
        listing = session.command(
            f"ext4ls mmc {mmc_device}:{fs_partition} {parent}", timeout=60.0
        )
        if name.encode("ascii") not in listing:
            raise RuntimeError(
                f"post-flash verification entry is missing: {guest_entry}"
            )

    return {
        "mode": "execute",
        "mmc_name": actual_name,
        "mmc_capacity_gib": actual_capacity_gib,
        "written": written,
        "verify_entries": verify_entries,
    }


def print_plan(manifest: dict[str, object], mmc_device: int) -> None:
    image = manifest["image"]
    assert isinstance(image, dict)
    print(
        "DRY RUN: no serial, network, or SD writes will be attempted\n"
        f"image_size={image['size']} image_sha256={image['sha256']}\n"
        f"chunks={len(manifest_chunks(manifest))} mmc_device={mmc_device}"
    )
    for command in build_flash_plan(manifest, mmc_device):
        print(command)


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="verify and optionally flash a prepared image through StarFive U-Boot"
    )
    parser.add_argument("manifest", type=Path)
    parser.add_argument("--tftp-root", type=Path)
    parser.add_argument("--serial", default="/dev/ttyUSB0")
    parser.add_argument("--host-ip", default="192.168.120.1")
    parser.add_argument("--board-ip", default="192.168.120.230")
    parser.add_argument("--mmc-device", type=int, default=1)
    parser.add_argument("--fs-partition", type=int, default=0)
    parser.add_argument("--expect-mmc-name")
    parser.add_argument("--expect-capacity-gib", type=float)
    parser.add_argument("--capacity-tolerance-gib", type=float, default=0.1)
    parser.add_argument("--verify-entry", action="append", default=[])
    parser.add_argument("--prompt-timeout", type=float, default=300.0)
    parser.add_argument("--tftp-timeout", type=float, default=900.0)
    parser.add_argument("--write-timeout", type=float, default=3600.0)
    parser.add_argument("--results-root", type=Path, default=DEFAULT_RESULTS_ROOT)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument(
        "--preflight-only",
        action="store_true",
        help="check serial, network, card identity, and one TFTP chunk without writing",
    )
    mode.add_argument(
        "--execute",
        action="store_true",
        help="perform destructive gzwrite operations after all safety checks",
    )
    parser.add_argument(
        "--confirm-image-sha256",
        help="required with --execute; must equal image.sha256 in the manifest",
    )
    args = parser.parse_args(argv)

    ipaddress.ip_address(args.host_ip)
    ipaddress.ip_address(args.board_ip)
    if args.mmc_device < 0 or args.fs_partition < 0:
        parser.error("MMC device and filesystem partition must be non-negative")
    if args.capacity_tolerance_gib < 0:
        parser.error("capacity tolerance must be non-negative")
    for timeout_name in ("prompt_timeout", "tftp_timeout", "write_timeout"):
        if getattr(args, timeout_name) <= 0:
            parser.error(f"--{timeout_name.replace('_', '-')} must be positive")
    for entry in args.verify_entry:
        try:
            validate_guest_entry(entry)
        except ValueError as error:
            parser.error(str(error))
    if args.execute or args.preflight_only:
        if not args.expect_mmc_name or args.expect_capacity_gib is None:
            parser.error(
                "board access requires --expect-mmc-name and --expect-capacity-gib"
            )
        if not SAFE_FILE.fullmatch(args.expect_mmc_name):
            parser.error("--expect-mmc-name contains unsafe characters")
        if args.expect_capacity_gib <= 0:
            parser.error("--expect-capacity-gib must be positive")
    if args.execute and not args.confirm_image_sha256:
        parser.error("--execute requires --confirm-image-sha256")
    if not args.execute and args.confirm_image_sha256:
        parser.error("--confirm-image-sha256 is only valid with --execute")
    return args


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    manifest_path = args.manifest.resolve()
    manifest = load_manifest(manifest_path)
    tftp_root = (args.tftp_root or manifest_path.parent).resolve()
    verify_local_chunks(manifest, tftp_root)

    image = manifest["image"]
    assert isinstance(image, dict)
    if args.execute and args.confirm_image_sha256 != image["sha256"]:
        raise SystemExit(
            "--confirm-image-sha256 does not match the prepared raw image; refusing to write"
        )
    if not args.execute and not args.preflight_only:
        print_plan(manifest, args.mmc_device)
        return 0

    run_id = datetime.now().astimezone().strftime("%Y%m%d-%H%M%S")
    run_dir = args.results_root.resolve() / run_id
    run_dir.mkdir(parents=True)
    result: dict[str, object] = {
        "run_id": run_id,
        "mode": "execute" if args.execute else "preflight",
        "manifest": str(manifest_path),
        "manifest_sha256": image_prepare.sha256_file(manifest_path),
        "image_sha256": image["sha256"],
        "serial": args.serial,
        "result": "running",
    }
    result_path = run_dir / "result.json"
    result_path.write_text(
        json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )

    fd = os.open(args.serial, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    starfive_runner.configure_serial(fd)
    log = starfive_runner.SerialLog(fd, run_dir / "serial.log")
    try:
        log.interrupt_until_prompt(args.prompt_timeout)
        board_result = run_board_session(
            log,
            manifest,
            host_ip=args.host_ip,
            board_ip=args.board_ip,
            mmc_device=args.mmc_device,
            expected_mmc_name=args.expect_mmc_name,
            expected_capacity_gib=args.expect_capacity_gib,
            capacity_tolerance_gib=args.capacity_tolerance_gib,
            execute=args.execute,
            tftp_timeout=args.tftp_timeout,
            write_timeout=args.write_timeout,
            fs_partition=args.fs_partition,
            verify_entries=args.verify_entry,
        )
        result.update(board_result)
        result["result"] = "pass"
        result["serial_bytes"] = len(log.all_bytes)
        print(
            f"\nStarFive SD {'flash' if args.execute else 'preflight'} passed: {run_dir}"
        )
        return 0
    except BaseException as error:
        result["result"] = "fail"
        result["error"] = f"{type(error).__name__}: {error}"
        result["serial_bytes"] = len(log.all_bytes)
        raise
    finally:
        result_path.write_text(
            json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
        log.close()
        os.close(fd)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        print(
            "\nInterrupted. If gzwrite had started, do not assume the SD image is complete; "
            "restart from chunk 0 with the same manifest.",
            file=sys.stderr,
        )
        raise SystemExit(130)
