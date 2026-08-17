#!/usr/bin/env python3

from __future__ import annotations

import argparse
import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPT = REPO_ROOT / "scripts/build_loongarch_board_elf.py"
SPEC = importlib.util.spec_from_file_location("build_loongarch_board_elf", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
BUILDER = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = BUILDER
SPEC.loader.exec_module(BUILDER)

RUNNER_SCRIPT = Path(__file__).resolve().parents[1] / "run_loongarch_board.py"
RUNNER_SPEC = importlib.util.spec_from_file_location(
    "run_loongarch_board_for_elf_tests", RUNNER_SCRIPT
)
assert RUNNER_SPEC is not None and RUNNER_SPEC.loader is not None
RUNNER = importlib.util.module_from_spec(RUNNER_SPEC)
sys.modules[RUNNER_SPEC.name] = RUNNER
RUNNER_SPEC.loader.exec_module(RUNNER)


def write_test_elf(
    path: Path,
    *,
    entry: int = 0x9000000090000000,
    load_address: int = 0x9000000090000000,
    memory_size: int = 0x1000,
) -> None:
    ident = bytearray(16)
    ident[:6] = b"\x7fELF\x02\x01"
    header = BUILDER.ELF_HEADER.pack(
        bytes(ident),
        BUILDER.ET_EXEC,
        BUILDER.EM_LOONGARCH,
        1,
        entry,
        BUILDER.ELF_HEADER.size,
        0,
        0x41,
        BUILDER.ELF_HEADER.size,
        BUILDER.PROGRAM_HEADER.size,
        1,
        0,
        0,
        0,
    )
    program = BUILDER.PROGRAM_HEADER.pack(
        BUILDER.PT_LOAD,
        BUILDER.PF_X | 4,
        0x100,
        load_address,
        load_address,
        4,
        memory_size,
        0x1000,
    )
    path.write_bytes(
        header + program + bytes(0x100 - len(header) - len(program)) + b"TEST"
    )


class LoongsonBoardElfBuilderTests(unittest.TestCase):
    def test_builder_runner_staging_contract_matches(self) -> None:
        config_source = (REPO_ROOT / "os/src/config.rs").read_text()
        self.assertRegex(
            config_source,
            r"LOONGARCH_BOOT_DTB_ADDRESS:\s*usize\s*=\s*0x9000_0000_0a00_0000;",
        )

        expected_staging = 0x9000000002000000
        expected_size = 4 * 1024 * 1024
        expected_fdt = 0x900000000A000000
        expected_fdt_size = 0x10000
        self.assertEqual(BUILDER.KERNEL_STAGING_ADDRESS, expected_staging)
        self.assertEqual(RUNNER.KERNEL_STAGING_ADDRESS, expected_staging)
        self.assertEqual(BUILDER.KERNEL_STAGING_RESERVED_SIZE, expected_size)
        self.assertEqual(RUNNER.KERNEL_STAGING_RESERVED_SIZE, expected_size)
        self.assertEqual(BUILDER.FDT_DESTINATION_ADDRESS, expected_fdt)
        self.assertEqual(RUNNER.FDT_DESTINATION_ADDRESS, expected_fdt)
        self.assertEqual(BUILDER.FDT_COPY_SIZE, expected_fdt_size)
        self.assertEqual(RUNNER.FDT_COPY_SIZE, expected_fdt_size)

    def test_accepts_program_header_elf_in_high_memory_bank(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            elf = Path(temporary) / "kernel-la"
            write_test_elf(elf)

            layout = BUILDER.read_elf_layout(elf)
            BUILDER.validate_board_elf(layout)

            self.assertEqual(layout.entry, 0x9000000090000000)
            self.assertEqual(len(layout.load_segments), 1)
            self.assertEqual(layout.load_segments[0].file_size, 4)

    def test_rejects_load_destination_overlapping_low_staging_area(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            elf = Path(temporary) / "overlap.elf"
            write_test_elf(
                elf,
                entry=BUILDER.KERNEL_STAGING_ADDRESS,
                load_address=BUILDER.KERNEL_STAGING_ADDRESS,
            )

            with self.assertRaisesRegex(ValueError, "staging range overlaps"):
                BUILDER.validate_board_elf(BUILDER.read_elf_layout(elf))

    def test_rejects_elf_larger_than_reserved_staging_envelope(self) -> None:
        BUILDER.validate_staging_size(BUILDER.KERNEL_STAGING_RESERVED_SIZE)
        with self.assertRaisesRegex(ValueError, "reserved 4 MiB staging"):
            BUILDER.validate_staging_size(BUILDER.KERNEL_STAGING_RESERVED_SIZE + 1)

    def test_make_uses_board_feature_and_root_makefile(self) -> None:
        args = argparse.Namespace(
            mode="release",
            perf_counters="1",
            block_io_mode="force-sync",
            cargo_default_features="1",
            board_feature="loongarch-board-2k1000",
            extra_feature=["perf-probe"],
        )

        command = BUILDER.make_command(args)

        self.assertEqual(command[:3], ["make", "--no-print-directory", "kernel-la"])
        self.assertIn("PERF_COUNTERS=1", command)
        self.assertIn("EXTRA_FEATURES=loongarch-board-2k1000 perf-probe", command)

    def test_strip_command_keeps_an_elf_not_a_flat_binary(self) -> None:
        strip = BUILDER.strip_command(
            "/usr/bin/llvm-strip", Path("kernel-la"), Path("whusp-2k1000.elf")
        )
        objcopy = BUILDER.strip_command(
            "/usr/bin/llvm-objcopy", Path("kernel-la"), Path("whusp-2k1000.elf")
        )

        self.assertEqual(
            strip,
            [
                "/usr/bin/llvm-strip",
                "--strip-debug",
                "-o",
                "whusp-2k1000.elf",
                "kernel-la",
            ],
        )
        self.assertEqual(
            objcopy,
            [
                "/usr/bin/llvm-objcopy",
                "--strip-debug",
                "kernel-la",
                "whusp-2k1000.elf",
            ],
        )


if __name__ == "__main__":
    unittest.main()
