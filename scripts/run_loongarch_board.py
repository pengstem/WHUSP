#!/usr/bin/env python3
"""Control a Loongson 2K1000LA board over its 115200-baud UART.

The runner can capture or wait for U-Boot, probe the board, or TFTP a
program-header ELF into a low-memory staging area and start it with ``bootelf
-p``.  No command persists the temporary network configuration.
"""

from __future__ import annotations

import argparse
import fcntl
import hashlib
import ipaddress
import json
import os
import re
import secrets
import select
import shlex
import sys
import termios
import time
from datetime import datetime
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_RESULTS_ROOT = REPO_ROOT / "tools" / "loongarch_board_runs"
DEFAULT_TFTP_ROOT = Path("/tmp/whusp-starfive-tftp")
UBOOT_PROMPT = b"=> "
LINUX_PROMPT = re.compile(rb"\[[^\r\n]+@[^\r\n]+ [^\r\n]*\]# ")
SAFE_FILE = re.compile(r"^[A-Za-z0-9._-]+$")
LOONGSON_SERIAL_PATH_ID = "pci-0000:06:00.4-usb-0:2.3:1.0"
LOONGSON_SERIAL_PATH_NAMES = (
    f"{LOONGSON_SERIAL_PATH_ID}-port0",
    "pci-0000:06:00.4-usbv2-0:2.3:1.0-port0",
)
LOONGSON_SERIAL_PCI = "0000:06:00.4"
LOONGSON_SERIAL_USB_PORTS = "2.3"
LOONGSON_SERIAL_INTERFACE = "1.0"
KERNEL_STAGING_ADDRESS = 0x9000000002000000
KERNEL_STAGING_RESERVED_SIZE = 4 * 1024 * 1024
FDT_DESTINATION_ADDRESS = 0x900000000A000000
FDT_COPY_SIZE = 0x10000
UBOOT_BANNERS = (b"U-Boot ", b"Hit any key to stop autoboot")
WATCHDOG_CONTROL_ADDRESS = 0x800000001FE27030
WATCHDOG_FEED_ADDRESS = WATCHDOG_CONTROL_ADDRESS + 4
WATCHDOG_TIMER_ADDRESS = WATCHDOG_CONTROL_ADDRESS + 8
WATCHDOG_ENABLE = 1 << 1
WATCHDOG_CLOCK_HZ = 125_000_000
WATCHDOG_MAX_SECONDS = 0xFFFFFFFF // WATCHDOG_CLOCK_HZ
BOOT_WATCHDOG_SECONDS = 30
LINUX_SERIAL_WRITE_CHUNK_BYTES = 16
LINUX_SERIAL_WRITE_GAP_SECONDS = 0.005


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--serial",
        default="auto",
        help=(
            "UART device, or 'auto' to resolve the Loongson USB port through "
            "/dev/serial/by-path with a sysfs topology fallback"
        ),
    )
    parser.add_argument("--baud", type=int, default=115200, choices=(115200,))
    parser.add_argument(
        "--action",
        choices=(
            "capture-uboot",
            "wait-uboot",
            "uboot-probe",
            "boot-kernel",
            "linux-command",
            "watchdog-test",
        ),
        default="capture-uboot",
    )
    parser.add_argument("--timeout", type=float, default=180.0)
    parser.add_argument("--results-root", type=Path, default=DEFAULT_RESULTS_ROOT)
    parser.add_argument("--host-ip", default="192.168.120.1")
    parser.add_argument("--board-ip", default="192.168.120.230")
    parser.add_argument("--tftp-root", type=Path, default=DEFAULT_TFTP_ROOT)
    parser.add_argument("--kernel-name", default="whusp-2k1000.elf")
    parser.add_argument(
        "--expect-marker",
        action="append",
        default=[],
        help="UART marker required after bootelf; repeat to require multiple markers",
    )
    parser.add_argument("--boot-timeout", type=float, default=900.0)
    parser.add_argument(
        "--reacquire-uboot",
        action=argparse.BooleanOptionalAction,
        default=False,
        help="after all markers, catch a guest reboot and repeatedly send U-Boot's keyed 'c'",
    )
    parser.add_argument("--reboot-timeout", type=float, default=600.0)
    parser.add_argument(
        "--watchdog-timeout",
        type=int,
        default=5,
        help=f"watchdog-test timeout in seconds (1..{WATCHDOG_MAX_SECONDS})",
    )
    parser.add_argument(
        "--arm-watchdog",
        action="store_true",
        help=(
            f"arm a {BOOT_WATCHDOG_SECONDS}s U-Boot recovery watchdog before "
            "bootelf; use only for bounded smoke runs that are expected to reboot"
        ),
    )
    parser.add_argument(
        "--network-probe",
        action="store_true",
        help="temporarily set the board IPv4 address and ping the TFTP host",
    )
    parser.add_argument(
        "--command",
        action="append",
        default=[],
        help=(
            "additional U-Boot probe command, or a vendor-Linux shell command "
            "when --action=linux-command"
        ),
    )
    parser.add_argument(
        "--no-default-probe",
        action="store_true",
        help="run only commands supplied with --command",
    )
    args = parser.parse_args()
    for value in (args.host_ip, args.board_ip):
        ipaddress.ip_address(value)
    if not SAFE_FILE.fullmatch(args.kernel_name):
        parser.error("--kernel-name must be a plain filename")
    if min(args.timeout, args.boot_timeout, args.reboot_timeout) <= 0:
        parser.error("timeouts must be positive")
    if args.action == "boot-kernel" and not args.expect_marker:
        parser.error("boot-kernel requires at least one --expect-marker")
    if args.action == "linux-command":
        if not args.command:
            parser.error("linux-command requires at least one --command")
        if any("\r" in command or "\n" in command for command in args.command):
            parser.error("linux-command values must not contain newlines")
    if not 1 <= args.watchdog_timeout <= WATCHDOG_MAX_SECONDS:
        parser.error(f"--watchdog-timeout must be in 1..{WATCHDOG_MAX_SECONDS}")
    if args.arm_watchdog and args.action != "boot-kernel":
        parser.error("--arm-watchdog is only valid with --action=boot-kernel")
    if args.arm_watchdog and not args.reacquire_uboot:
        parser.error("--arm-watchdog requires --reacquire-uboot")
    return args


