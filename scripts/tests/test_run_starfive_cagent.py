from __future__ import annotations

import os
import sys
import tempfile
import threading
import tty
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT / "scripts"))

import run_starfive_cagent as starfive_runner


class StarFiveRunnerTests(unittest.TestCase):
    def test_maintenance_mmc_identity_is_exact(self) -> None:
        output = b"Name: SK64G \r\nCapacity: 59.5 GiB\r\n"
        starfive_runner.require_mmc_identity(output, "SK64G", 59.5)
        with self.assertRaisesRegex(RuntimeError, "identity mismatch"):
            starfive_runner.require_mmc_identity(output, "SK64GX", 59.5)
        with self.assertRaisesRegex(RuntimeError, "identity mismatch"):
            starfive_runner.require_mmc_identity(output, "SK64G", 58.0)

    def test_sd_maintenance_final_marker_is_strict(self) -> None:
        good = (
            b"STARFIVE_SD_MAINTENANCE_FINAL action=list status=0 count=3\r\n"
        )
        bad = b"STARFIVE_SD_MAINTENANCE_FINAL action=list status=1 count=3\r\n"
        self.assertEqual(
            starfive_runner.SD_MAINTENANCE_FINAL_PATTERN.findall(good),
            [(b"list", b"0", b"3")],
        )
        self.assertEqual(
            starfive_runner.SD_MAINTENANCE_FINAL_PATTERN.findall(bad),
            [(b"list", b"1", b"3")],
        )

    def test_default_results_root_is_persistent_and_ignored(self) -> None:
        self.assertEqual(
            starfive_runner.DEFAULT_RESULTS_ROOT,
            REPO_ROOT / "tools" / "starfive_runs",
        )
        self.assertNotEqual(starfive_runner.DEFAULT_RESULTS_ROOT.parent, Path("/tmp"))
        self.assertTrue((REPO_ROOT / ".gitignore").read_text().splitlines().count("tools/"))

    def test_uboot_command_retries_until_its_own_echo_is_observed(self) -> None:
        peer_fd, client_fd = os.openpty()
        tty.setraw(client_fd)
        command = b"ping 192.168.120.1"
        os.write(peer_fd, b"stale output\r\nStarFive # ")

        def emulate_uboot() -> None:
            buffer = bytearray()
            command_attempts = 0
            while command_attempts < 2:
                buffer.extend(os.read(peer_fd, 4096))
                if b"\x03\r" in buffer:
                    os.write(peer_fd, b"<INTERRUPT>\r\nStarFive # ")
                    buffer.clear()
                    continue
                if b"\x15" + command + b"\r" not in buffer:
                    continue
                command_attempts += 1
                if command_attempts == 1:
                    os.write(peer_fd, b"StarFive # ")
                else:
                    os.write(
                        peer_fd,
                        command
                        + b"\r\nhost 192.168.120.1 is alive\r\nStarFive # "
                    )
                buffer.clear()

        worker = threading.Thread(target=emulate_uboot)
        worker.start()
        try:
            with tempfile.TemporaryDirectory() as temp_dir:
                log = starfive_runner.SerialLog(
                    client_fd, Path(temp_dir) / "serial.log"
                )
                try:
                    output = log.command(command.decode(), timeout=1.0)
                finally:
                    log.close()
            self.assertIn(command, output)
            self.assertIn(b"is alive", output)
        finally:
            os.close(client_fd)
            os.close(peer_fd)
            worker.join(timeout=1.0)
        self.assertFalse(worker.is_alive())


if __name__ == "__main__":
    unittest.main()
