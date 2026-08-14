from __future__ import annotations

import contextlib
import io
import re
import sys
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT / "scripts"))

import flash_starfive_sd as flash
import prepare_starfive_sd_image as prepare


class FakeUBoot:
    def __init__(
        self,
        manifest: dict[str, object],
        *,
        mmc_name: str = "SK64G",
        bad_crc: bool = False,
    ):
        self.manifest = manifest
        self.mmc_name = mmc_name
        self.bad_crc = bad_crc
        self.commands: list[str] = []

    def command(self, command: str, timeout: float = 10.0) -> bytes:
        del timeout
        self.commands.append(command)
        if command == "version":
            return b"U-Boot 2021.10\r\nStarFive # "
        if command == "help gzwrite":
            return b"gzwrite - unzip and write memory to block device\r\nStarFive # "
        if command.startswith("setenv "):
            return command.encode() + b"\r\nStarFive # "
        if command.startswith("ping "):
            return b"host 192.168.120.1 is alive\r\nStarFive # "
        if command.startswith("mmc dev "):
            device = command.split()[-1]
            return f"mmc{device} is current device\r\nStarFive # ".encode()
        if command == "mmc rescan":
            return b"StarFive # "
        if command == "mmc info":
            return (
                b"Device: sdio1@16020000\r\n"
                + f"Name: {self.mmc_name} \r\n".encode()
                + b"Capacity: 59.5 GiB\r\nStarFive # "
            )
        if command.startswith("tftpboot "):
            filename = command.split()[-1]
            chunk = next(
                item
                for item in flash.manifest_chunks(self.manifest)
                if item["filename"] == filename
            )
            return (
                f"Bytes transferred = {chunk['compressed_size']} "
                f"({int(chunk['compressed_size']):x} hex)\r\nStarFive # "
            ).encode()
        if command == "md.b ${fileaddr} 3":
            return b"40200000: 1f 8b 08\r\nStarFive # "
        if command.startswith("gzwrite "):
            match = re.search(r" ([0-9a-f]+) ([0-9a-f]+)$", command)
            assert match is not None
            offset = int(match.group(1), 16)
            raw_size = int(match.group(2), 16)
            chunk = next(
                item
                for item in flash.manifest_chunks(self.manifest)
                if item["offset"] == offset
            )
            assert raw_size == chunk["raw_size"]
            crc = "0x00000000" if self.bad_crc else chunk["raw_crc32"]
            return f"\t{raw_size} bytes, crc {crc}\r\nStarFive # ".encode()
        if command.startswith("ext4ls "):
            return (
                b"<DIR> 4096 glibc\r\n"
                b"3150 cagent_testcode.sh\r\n"
                b"3286 buildstorm_testcode.sh\r\nStarFive # "
            )
        raise AssertionError(f"unexpected fake U-Boot command: {command}")


class FlashStarFiveSdTests(unittest.TestCase):
    def prepared_image(self, root: Path) -> tuple[Path, dict[str, object]]:
        source = root / "official.img"
        output = root / "tftp"
        source.write_bytes(bytes(range(256)) * 8)
        manifest_path = prepare.prepare_image(
            source,
            output,
            prefix="test-sd",
            chunk_size=1024,
            max_compressed_size=4096,
            compressor="python",
        )
        return manifest_path, flash.load_manifest(manifest_path)

    def test_local_dry_run_validates_chunks_without_serial_or_network(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            manifest_path, manifest = self.prepared_image(root)
            flash.verify_local_chunks(manifest, manifest_path.parent)
            with contextlib.redirect_stdout(io.StringIO()) as output:
                status = flash.main([str(manifest_path)])
            self.assertEqual(status, 0)
            self.assertIn("DRY RUN", output.getvalue())
            self.assertIn("gzwrite mmc 1", output.getvalue())

    def test_execute_checks_every_tftp_size_and_gzwrite_crc(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            _, manifest = self.prepared_image(Path(temp_dir))
            session = FakeUBoot(manifest)
            result = flash.run_board_session(
                session,
                manifest,
                host_ip="192.168.120.1",
                board_ip="192.168.120.230",
                mmc_device=1,
                expected_mmc_name="SK64G",
                expected_capacity_gib=59.5,
                capacity_tolerance_gib=0.1,
                execute=True,
                tftp_timeout=5.0,
                write_timeout=5.0,
                fs_partition=0,
                verify_entries=["/glibc/cagent_testcode.sh"],
            )
            self.assertEqual(result["mode"], "execute")
            self.assertEqual(
                len(result["written"]), len(flash.manifest_chunks(manifest))
            )
            self.assertEqual(
                len([cmd for cmd in session.commands if cmd.startswith("gzwrite ")]),
                len(flash.manifest_chunks(manifest)),
            )

    def test_preflight_downloads_smallest_chunk_but_never_writes(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            _, manifest = self.prepared_image(Path(temp_dir))
            session = FakeUBoot(manifest)
            result = flash.run_board_session(
                session,
                manifest,
                host_ip="192.168.120.1",
                board_ip="192.168.120.230",
                mmc_device=1,
                expected_mmc_name="SK64G",
                expected_capacity_gib=59.5,
                capacity_tolerance_gib=0.1,
                execute=False,
                tftp_timeout=5.0,
                write_timeout=5.0,
                fs_partition=0,
                verify_entries=[],
            )
            self.assertEqual(result["mode"], "preflight")
            self.assertFalse(
                any(cmd.startswith("gzwrite ") for cmd in session.commands)
            )

    def test_unexpected_card_identity_blocks_before_any_tftp(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            _, manifest = self.prepared_image(Path(temp_dir))
            session = FakeUBoot(manifest, mmc_name="OTHER")
            with self.assertRaisesRegex(RuntimeError, "unexpected MMC name"):
                flash.run_board_session(
                    session,
                    manifest,
                    host_ip="192.168.120.1",
                    board_ip="192.168.120.230",
                    mmc_device=1,
                    expected_mmc_name="SK64G",
                    expected_capacity_gib=59.5,
                    capacity_tolerance_gib=0.1,
                    execute=True,
                    tftp_timeout=5.0,
                    write_timeout=5.0,
                    fs_partition=0,
                    verify_entries=[],
                )
            self.assertFalse(
                any(cmd.startswith("tftpboot ") for cmd in session.commands)
            )

    def test_bad_board_crc_stops_the_flash(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            _, manifest = self.prepared_image(Path(temp_dir))
            session = FakeUBoot(manifest, bad_crc=True)
            with self.assertRaisesRegex(RuntimeError, "gzwrite verification failed"):
                flash.run_board_session(
                    session,
                    manifest,
                    host_ip="192.168.120.1",
                    board_ip="192.168.120.230",
                    mmc_device=1,
                    expected_mmc_name="SK64G",
                    expected_capacity_gib=59.5,
                    capacity_tolerance_gib=0.1,
                    execute=True,
                    tftp_timeout=5.0,
                    write_timeout=5.0,
                    fs_partition=0,
                    verify_entries=[],
                )

    def test_manifest_rejects_non_contiguous_offsets(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            _, manifest = self.prepared_image(Path(temp_dir))
            chunks = flash.manifest_chunks(manifest)
            chunks[1]["offset"] = int(chunks[1]["offset"]) + 512
            chunks[1]["offset_hex"] = f"0x{int(chunks[1]['offset']):x}"
            with self.assertRaisesRegex(ValueError, "contiguous"):
                flash.validate_manifest(manifest)


if __name__ == "__main__":
    unittest.main()