def _sysfs_usb_topology_matches(device_path: Path) -> bool:
    """Match the physical USB port without depending on ttyUSB numbering."""

    parts = device_path.parts
    if LOONGSON_SERIAL_PCI not in parts:
        return False
    device = re.compile(rf"^[0-9]+-{re.escape(LOONGSON_SERIAL_USB_PORTS)}$")
    interface = re.compile(
        rf"^[0-9]+-{re.escape(LOONGSON_SERIAL_USB_PORTS)}:"
        rf"{re.escape(LOONGSON_SERIAL_INTERFACE)}$"
    )
    return any(device.fullmatch(part) for part in parts) and any(
        interface.fullmatch(part) for part in parts
    )


def resolve_serial_device(
    requested: str,
    *,
    dev_root: Path = Path("/dev"),
    sys_tty_root: Path = Path("/sys/class/tty"),
) -> Path:
    """Resolve the board UART by stable USB topology, never by ttyUSB index."""

    if requested != "auto":
        explicit = Path(requested)
        if not explicit.exists():
            raise FileNotFoundError(f"serial device does not exist: {explicit}")
        return explicit

    by_path = dev_root / "serial" / "by-path"
    for name in LOONGSON_SERIAL_PATH_NAMES:
        candidate = by_path / name
        if candidate.exists():
            return candidate

    candidates: list[Path] = []
    for tty_dir in sorted(sys_tty_root.glob("ttyUSB*")):
        sys_device = tty_dir / "device"
        try:
            topology = sys_device.resolve(strict=True)
        except FileNotFoundError:
            continue
        candidate = dev_root / tty_dir.name
        if _sysfs_usb_topology_matches(topology) and candidate.exists():
            candidates.append(candidate)
    if len(candidates) == 1:
        return candidates[0]
    if len(candidates) > 1:
        joined = ", ".join(str(path) for path in candidates)
        raise RuntimeError(f"multiple Loongson UART candidates matched: {joined}")
    raise FileNotFoundError(
        "Loongson UART was not found at USB path " + LOONGSON_SERIAL_PATH_ID
    )


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def validate_tftp_layout(artifact_size: int) -> None:
    if artifact_size <= 0:
        raise ValueError("kernel ELF must not be empty")
    if artifact_size > KERNEL_STAGING_RESERVED_SIZE:
        raise ValueError(
            "kernel ELF exceeds the reserved 4 MiB staging envelope: "
            f"bytes={artifact_size} limit={KERNEL_STAGING_RESERVED_SIZE}"
        )
    staging_end = KERNEL_STAGING_ADDRESS + KERNEL_STAGING_RESERVED_SIZE
    if staging_end > FDT_DESTINATION_ADDRESS:
        raise ValueError(
            "kernel ELF staging range reaches the live FDT destination: "
            f"[0x{KERNEL_STAGING_ADDRESS:x}, 0x{staging_end:x})"
        )


