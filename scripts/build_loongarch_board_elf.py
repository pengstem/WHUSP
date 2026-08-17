#!/usr/bin/env python3
"""Build and validate the program-header ELF used by LS2K1000 U-Boot.

U-Boot receives this file at a low-memory staging address and ``bootelf -p``
copies its PT_LOAD segments to their physical addresses.  The validation here
is deliberately independent of section headers: stripping debug sections is
safe only when the entry point and every program header remain unchanged.
"""

from __future__ import annotations

import argparse
import dataclasses
import hashlib
import json
import os
import re
import shutil
import struct
import subprocess
import tempfile
from datetime import datetime
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_SOURCE = REPO_ROOT / "kernel-la"
DEFAULT_TFTP_ROOT = Path("/tmp/whusp-starfive-tftp")
DEFAULT_OUTPUT_NAME = "whusp-2k1000.elf"
DEFAULT_BOARD_FEATURE = "loongarch-board-2k1000"
KERNEL_STAGING_ADDRESS = 0x9000000002000000
KERNEL_STAGING_RESERVED_SIZE = 4 * 1024 * 1024
FDT_DESTINATION_ADDRESS = 0x900000000A000000
FDT_COPY_SIZE = 0x10000
EM_LOONGARCH = 258
ET_EXEC = 2
PT_LOAD = 1
PF_X = 1
BOARD_MEMORY_BANKS = (
    (0x9000000000000000, 0x10000000),
    (0x9000000090000000, 0x30000000),
)
ELF_HEADER = struct.Struct("<16sHHIQQQIHHHHHH")
PROGRAM_HEADER = struct.Struct("<IIQQQQQQ")
SAFE_FILE = re.compile(r"^[A-Za-z0-9._-]+$")


@dataclasses.dataclass(frozen=True)
class ProgramHeader:
    type: int
    flags: int
    offset: int
    virtual_address: int
    physical_address: int
    file_size: int
    memory_size: int
    alignment: int

    @property
    def load_address(self) -> int:
        return self.physical_address


@dataclasses.dataclass(frozen=True)
class ElfLayout:
    entry: int
    machine: int
    elf_type: int
    file_size: int
    program_headers: tuple[ProgramHeader, ...]

    @property
    def load_segments(self) -> tuple[ProgramHeader, ...]:
        return tuple(
            header for header in self.program_headers if header.type == PT_LOAD
        )


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source", type=Path, default=DEFAULT_SOURCE)
    parser.add_argument("--tftp-root", type=Path, default=DEFAULT_TFTP_ROOT)
    parser.add_argument("--output-name", default=DEFAULT_OUTPUT_NAME)
    parser.add_argument("--no-build", action="store_true")
    parser.add_argument("--mode", choices=("debug", "release"), default="release")
    parser.add_argument("--perf-counters", choices=("0", "1"), default="0")
    parser.add_argument(
        "--block-io-mode", choices=("auto", "force-sync"), default="force-sync"
    )
    parser.add_argument("--cargo-default-features", choices=("0", "1"), default="1")
    parser.add_argument("--board-feature", default=DEFAULT_BOARD_FEATURE)
    parser.add_argument("--extra-feature", action="append", default=[])
    parser.add_argument(
        "--strip-tool",
        help="llvm-strip/compatible strip, or llvm-objcopy/compatible objcopy",
    )
    args = parser.parse_args()
    if not SAFE_FILE.fullmatch(args.output_name):
        parser.error("--output-name must be a plain filename")
    return args


