from __future__ import annotations

import contextlib
import importlib.util
import io
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

SCRIPT = Path(__file__).resolve().parents[1] / "run_loongarch_board.py"
SPEC = importlib.util.spec_from_file_location("run_loongarch_board", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
RUNNER = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = RUNNER
SPEC.loader.exec_module(RUNNER)


class LoongsonBoardRunnerTests(unittest.TestCase):
    def test_runner_scope_excludes_workload_and_rootfs_installation(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")

        self.assertNotIn("install-rootfs", source)
        self.assertNotIn("update-entry", source)
        self.assertNotIn("BuildStorm", source)
        self.assertNotIn("ROOTFS_", source)

    def test_default_probe_is_non_persistent(self) -> None:
        commands = RUNNER.probe_commands(False, "192.168.120.1", "192.168.120.230")

        self.assertIn("bdinfo", commands)
        self.assertIn("scsi reset", commands)
        self.assertIn("scsi part 0", commands)
        self.assertNotIn("saveenv", commands)
        self.assertFalse(any(command.startswith("setenv ") for command in commands))

    def test_network_probe_is_temporary(self) -> None:
        commands = RUNNER.probe_commands(True, "10.0.0.1", "10.0.0.2")

        self.assertIn("setenv serverip 10.0.0.1", commands)
        self.assertIn("setenv ipaddr 10.0.0.2", commands)
        self.assertIn("ping 10.0.0.1", commands)
        self.assertNotIn("saveenv", commands)

    def test_armed_watchdog_requires_automatic_uboot_reacquire(self) -> None:
        with (
            mock.patch.object(
                sys,
                "argv",
                [
                    "run_loongarch_board.py",
                    "--action",
                    "boot-kernel",
                    "--expect-marker",
                    "READY",
                    "--arm-watchdog",
                ],
            ),
            contextlib.redirect_stderr(io.StringIO()),
            self.assertRaises(SystemExit),
        ):
            RUNNER.parse_args()

    def test_boot_preflight_uses_safe_layout_and_temporary_network(self) -> None:
        commands = RUNNER.boot_preflight_commands(
            "192.168.120.1", "192.168.120.230", "whusp-2k1000.elf"
        )

        self.assertEqual(
            commands,
            [
                "setenv serverip 192.168.120.1",
                "setenv ipaddr 192.168.120.230",
                "ping 192.168.120.1",
                "setenv fdt_addr 0x900000000a000000",
                "fdt addr ${fdtcontroladdr}",
                "fdt move ${fdtcontroladdr} ${fdt_addr} 10000",
                "tftpboot 0x9000000002000000 whusp-2k1000.elf",
            ],
        )
        self.assertFalse(any(command == "saveenv" for command in commands))

    def test_auto_serial_prefers_stable_by_path(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            dev_root = root / "dev"
            by_path = dev_root / "serial" / "by-path"
            by_path.mkdir(parents=True)
            target = dev_root / "ttyUSB7"
            target.touch()
            stable = by_path / RUNNER.LOONGSON_SERIAL_PATH_NAMES[0]
            stable.symlink_to(Path("../../ttyUSB7"))

            resolved = RUNNER.resolve_serial_device(
                "auto", dev_root=dev_root, sys_tty_root=root / "missing-sysfs"
            )

            self.assertEqual(resolved, stable)

    def test_auto_serial_uses_usb_topology_sysfs_fallback(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            dev_root = root / "dev"
            dev_root.mkdir()
            (dev_root / "ttyUSB7").touch()
            topology = (
                root
                / "devices/pci0000:00/0000:00:08.1/0000:06:00.4"
                / "usb3/3-2/3-2.3/3-2.3:1.0/ttyUSB7"
            )
            topology.mkdir(parents=True)
            tty = root / "sys/class/tty/ttyUSB7"
            tty.mkdir(parents=True)
            (tty / "device").symlink_to(topology, target_is_directory=True)

            resolved = RUNNER.resolve_serial_device(
                "auto", dev_root=dev_root, sys_tty_root=root / "sys/class/tty"
            )

            self.assertEqual(resolved, dev_root / "ttyUSB7")

    def test_linux_command_is_shell_quoted_and_has_status_sentinel(self) -> None:
        line = RUNNER.linux_command_line("printf '%s' 'hello world'", "TOKEN_123")

        self.assertIn("sh -c", line)
        self.assertIn("TOKEN_123=%s", line)
        self.assertIn("__whusp_rc=$?", line)

    def test_linux_line_write_is_paced_and_preserves_every_byte(self) -> None:
        command = b"x" * (RUNNER.LINUX_SERIAL_WRITE_CHUNK_BYTES * 2 + 5)
        expected = b"\x15" + command + b"\r"
        written_bytes = bytearray()

        def partial_write(fd: int, data: bytes) -> int:
            self.assertEqual(fd, 37)
            count = min(7, len(data))
            written_bytes.extend(bytes(data[:count]))
            return count

        with (
            mock.patch.object(RUNNER.os, "write", side_effect=partial_write) as write,
            mock.patch.object(RUNNER.termios, "tcdrain") as drain,
            mock.patch.object(RUNNER.time, "sleep") as sleep,
        ):
            RUNNER.write_paced_linux_line(37, command)

        chunks = (
            len(expected) + RUNNER.LINUX_SERIAL_WRITE_CHUNK_BYTES - 1
        ) // RUNNER.LINUX_SERIAL_WRITE_CHUNK_BYTES
        self.assertEqual(written_bytes, expected)
        self.assertTrue(
            all(
                len(call.args[1]) <= RUNNER.LINUX_SERIAL_WRITE_CHUNK_BYTES
                for call in write.call_args_list
            )
        )
        self.assertEqual(drain.call_count, chunks)
        self.assertEqual(sleep.call_count, chunks - 1)

    def test_linux_command_uses_paced_line_write(self) -> None:
        token_hex = "ab" * 12
        token = "WHUSP_LINUX_RC_" + token_hex.upper()

        class FakeSession:
            def __init__(self) -> None:
                self.fd = 37
                self.all_bytes = bytearray()

            def drain_until_quiet(self) -> bytes:
                return b""

            def read_until_pattern(self, _pattern: object, _timeout: float) -> bytes:
                return b""

            def read_available(self, _timeout: float) -> bytes:
                data = f"{token}=0\n[root@LS-GD ~]# ".encode()
                self.all_bytes.extend(data)
                return data

        session = FakeSession()
        with (
            mock.patch.object(RUNNER.secrets, "token_hex", return_value=token_hex),
            mock.patch.object(RUNNER.os, "write", return_value=2) as write,
            mock.patch.object(RUNNER, "write_paced_linux_line") as paced_write,
        ):
            _output, status = RUNNER.SerialSession.linux_command(
                session, "printf ok", 1.0
            )

        self.assertEqual(status, 0)
        write.assert_called_once_with(37, b"\x03\r")
        paced_write.assert_called_once_with(
            37,
            RUNNER.linux_command_line("printf ok", token).encode("utf-8"),
        )

    def test_tftp_staging_must_not_exceed_reserved_envelope(self) -> None:
        RUNNER.validate_tftp_layout(RUNNER.KERNEL_STAGING_RESERVED_SIZE)
        with self.assertRaisesRegex(ValueError, "reserved 4 MiB staging"):
            RUNNER.validate_tftp_layout(RUNNER.KERNEL_STAGING_RESERVED_SIZE + 1)

    def test_tftp_transfer_size_parser_requires_reported_count(self) -> None:
        self.assertEqual(
            RUNNER.transferred_size(b"Bytes transferred = 3480000 (3519c0 hex)"),
            3_480_000,
        )
        with self.assertRaisesRegex(RuntimeError, "did not report"):
            RUNNER.transferred_size(b"TFTP error")

    def test_monitor_reacquires_uboot_without_a_gap_after_marker(self) -> None:
        read_fd, write_fd = os.pipe()

        class FakeSession:
            def __init__(self) -> None:
                self.fd = write_fd
                self.all_bytes = bytearray()
                self.chunks = [b"KERNEL_PASS\r\n", b"U-Boot 2022.04\r\n", b"=> "]
                self.cleared = False

            def read_available(self, _timeout: float) -> bytes:
                data = self.chunks.pop(0)
                self.all_bytes.extend(data)
                return data

            def clear_uboot_input(self) -> None:
                self.cleared = True

        try:
            session = FakeSession()
            reacquired = RUNNER.SerialSession.monitor_kernel(
                session, [b"KERNEL_PASS"], 1.0, True, 1.0
            )
            os.set_blocking(read_fd, False)
            stop_keys = os.read(read_fd, 16)
        finally:
            os.close(read_fd)
            os.close(write_fd)

        self.assertTrue(reacquired)
        self.assertTrue(session.cleared)
        self.assertIn(b"c", stop_keys)

    def test_wait_uboot_only_sends_stop_key_after_banner(self) -> None:
        read_fd, write_fd = os.pipe()

        class FakeSession:
            def __init__(self) -> None:
                self.fd = write_fd
                self.all_bytes = bytearray()
                self.chunks = [b"U-Boot 2022.04\r\n", b"=> "]
                self.cleared = False

            def drain_until_quiet(self) -> bytes:
                return b""

            def read_available(self, _timeout: float) -> bytes:
                data = self.chunks.pop(0)
                self.all_bytes.extend(data)
                return data

            def clear_uboot_input(self) -> None:
                self.cleared = True

        try:
            session = FakeSession()
            RUNNER.SerialSession.wait_for_reset_to_uboot(session, 1.0)
            os.set_blocking(read_fd, False)
            stop_keys = os.read(read_fd, 16)
        finally:
            os.close(read_fd)
            os.close(write_fd)

        self.assertTrue(session.cleared)
        self.assertEqual(stop_keys, b"\x03c")

    def test_capture_uboot_uses_sysrq_after_reboot_command_failed(self) -> None:
        read_fd, write_fd = os.pipe()

        class FakeSession:
            def __init__(self) -> None:
                self.fd = write_fd
                self.all_bytes = bytearray()
                self.chunks = [
                    b"reboot\r\n-bash: reboot: command not found\r\n[root@LS-GD ~]# ",
                    b"U-Boot 2022.04\r\n",
                    b"=> ",
                ]
                self.cleared = False

            def current_console(self) -> str:
                return "linux"

            def read_available(self, _timeout: float) -> bytes:
                data = self.chunks.pop(0)
                self.all_bytes.extend(data)
                return data

            def clear_uboot_input(self) -> None:
                self.cleared = True

        try:
            session = FakeSession()
            RUNNER.SerialSession.capture_uboot(session, 1.0)
            os.set_blocking(read_fd, False)
            writes = os.read(read_fd, 256)
        finally:
            os.close(read_fd)
            os.close(write_fd)

        self.assertTrue(session.cleared)
        self.assertEqual(
            writes,
            b"reboot\r"
            b"\x15echo s > /proc/sysrq-trigger; "
            b"echo b > /proc/sysrq-trigger\r"
            b"c",
        )

    def test_watchdog_sequence_preserves_control_bits_and_uses_125mhz(self) -> None:
        commands = RUNNER.watchdog_program_commands(0xA7, 5)

        self.assertEqual(
            commands,
            [
                "mw.l 0x800000001fe27034 0x1 1",
                "mw.l 0x800000001fe27030 0xa5 1",
                "mw.l 0x800000001fe27038 0x2540be40 1",
                "mw.l 0x800000001fe27030 0xa7 1",
                "mw.l 0x800000001fe27034 0x1 1",
            ],
        )

    def test_watchdog_control_parser_uses_exact_mmio_address(self) -> None:
        output = b"800000001fe27030: 00000082 00000000\r\n=> "

        self.assertEqual(RUNNER.parse_watchdog_control(output), 0x82)

    def test_watchdog_timeout_is_bounded_by_32_bit_counter(self) -> None:
        with self.assertRaises(ValueError):
            RUNNER.watchdog_program_commands(0, RUNNER.WATCHDOG_MAX_SECONDS + 1)


if __name__ == "__main__":
    unittest.main()