def linux_command_line(command: str, token: str) -> str:
    return (
        f"sh -c {shlex.quote(command)}; __whusp_rc=$?; "
        f"printf '\\n{token}=%s\\n' \"$__whusp_rc\""
    )


def write_paced_linux_line(fd: int, command: bytes) -> None:
    """Send one shell line without a long continuous UART receive burst."""

    framed = memoryview(b"\x15" + command + b"\r")
    offset = 0
    while offset < len(framed):
        chunk_end = min(offset + LINUX_SERIAL_WRITE_CHUNK_BYTES, len(framed))
        while offset < chunk_end:
            written = os.write(fd, framed[offset:chunk_end])
            if written <= 0 or written > chunk_end - offset:
                raise OSError("serial write made no progress")
            offset += written
        termios.tcdrain(fd)
        if offset < len(framed):
            time.sleep(LINUX_SERIAL_WRITE_GAP_SECONDS)


def configure_serial(fd: int) -> None:
    attrs = termios.tcgetattr(fd)
    attrs[0] = 0
    attrs[1] = 0
    attrs[2] = termios.CS8 | termios.CREAD | termios.CLOCAL
    attrs[3] = 0
    attrs[4] = termios.B115200
    attrs[5] = termios.B115200
    attrs[6][termios.VMIN] = 0
    attrs[6][termios.VTIME] = 1
    termios.tcsetattr(fd, termios.TCSANOW, attrs)