def read_elf_layout(path: Path) -> ElfLayout:
    file_size = path.stat().st_size
    with path.open("rb") as source:
        raw_header = source.read(ELF_HEADER.size)
        if len(raw_header) != ELF_HEADER.size:
            raise ValueError(f"ELF header is truncated: {path}")
        fields = ELF_HEADER.unpack(raw_header)
        ident = fields[0]
        if ident[:4] != b"\x7fELF" or ident[4] != 2 or ident[5] != 1:
            raise ValueError(f"expected a little-endian ELF64 file: {path}")
        elf_type, machine, entry = fields[1], fields[2], fields[4]
        phoff, phentsize, phnum = fields[5], fields[9], fields[10]
        if phentsize != PROGRAM_HEADER.size:
            raise ValueError(f"unexpected ELF64 program-header size: {phentsize}")
        if phnum == 0:
            raise ValueError("ELF contains no program headers")
        table_end = phoff + phentsize * phnum
        if phoff < ELF_HEADER.size or table_end > file_size:
            raise ValueError("ELF program-header table lies outside the file")
        source.seek(phoff)
        headers = []
        for _index in range(phnum):
            raw_program = source.read(phentsize)
            if len(raw_program) != phentsize:
                raise ValueError("ELF program-header table is truncated")
            values = PROGRAM_HEADER.unpack(raw_program)
            headers.append(
                ProgramHeader(
                    type=values[0],
                    flags=values[1],
                    offset=values[2],
                    virtual_address=values[3],
                    physical_address=values[4],
                    file_size=values[5],
                    memory_size=values[6],
                    alignment=values[7],
                )
            )
    return ElfLayout(
        entry=entry,
        machine=machine,
        elf_type=elf_type,
        file_size=file_size,
        program_headers=tuple(headers),
    )


def ranges_overlap(start_a: int, size_a: int, start_b: int, size_b: int) -> bool:
    return start_a < start_b + size_b and start_b < start_a + size_a


def range_is_in_board_memory(start: int, size: int) -> bool:
    end = start + size
    return any(
        start >= bank_start and end <= bank_start + bank_size
        for bank_start, bank_size in BOARD_MEMORY_BANKS
    )


def validate_staging_size(file_size: int) -> None:
    if file_size <= 0:
        raise ValueError("kernel ELF must not be empty")
    if file_size > KERNEL_STAGING_RESERVED_SIZE:
        raise ValueError(
            "kernel ELF exceeds the reserved 4 MiB staging envelope: "
            f"bytes={file_size} limit={KERNEL_STAGING_RESERVED_SIZE}"
        )


def validate_board_elf(layout: ElfLayout) -> None:
    if layout.machine != EM_LOONGARCH or layout.elf_type != ET_EXEC:
        raise ValueError(
            "expected a LoongArch ET_EXEC ELF: "
            f"machine={layout.machine} type={layout.elf_type}"
        )
    loads = layout.load_segments
    if not loads:
        raise ValueError("ELF contains no PT_LOAD segments")
    entry_is_executable = False
    for segment in loads:
        if segment.file_size > segment.memory_size:
            raise ValueError("PT_LOAD file size exceeds its memory size")
        if segment.offset + segment.file_size > layout.file_size:
            raise ValueError("PT_LOAD file range lies outside the ELF")
        if segment.memory_size == 0:
            continue
        if not range_is_in_board_memory(segment.load_address, segment.memory_size):
            raise ValueError(
                "PT_LOAD is outside the LS2K1000 cached memory banks: "
                f"start=0x{segment.load_address:x} size=0x{segment.memory_size:x}"
            )
        if ranges_overlap(
            KERNEL_STAGING_ADDRESS,
            KERNEL_STAGING_RESERVED_SIZE,
            segment.load_address,
            segment.memory_size,
        ):
            raise ValueError("ELF staging range overlaps a PT_LOAD destination")
        if ranges_overlap(
            FDT_DESTINATION_ADDRESS,
            FDT_COPY_SIZE,
            segment.load_address,
            segment.memory_size,
        ):
            raise ValueError("live FDT copy overlaps a PT_LOAD destination")
        if (
            segment.flags & PF_X
            and layout.entry >= segment.virtual_address
            and layout.entry < segment.virtual_address + segment.memory_size
        ):
            entry_is_executable = True
    if not entry_is_executable:
        raise ValueError("ELF entry point is not inside an executable PT_LOAD")
    if KERNEL_STAGING_ADDRESS + KERNEL_STAGING_RESERVED_SIZE > FDT_DESTINATION_ADDRESS:
        raise ValueError("reserved ELF staging envelope overlaps the live FDT copy")