class SerialSession:
    def __init__(self, fd: int, log_path: Path):
        self.fd = fd
        self.log = log_path.open("wb")
        self.all_bytes = bytearray()

    def close(self) -> None:
        self.log.close()

    def record(self, data: bytes) -> None:
        self.log.write(data)
        self.log.flush()
        self.all_bytes.extend(data)
        sys.stdout.buffer.write(data)
        sys.stdout.buffer.flush()

    def read_available(self, timeout: float) -> bytes:
        ready, _, _ = select.select([self.fd], [], [], timeout)
        if not ready:
            return b""
        try:
            data = os.read(self.fd, 4096)
        except BlockingIOError:
            return b""
        if data:
            self.record(data)
        return data

    def drain_until_quiet(self, quiet: float = 0.15, timeout: float = 1.0) -> bytes:
        start = len(self.all_bytes)
        deadline = time.monotonic() + timeout
        quiet_deadline = min(deadline, time.monotonic() + quiet)
        while time.monotonic() < deadline:
            wait = min(deadline, quiet_deadline) - time.monotonic()
            if wait <= 0:
                break
            if self.read_available(wait):
                quiet_deadline = min(deadline, time.monotonic() + quiet)
        return bytes(self.all_bytes[start:])

    def read_until(self, marker: bytes, timeout: float) -> bytes:
        start = len(self.all_bytes)
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.read_available(min(0.25, deadline - time.monotonic()))
            current = bytes(self.all_bytes[start:])
            if marker in current:
                return current
        raise TimeoutError(f"serial marker not received: {marker!r}")

    def read_until_pattern(self, pattern: re.Pattern[bytes], timeout: float) -> bytes:
        start = len(self.all_bytes)
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.read_available(min(0.25, deadline - time.monotonic()))
            current = bytes(self.all_bytes[start:])
            if pattern.search(current):
                return current
        raise TimeoutError(f"serial pattern not received: {pattern.pattern!r}")

    def current_console(self) -> str:
        self.drain_until_quiet()
        start = len(self.all_bytes)
        # Ctrl-C elicits a prompt from both consoles.  An empty carriage return
        # repeats the preceding command on this U-Boot and is therefore unsafe.
        os.write(self.fd, b"\x03")
        deadline = time.monotonic() + 3.0
        while time.monotonic() < deadline:
            self.read_available(min(0.25, deadline - time.monotonic()))
            current = bytes(self.all_bytes[start:])
            if UBOOT_PROMPT in current:
                return "uboot"
            if LINUX_PROMPT.search(current):
                return "linux"
        return "unknown"

    def capture_uboot(self, timeout: float) -> None:
        console = self.current_console()
        if console == "uboot":
            self.clear_uboot_input()
            return
        if console != "linux":
            raise RuntimeError(
                "neither a vendor Linux shell nor U-Boot prompt was detected"
            )

        os.write(self.fd, b"reboot\r")
        start = len(self.all_bytes)
        deadline = time.monotonic() + timeout
        bootloader_seen = False
        fallback_sent = False
        linux_prompt_count = 0
        next_stop_key = float("inf")
        while time.monotonic() < deadline:
            now = time.monotonic()
            if bootloader_seen and now >= next_stop_key:
                # This board uses CONFIG_AUTOBOOT_KEYED and the character `c`.
                # Send it only after a firmware banner proves Linux has left;
                # otherwise a failed `reboot` command would flood the shell.
                os.write(self.fd, b"c")
                next_stop_key = now + 0.05
            self.read_available(min(0.05, deadline - time.monotonic()))
            current = bytes(self.all_bytes[start:])
            if not bootloader_seen and any(
                banner in current for banner in UBOOT_BANNERS
            ):
                bootloader_seen = True
                next_stop_key = 0.0
            prompts = (
                list(LINUX_PROMPT.finditer(current)) if not bootloader_seen else []
            )
            if len(prompts) > linux_prompt_count:
                linux_prompt_count = len(prompts)
                if fallback_sent:
                    raise RuntimeError(
                        "vendor Linux rejected both reboot and the synced SysRq fallback"
                    )
                # A damaged userspace may no longer provide the vendor reboot
                # utility.  Shell redirection still works in the live console,
                # so ask Linux to sync and reboot through SysRq without relying
                # on a second userspace binary.  This fallback is attempted only
                # after the ordinary command returned to Linux.
                os.write(
                    self.fd,
                    b"\x15echo s > /proc/sysrq-trigger; echo b > /proc/sysrq-trigger\r",
                )
                fallback_sent = True
            if UBOOT_PROMPT in current:
                self.clear_uboot_input()
                return
        raise TimeoutError("U-Boot prompt was not captured after the Linux reboot")

    def wait_for_reset_to_uboot(self, timeout: float) -> None:
        """Wait passively for a reset, then send keyed stop characters."""

        self.drain_until_quiet()
        start = len(self.all_bytes)
        # Ctrl-C is safe on the silent kernel and elicits a fresh prompt if the
        # board is already parked.  The firmware-specific `c` stop key is sent
        # only after a U-Boot banner proves that a reset is in progress.
        os.write(self.fd, b"\x03")
        deadline = time.monotonic() + timeout
        bootloader_seen = False
        next_stop_key = float("inf")
        while time.monotonic() < deadline:
            now = time.monotonic()
            if bootloader_seen and now >= next_stop_key:
                os.write(self.fd, b"c")
                next_stop_key = now + 0.05
            self.read_available(min(0.05, deadline - now))
            current = bytes(self.all_bytes[start:])
            if not bootloader_seen and any(
                banner in current for banner in UBOOT_BANNERS
            ):
                bootloader_seen = True
                next_stop_key = 0.0
            if UBOOT_PROMPT in current:
                self.clear_uboot_input()
                return
            if not bootloader_seen and LINUX_PROMPT.search(current):
                raise RuntimeError(
                    "vendor Linux is live; use capture-uboot to request its reboot"
                )
        raise TimeoutError("U-Boot prompt was not observed before the wait timeout")

    def clear_uboot_input(self) -> None:
        """Discard stop-key characters queued after the prompt was recognized."""

        os.write(self.fd, b"\x03")
        self.read_until(UBOOT_PROMPT, 3.0)
        self.drain_until_quiet(quiet=0.05, timeout=0.25)

    def synchronize_uboot(self) -> None:
        self.drain_until_quiet()
        # A carriage return on an empty U-Boot line repeats the preceding
        # command on this firmware. Ctrl-C alone produces a fresh prompt.
        os.write(self.fd, b"\x03")
        self.read_until(UBOOT_PROMPT, 3.0)

    def uboot_command(self, command: str, timeout: float = 30.0) -> bytes:
        encoded = command.encode("ascii")
        self.synchronize_uboot()
        os.write(self.fd, b"\x15" + encoded + b"\r")
        output = self.read_until(UBOOT_PROMPT, timeout)
        return output

    def start_uboot_command(self, command: str) -> None:
        encoded = command.encode("ascii")
        self.synchronize_uboot()
        os.write(self.fd, b"\x15" + encoded + b"\r")

    def ensure_linux(self, timeout: float) -> None:
        console = self.current_console()
        if console == "linux":
            return
        if console != "uboot":
            raise RuntimeError(
                "neither a vendor Linux shell nor U-Boot prompt was detected"
            )
        self.start_uboot_command("run bootcmd")
        self.read_until_pattern(LINUX_PROMPT, timeout)

    def linux_command(self, command: str, timeout: float) -> tuple[bytes, int]:
        """Run one vendor-shell command and parse its unique exit-status record."""

        token = "WHUSP_LINUX_RC_" + secrets.token_hex(12).upper()
        wrapped = linux_command_line(command, token)
        encoded = wrapped.encode("utf-8")
        self.drain_until_quiet()
        os.write(self.fd, b"\x03\r")
        self.read_until_pattern(LINUX_PROMPT, 3.0)
        start = len(self.all_bytes)
        write_paced_linux_line(self.fd, encoded)
        status_pattern = re.compile(re.escape(token.encode("ascii")) + rb"=([0-9]+)")
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.read_available(min(0.25, deadline - time.monotonic()))
            output = bytes(self.all_bytes[start:])
            status = status_pattern.search(output)
            prompts = list(LINUX_PROMPT.finditer(output))
            if status and prompts and prompts[-1].start() > status.end():
                return output, int(status.group(1))
        raise TimeoutError(f"vendor Linux command did not finish: {command!r}")

    def monitor_kernel(
        self,
        markers: list[bytes],
        boot_timeout: float,
        reacquire_uboot: bool,
        reboot_timeout: float,
    ) -> bool:
        """Capture markers and, when requested, never miss a subsequent reboot."""

        start = len(self.all_bytes)
        marker_deadline = time.monotonic() + boot_timeout
        reboot_deadline: float | None = None
        bootloader_seen = False
        next_stop_key = float("inf")
        while True:
            now = time.monotonic()
            deadline = marker_deadline if reboot_deadline is None else reboot_deadline
            if now >= deadline:
                break
            if bootloader_seen and now >= next_stop_key:
                os.write(self.fd, b"c")
                next_stop_key = now + 0.05
            self.read_available(min(0.05, deadline - now))
            current = bytes(self.all_bytes[start:])
            if not bootloader_seen and any(
                banner in current for banner in UBOOT_BANNERS
            ):
                bootloader_seen = True
                next_stop_key = 0.0

            markers_complete = all(marker in current for marker in markers)
            if markers_complete and reboot_deadline is None:
                if not reacquire_uboot:
                    return False
                reboot_deadline = time.monotonic() + reboot_timeout
            if UBOOT_PROMPT in current:
                missing = [marker for marker in markers if marker not in current]
                self.clear_uboot_input()
                if missing:
                    raise RuntimeError(
                        "kernel rebooted to captured U-Boot before required markers: "
                        + ", ".join(repr(marker) for marker in missing)
                    )
                if reacquire_uboot and bootloader_seen:
                    return True

        current = bytes(self.all_bytes[start:])
        missing = [marker for marker in markers if marker not in current]
        if missing:
            raise TimeoutError(
                "kernel UART markers not received: "
                + ", ".join(repr(marker) for marker in missing)
            )
        raise TimeoutError("kernel markers passed but U-Boot was not reacquired")


def probe_commands(network_probe: bool, host_ip: str, board_ip: str) -> list[str]:
    commands = [
        "version",
        "printenv ipaddr serverip loadaddr fdt_addr fdt_size bootcmd bootargs",
        "bdinfo",
        "help reset",
        "help bootelf",
        "help bootm",
        "help scsi",
        "scsi reset",
        "scsi info",
        "scsi part 0",
    ]
    if network_probe:
        commands.extend(
            [
                f"setenv serverip {host_ip}",
                f"setenv ipaddr {board_ip}",
                f"ping {host_ip}",
            ]
        )
    return commands


def boot_preflight_commands(host_ip: str, board_ip: str, kernel_name: str) -> list[str]:
    """Commands are intentionally session-only: there is no ``saveenv``."""

    return [
        f"setenv serverip {host_ip}",
        f"setenv ipaddr {board_ip}",
        f"ping {host_ip}",
        f"setenv fdt_addr 0x{FDT_DESTINATION_ADDRESS:x}",
        "fdt addr ${fdtcontroladdr}",
        "fdt move ${fdtcontroladdr} ${fdt_addr} 10000",
        f"tftpboot 0x{KERNEL_STAGING_ADDRESS:x} {kernel_name}",
    ]


def require_output(output: bytes, marker: bytes, context: str) -> None:
    if marker not in output:
        raise RuntimeError(f"{context} failed; missing {marker!r}")