def make_command(args: argparse.Namespace) -> list[str]:
    features = [args.board_feature, *args.extra_feature]
    return [
        "make",
        "--no-print-directory",
        "kernel-la",
        f"MODE={args.mode}",
        f"PERF_COUNTERS={args.perf_counters}",
        f"BLOCK_IO_MODE={args.block_io_mode}",
        f"CARGO_DEFAULT_FEATURES={args.cargo_default_features}",
        "EXTRA_FEATURES=" + " ".join(feature for feature in features if feature),
    ]


def find_strip_tool(requested: str | None) -> str:
    candidates = [requested] if requested else ["llvm-strip", "llvm-objcopy"]
    for candidate in candidates:
        if not candidate:
            continue
        resolved = shutil.which(candidate)
        if resolved:
            return resolved
    raise FileNotFoundError("llvm-strip or llvm-objcopy is required")


def strip_command(tool: str, source: Path, output: Path) -> list[str]:
    if "objcopy" in Path(tool).name:
        return [tool, "--strip-debug", str(source), str(output)]
    return [tool, "--strip-debug", "-o", str(output), str(source)]


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def layout_json(layout: ElfLayout) -> dict[str, object]:
    return {
        "entry": f"0x{layout.entry:x}",
        "file_bytes": layout.file_size,
        "program_headers": [
            {
                **dataclasses.asdict(header),
                "virtual_address_hex": f"0x{header.virtual_address:x}",
                "physical_address_hex": f"0x{header.physical_address:x}",
            }
            for header in layout.program_headers
        ],
    }


def main() -> int:
    args = parse_args()
    build = None
    if not args.no_build:
        build = make_command(args)
        subprocess.run(build, cwd=REPO_ROOT, check=True)
    source = args.source.resolve()
    if not source.is_file():
        raise SystemExit(f"LoongArch kernel ELF does not exist: {source}")

    source_layout = read_elf_layout(source)
    validate_board_elf(source_layout)
    output = args.tftp_root.resolve() / args.output_name
    output.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary_name = tempfile.mkstemp(
        prefix=f".{args.output_name}.", suffix=".tmp", dir=output.parent
    )
    os.close(fd)
    temporary = Path(temporary_name)
    try:
        tool = find_strip_tool(args.strip_tool)
        strip = strip_command(tool, source, temporary)
        subprocess.run(strip, cwd=REPO_ROOT, check=True)
        stripped_layout = read_elf_layout(temporary)
        validate_staging_size(stripped_layout.file_size)
        validate_board_elf(stripped_layout)
        if source_layout.entry != stripped_layout.entry:
            raise ValueError("stripping changed the ELF entry point")
        if source_layout.program_headers != stripped_layout.program_headers:
            raise ValueError("stripping changed the ELF program headers")
        # mkstemp starts at 0600; dnsmasq commonly drops privileges before
        # reading the TFTP root, so the final immutable artifact must be public.
        temporary.chmod(0o644)
        os.replace(temporary, output)
    finally:
        temporary.unlink(missing_ok=True)

    manifest = {
        "built_at": datetime.now().astimezone().isoformat(),
        "build_command": build,
        "source": str(source),
        "source_bytes": source_layout.file_size,
        "source_sha256": sha256(source),
        "strip_command": strip,
        "output": str(output),
        "output_bytes": stripped_layout.file_size,
        "output_sha256": sha256(output),
        "kernel_staging_address": f"0x{KERNEL_STAGING_ADDRESS:x}",
        "kernel_staging_reserved_bytes": KERNEL_STAGING_RESERVED_SIZE,
        "fdt_destination_address": f"0x{FDT_DESTINATION_ADDRESS:x}",
        "layout": layout_json(stripped_layout),
    }
    manifest_path = output.with_name(output.name + ".json")
    manifest_path.write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    print(
        "built Loongson bootelf artifact: "
        f"{output} ({source_layout.file_size} -> {stripped_layout.file_size} bytes)"
    )
    print(f"validation manifest: {manifest_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