def transferred_size(output: bytes) -> int:
    match = re.search(rb"Bytes transferred = ([0-9]+)", output)
    if not match:
        raise RuntimeError("TFTP transfer did not report a byte count")
    return int(match.group(1))


def parse_watchdog_control(output: bytes) -> int:
    pattern = re.compile(
        rf"{WATCHDOG_CONTROL_ADDRESS:016x}:\s+([0-9a-fA-F]{{8}})".encode()
    )
    match = pattern.search(output)
    if not match:
        raise RuntimeError("U-Boot watchdog control value is not parseable")
    return int(match.group(1), 16)


def watchdog_program_commands(control: int, seconds: int) -> list[str]:
    if not 1 <= seconds <= WATCHDOG_MAX_SECONDS:
        raise ValueError(f"watchdog timeout must be in 1..{WATCHDOG_MAX_SECONDS}")
    stopped_control = control & ~WATCHDOG_ENABLE
    enabled_control = stopped_control | WATCHDOG_ENABLE
    ticks = seconds * WATCHDOG_CLOCK_HZ
    # This is the order used by the official ls2x_wdt driver: ping, stop,
    # program, enable, ping.  All non-watchdog control bits are preserved.
    return [
        f"mw.l 0x{WATCHDOG_FEED_ADDRESS:x} 0x1 1",
        f"mw.l 0x{WATCHDOG_CONTROL_ADDRESS:x} 0x{stopped_control:x} 1",
        f"mw.l 0x{WATCHDOG_TIMER_ADDRESS:x} 0x{ticks:x} 1",
        f"mw.l 0x{WATCHDOG_CONTROL_ADDRESS:x} 0x{enabled_control:x} 1",
        f"mw.l 0x{WATCHDOG_FEED_ADDRESS:x} 0x1 1",
    ]


def arm_watchdog(session: SerialSession, seconds: int) -> dict[str, object]:
    read_command = f"md.l 0x{WATCHDOG_CONTROL_ADDRESS:x} 1"
    control = parse_watchdog_control(session.uboot_command(read_command))
    commands = watchdog_program_commands(control, seconds)
    for command in commands:
        session.uboot_command(command)
    return {
        "seconds": seconds,
        "clock_hz": WATCHDOG_CLOCK_HZ,
        "ticks": seconds * WATCHDOG_CLOCK_HZ,
        "control_before": f"0x{control:08x}",
        "control_stopped": f"0x{control & ~WATCHDOG_ENABLE:08x}",
        "control_enabled": f"0x{control | WATCHDOG_ENABLE:08x}",
        "commands": [read_command, *commands],
    }


def main() -> int:
    args = parse_args()
    run_id = datetime.now().astimezone().strftime("%Y%m%d-%H%M%S")
    run_dir = args.results_root / run_id
    run_dir.mkdir(parents=True)
    serial_path = resolve_serial_device(args.serial)
    kernel_path = args.tftp_root / args.kernel_name
    if args.action == "boot-kernel":
        if not kernel_path.is_file():
            raise SystemExit(
                f"kernel ELF does not exist in the TFTP root: {kernel_path}"
            )
        validate_tftp_layout(kernel_path.stat().st_size)

    manifest: dict[str, object] = {
        "action": args.action,
        "network_probe": args.network_probe,
        "commands": args.command,
        "default_probe": not args.no_default_probe,
        "run_id": run_id,
        "serial_requested": args.serial,
        "serial": str(serial_path),
        "timeout": args.timeout,
        "host_ip": args.host_ip,
        "board_ip": args.board_ip,
        "expected_markers": args.expect_marker,
        "reacquire_uboot": args.reacquire_uboot,
        "arm_watchdog": args.arm_watchdog,
        "watchdog_timeout": args.watchdog_timeout,
    }
    if args.action == "boot-kernel":
        manifest.update(
            {
                "kernel": str(kernel_path),
                "kernel_bytes": kernel_path.stat().st_size,
                "kernel_sha256": sha256(kernel_path),
                "kernel_staging_address": f"0x{KERNEL_STAGING_ADDRESS:x}",
                "kernel_staging_reserved_bytes": KERNEL_STAGING_RESERVED_SIZE,
                "fdt_destination_address": f"0x{FDT_DESTINATION_ADDRESS:x}",
                "fdt_copy_size": FDT_COPY_SIZE,
            }
        )
    manifest_path = run_dir / "manifest.json"
    manifest_path.write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )

    fd = os.open(serial_path, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    configure_serial(fd)
    session = SerialSession(fd, run_dir / "serial.log")
    try:
        if args.action == "wait-uboot":
            session.wait_for_reset_to_uboot(args.timeout)
            message = "Loongson reset was captured at U-Boot"
        elif args.action == "linux-command":
            session.ensure_linux(args.timeout)
            results = []
            for command in args.command:
                _output, status = session.linux_command(command, args.timeout)
                results.append({"command": command, "status": status})
                if status != 0:
                    raise RuntimeError(
                        f"vendor Linux command failed with status {status}: {command}"
                    )
            manifest["linux_commands"] = results
            message = "Loongson vendor-Linux commands passed"
        elif args.action == "watchdog-test":
            session.capture_uboot(args.timeout)
            manifest["watchdog"] = arm_watchdog(session, args.watchdog_timeout)
            reacquired = session.monitor_kernel([], 1.0, True, args.reboot_timeout)
            manifest["uboot_reacquired"] = reacquired
            message = "Loongson watchdog reset returned to U-Boot"
        elif args.action == "boot-kernel":
            session.capture_uboot(args.timeout)
            for command in boot_preflight_commands(
                args.host_ip, args.board_ip, args.kernel_name
            ):
                output = session.uboot_command(
                    command, 180.0 if command.startswith("tftpboot ") else 30.0
                )
                if command.startswith("ping "):
                    require_output(output, b"is alive", "U-Boot network preflight")
                elif command.startswith("fdt ") and b"FDT_ERR" in output:
                    raise RuntimeError(f"U-Boot FDT preparation failed: {command}")
                elif command.startswith("tftpboot "):
                    actual_size = transferred_size(output)
                    expected_size = kernel_path.stat().st_size
                    if actual_size != expected_size:
                        raise RuntimeError(
                            "TFTP byte count differs from the local ELF: "
                            f"actual={actual_size} expected={expected_size}"
                        )
                    manifest["tftp_bytes"] = actual_size
            session.uboot_command("setenv autostart yes")
            if args.arm_watchdog:
                manifest["watchdog"] = arm_watchdog(session, BOOT_WATCHDOG_SECONDS)
            session.start_uboot_command(f"bootelf -p 0x{KERNEL_STAGING_ADDRESS:x}")
            markers = [marker.encode("utf-8") for marker in args.expect_marker]
            uboot_reacquired = session.monitor_kernel(
                markers,
                args.boot_timeout,
                args.reacquire_uboot,
                args.reboot_timeout,
            )
            manifest["observed_markers"] = args.expect_marker
            manifest["uboot_reacquired"] = uboot_reacquired
            message = "Loongson kernel UART markers passed"
        else:
            if args.action == "capture-uboot":
                session.capture_uboot(args.timeout)
            elif session.current_console() != "uboot":
                raise RuntimeError("board is not parked at the U-Boot prompt")

            commands = (
                []
                if args.no_default_probe
                else probe_commands(args.network_probe, args.host_ip, args.board_ip)
            )
            commands.extend(args.command)
            for command in commands:
                session.uboot_command(command)
            message = "Loongson board parked at U-Boot"

        manifest["result"] = "pass"
        manifest["serial_bytes"] = len(session.all_bytes)
        manifest_path.write_text(
            json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
        print(f"\n{message}; evidence: {run_dir}")
        return 0
    except Exception as error:
        manifest["result"] = "fail"
        manifest["error"] = str(error)
        manifest["serial_bytes"] = len(session.all_bytes)
        manifest_path.write_text(
            json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
        raise
    finally:
        session.close()
        os.close(fd)


if __name__ == "__main__":
    raise SystemExit(main())
