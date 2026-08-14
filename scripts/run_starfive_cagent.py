#!/usr/bin/env python3
"""Load a WHUSP FIT through VisionFive 2 U-Boot and validate guest output."""

from __future__ import annotations

import argparse
import fcntl
import hashlib
import ipaddress
import json
import os
import re
import select
import subprocess
import sys
import termios
import time
from datetime import datetime
from pathlib import Path

import analyze_starfive_rust_smoke

REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_RESULTS_ROOT = REPO_ROOT / "tools" / "starfive_runs"
PROMPT = b"StarFive # "
SAFE_FILE = re.compile(r"^[A-Za-z0-9._-]+$")
PASS_PATTERN = re.compile(
    rb"^testcase cagent ([^ \r\n]+) pass(?: [0-9]+)?\r*$", re.MULTILINE
)
FINAL_STATUS_PATTERN = re.compile(
    rb"^FINAL: all enabled tests finished \(status=[0-9]+\)\r?$", re.MULTILINE
)
BUILDSTORM_PATTERN = re.compile(
    rb"^BUILDSTORM_COMPILE mode=multi ok=(true|false)(?: rc=([0-9]+))? "
    rb"elapsed_s=([0-9]+(?:\.[0-9]+)?) cores=([0-9]+) bytes=([0-9]+) "
    rb"arch=([^ \r\n]+)\r?$",
    re.MULTILINE,
)
RUST_SMOKE_FINAL_PATTERN = re.compile(
    rb"^FINAL: starfive rust smoke finished \(status=[0-9]+\)\r?$", re.MULTILINE
)
RUST_SMOKE_PASS_PATTERN = re.compile(
    rb"^G0_RUST_HELLO_PASS run_id="
    rb"(starfive-hello-cold|starfive-hello-warm|starfive-multicrate) "
    rb"[^\r\n]* workload=(hello|multicrate)\r?$",
    re.MULTILINE,
)
RUST_SMOKE_RESULT_PATTERN = re.compile(
    rb"^G0_RUST_HELLO_RESULT run_id="
    rb"(starfive-hello-cold|starfive-hello-warm|starfive-multicrate) "
    rb"[^\r\n]* workload=(hello|multicrate) "
    rb"[^\r\n]* ok=1\r?$",
    re.MULTILINE,
)
SD_MAINTENANCE_FINAL_PATTERN = re.compile(
    rb"^STARFIVE_SD_MAINTENANCE_FINAL action=(list|delete) "
    rb"status=([0-9]+) count=([0-9]+)\r?$",
    re.MULTILINE,
)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--serial", default="/dev/ttyUSB0")
    parser.add_argument("--baud", type=int, default=115200, choices=[115200])
    parser.add_argument("--host-ip", default="192.168.120.1")
    parser.add_argument("--board-ip", default="192.168.120.230")
    parser.add_argument("--tftp-root", type=Path, default=Path("/tmp/whusp-starfive-tftp"))
    parser.add_argument("--fit-name", default="whusp-cagent.itb")
    parser.add_argument(
        "--mode",
        choices=["cagent", "buildstorm", "rust-smoke", "sd-maintenance", "shell"],
        default="cagent",
    )
    parser.add_argument(
        "--expect-mmc-name",
        help="exact U-Boot 'Name:' value; required for sd-maintenance",
    )
    parser.add_argument(
        "--expect-capacity-gib",
        type=float,
        help="exact displayed U-Boot GiB capacity; required for sd-maintenance",
    )
    parser.add_argument(
        "--prompt-timeout",
        type=float,
        default=300.0,
        help="seconds to wait for a reset and interrupt U-Boot autoboot",
    )
    parser.add_argument(
        "--preflight-only",
        action="store_true",
        help="verify serial, network, TFTP, and FIT parsing without requiring an SD card",
    )
    parser.add_argument(
        "--prompt-only",
        action="store_true",
        help="interrupt autoboot and leave the board parked at the U-Boot prompt",
    )
    parser.add_argument(
        "--reacquire-uboot",
        action=argparse.BooleanOptionalAction,
        default=False,
        help=(
            "after a non-interactive guest result, catch the JH7110 watchdog "
            "reboot and park at U-Boot"
        ),
    )
    parser.add_argument("--reboot-timeout", type=float, default=600.0)
    parser.add_argument("--boot-timeout", type=float, default=900.0)
    parser.add_argument("--results-root", type=Path, default=DEFAULT_RESULTS_ROOT)
    args = parser.parse_args()
    ipaddress.ip_address(args.host_ip)
    ipaddress.ip_address(args.board_ip)
    if not SAFE_FILE.fullmatch(args.fit_name):
        parser.error("--fit-name must be a plain filename")
    if args.prompt_timeout <= 0 or args.boot_timeout <= 0 or args.reboot_timeout <= 0:
        parser.error("--prompt-timeout, --boot-timeout, and --reboot-timeout must be positive")
    if args.prompt_only and args.preflight_only:
        parser.error("--prompt-only and --preflight-only are mutually exclusive")
    if (
        args.mode == "sd-maintenance"
        and not args.preflight_only
        and not args.prompt_only
    ):
        if not args.expect_mmc_name or args.expect_capacity_gib is None:
            parser.error(
                "sd-maintenance requires --expect-mmc-name and --expect-capacity-gib"
            )
        if not SAFE_FILE.fullmatch(args.expect_mmc_name):
            parser.error("--expect-mmc-name contains unsafe characters")
        if args.expect_capacity_gib <= 0:
            parser.error("--expect-capacity-gib must be positive")
    return args


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
    termios.tcflush(fd, termios.TCIOFLUSH)


class SerialLog:
    def __init__(self, fd: int, path: Path):
        self.fd = fd
        self.file = path.open("wb")
        self.all_bytes = bytearray()

    def close(self) -> None:
        self.file.close()

    def write(self, data: bytes) -> None:
        self.file.write(data)
        self.file.flush()
        self.all_bytes.extend(data)
        sys.stdout.buffer.write(data)
        sys.stdout.buffer.flush()

    def read_until(self, marker: bytes, timeout: float) -> bytes:
        start_index = len(self.all_bytes)
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            ready, _, _ = select.select([self.fd], [], [], min(0.25, deadline - time.monotonic()))
            if not ready:
                continue
            try:
                data = os.read(self.fd, 4096)
            except BlockingIOError:
                continue
            if not data:
                continue
            self.write(data)
            current = bytes(self.all_bytes[start_index:])
            if marker in current:
                return current
        raise TimeoutError(f"serial marker not received: {marker!r}")

    def read_until_pattern(self, pattern: re.Pattern[bytes], timeout: float) -> bytes:
        start_index = len(self.all_bytes)
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            ready, _, _ = select.select(
                [self.fd], [], [], min(0.25, deadline - time.monotonic())
            )
            if not ready:
                continue
            try:
                data = os.read(self.fd, 4096)
            except BlockingIOError:
                continue
            if not data:
                continue
            self.write(data)
            current = bytes(self.all_bytes[start_index:])
            if pattern.search(current):
                return current
        raise TimeoutError(f"serial pattern not received: {pattern.pattern!r}")

    def drain_until_quiet(self, quiet: float = 0.1, timeout: float = 0.5) -> bytes:
        """Consume and record stale input until the UART stays quiet."""
        start_index = len(self.all_bytes)
        deadline = time.monotonic() + timeout
        quiet_deadline = min(deadline, time.monotonic() + quiet)
        while time.monotonic() < deadline:
            wait = min(deadline, quiet_deadline) - time.monotonic()
            if wait <= 0:
                break
            ready, _, _ = select.select([self.fd], [], [], wait)
            if not ready:
                break
            try:
                data = os.read(self.fd, 4096)
            except BlockingIOError:
                continue
            if not data:
                continue
            self.write(data)
            quiet_deadline = min(deadline, time.monotonic() + quiet)
        return bytes(self.all_bytes[start_index:])

    def synchronize_prompt(self, timeout: float = 3.0) -> bytes:
        """Clear an incomplete U-Boot line and obtain a fresh prompt."""
        self.drain_until_quiet()
        os.write(self.fd, b"\x03\r")
        output = self.read_until(PROMPT, timeout)
        self.drain_until_quiet(quiet=0.05, timeout=0.25)
        return output

    def interrupt_until_prompt(self, timeout: float) -> bytes:
        start_index = len(self.all_bytes)
        deadline = time.monotonic() + timeout
        next_interrupt = 0.0
        while time.monotonic() < deadline:
            now = time.monotonic()
            if now >= next_interrupt:
                # Ctrl-C is safe at an idle U-Boot prompt and reliably stops
                # its autoboot countdown after a physical board reset.
                os.write(self.fd, b"\x03")
                next_interrupt = now + 0.5
            ready, _, _ = select.select(
                [self.fd], [], [], min(0.25, deadline - time.monotonic())
            )
            if not ready:
                continue
            try:
                data = os.read(self.fd, 4096)
            except BlockingIOError:
                continue
            if not data:
                continue
            self.write(data)
            current = bytes(self.all_bytes[start_index:])
            if PROMPT in current:
                return current
        raise TimeoutError("U-Boot prompt not received; reset the board and check serial")

    def reacquire_prompt_after_reboot(self, timeout: float) -> bytes:
        start_index = len(self.all_bytes)
        deadline = time.monotonic() + timeout
        bootloader_seen = False
        next_interrupt = float("inf")
        while time.monotonic() < deadline:
            now = time.monotonic()
            if bootloader_seen and now >= next_interrupt:
                os.write(self.fd, b"\x03")
                next_interrupt = now + 0.25
            ready, _, _ = select.select(
                [self.fd], [], [], min(0.25, deadline - time.monotonic())
            )
            if not ready:
                continue
            try:
                data = os.read(self.fd, 4096)
            except BlockingIOError:
                continue
            if not data:
                continue
            self.write(data)
            current = bytes(self.all_bytes[start_index:])
            if b"U-Boot SPL" in current or b"\nU-Boot 2021.10" in current:
                bootloader_seen = True
                next_interrupt = 0.0
            if bootloader_seen and PROMPT in current:
                return current
        raise TimeoutError("guest finished but U-Boot prompt was not reacquired")

    def command(self, command: str, timeout: float = 10.0) -> bytes:
        encoded = command.encode("ascii")
        for _attempt in range(3):
            self.synchronize_prompt()
            os.write(self.fd, b"\x15" + encoded + b"\r")
            output = self.read_until(PROMPT, timeout)
            if encoded in output:
                return output
        raise RuntimeError(f"U-Boot command echo did not synchronize: {command!r}")


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def git_head(repo_root: Path) -> str:
    return subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=repo_root,
        check=True,
        text=True,
        stdout=subprocess.PIPE,
    ).stdout.strip()


def require_text(output: bytes, pattern: bytes, context: str) -> None:
    if pattern not in output:
        raise RuntimeError(f"{context} failed; missing {pattern!r}")


def require_mmc_identity(
    output: bytes, expected_name: str, expected_capacity_gib: float
) -> None:
    text = output.decode("ascii", errors="replace").replace("\r", "")
    name_match = re.search(r"^Name:\s*(\S+)\s*$", text, re.MULTILINE)
    capacity_match = re.search(
        r"^Capacity:\s*([0-9]+(?:\.[0-9]+)?)\s+GiB\s*$", text, re.MULTILINE
    )
    if not name_match or not capacity_match:
        raise RuntimeError("maintenance MMC identity is not parseable")
    actual_name = name_match.group(1)
    actual_capacity_gib = float(capacity_match.group(1))
    if actual_name != expected_name or actual_capacity_gib != expected_capacity_gib:
        raise RuntimeError(
            "maintenance MMC identity mismatch: "
            f"actual={actual_name}/{actual_capacity_gib:g}GiB "
            f"expected={expected_name}/{expected_capacity_gib:g}GiB"
        )


def main() -> int:
    args = parse_args()
    fit_path = args.tftp_root / args.fit_name
    if not args.prompt_only and not fit_path.is_file():
        raise SystemExit(f"FIT image does not exist: {fit_path}")

    run_id = datetime.now().astimezone().strftime("%Y%m%d-%H%M%S")
    run_dir = args.results_root / run_id
    run_dir.mkdir(parents=True)
    manifest = {
        "run_id": run_id,
        "git_head": git_head(REPO_ROOT),
        "fit": None if args.prompt_only else str(fit_path),
        "fit_sha256": None if args.prompt_only else sha256(fit_path),
        "serial": args.serial,
        "host_ip": args.host_ip,
        "board_ip": args.board_ip,
        "mode": args.mode,
        "expect_mmc_name": args.expect_mmc_name,
        "expect_capacity_gib": args.expect_capacity_gib,
        "preflight_only": args.preflight_only,
        "prompt_only": args.prompt_only,
        "reacquire_uboot": args.reacquire_uboot,
        "prompt_timeout": args.prompt_timeout,
        "reboot_timeout": args.reboot_timeout,
    }
    (run_dir / "manifest.json").write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )

    fd = os.open(args.serial, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    configure_serial(fd)
    log = SerialLog(fd, run_dir / "serial.log")
    reacquire_requested = False
    try:
        log.interrupt_until_prompt(args.prompt_timeout)
        if args.prompt_only:
            manifest["result"] = "pass"
            manifest["uboot_reacquired"] = True
            manifest["serial_bytes"] = len(log.all_bytes)
            (run_dir / "manifest.json").write_text(
                json.dumps(manifest, indent=2, sort_keys=True) + "\n",
                encoding="utf-8",
            )
            print(f"\nStarFive parked at U-Boot; evidence: {run_dir}")
            return 0
        log.command(f"setenv ipaddr {args.board_ip}")
        log.command(f"setenv serverip {args.host_ip}")
        ping = log.command(f"ping {args.host_ip}", 15.0)
        require_text(ping, b"is alive", "U-Boot network preflight")
        transfer = log.command(
            f"tftpboot ${{kernel_addr_r}} {args.fit_name}", timeout=180.0
        )
        require_text(transfer, b"Bytes transferred", "FIT TFTP transfer")
        image_info = log.command("iminfo ${kernel_addr_r}", 30.0)
        require_text(image_info, b"FIT image found", "FIT image preflight")
        if args.preflight_only:
            log.command("fdt addr ${fdtcontroladdr}")
            log.command("fdt print /cpus", 30.0)
            manifest["result"] = "pass"
            manifest["serial_bytes"] = len(log.all_bytes)
            (run_dir / "manifest.json").write_text(
                json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8"
            )
            print(f"\nStarFive FIT preflight passed; evidence: {run_dir}")
            return 0

        log.command("mmc dev 1", 10.0)
        mmc = log.command("mmc rescan", 30.0)
        if b"no card present" in mmc.lower() or b"Card did not respond" in mmc:
            raise RuntimeError("U-Boot did not detect an SD card in mmc1")
        mmc_info = log.command("mmc info", 10.0)
        if args.mode == "sd-maintenance":
            require_mmc_identity(
                mmc_info, args.expect_mmc_name, args.expect_capacity_gib
            )
        if args.mode == "rust-smoke":
            root = log.command("ext4ls mmc 1:0 /root/.cargo/bin", 30.0)
            require_text(root, b"cargo", "Rust smoke cargo preflight")
            require_text(root, b"rustc", "Rust smoke rustc preflight")
        elif args.mode == "sd-maintenance":
            root = log.command("ext4ls mmc 1:0 /work", 30.0)
            if b"**" in root or b"Failed" in root:
                raise RuntimeError("maintenance target rootfs is not readable")
        else:
            root = log.command("ext4ls mmc 1:0 /glibc", 30.0)
            required_test_script = (
                b"buildstorm_testcode.sh"
                if args.mode == "buildstorm"
                else b"cagent_testcode.sh"
            )
            require_text(root, required_test_script, f"{args.mode} rootfs preflight")
        log.command("fdt addr ${fdtcontroladdr}")
        log.command("fdt move ${fdtcontroladdr} ${fdt_addr_r} 20000")

        os.write(
            fd,
            b"bootm ${kernel_addr_r}:kernel ${kernel_addr_r}:ramdisk ${fdt_addr_r}\r",
        )
        if args.mode == "shell":
            log.read_until(b"FINAL: entering interactive BusyBox shell", args.boot_timeout)
        elif args.mode == "rust-smoke":
            log.read_until_pattern(RUST_SMOKE_FINAL_PATTERN, args.boot_timeout)
        elif args.mode == "sd-maintenance":
            log.read_until_pattern(SD_MAINTENANCE_FINAL_PATTERN, args.boot_timeout)
        else:
            log.read_until_pattern(FINAL_STATUS_PATTERN, args.boot_timeout)
        reacquire_requested = args.mode != "shell" and args.reacquire_uboot
        if args.mode == "cagent":
            full_log = bytes(log.all_bytes)
            passes = set(PASS_PATTERN.findall(full_log))
            if len(passes) != 10:
                raise RuntimeError(f"expected 10 unique CAgent passes, observed {len(passes)}")
            require_text(
                full_log,
                b"FINAL: finished cagent-glibc (status=0)",
                "CAGENT group",
            )
            require_text(
                full_log,
                b"FINAL: all enabled tests finished (status=0)",
                "CAgent final status",
            )
        elif args.mode == "buildstorm":
            full_log = bytes(log.all_bytes)
            records = BUILDSTORM_PATTERN.findall(full_log)
            if len(records) != 1:
                raise RuntimeError(
                    f"expected one BuildStorm result record, observed {len(records)}"
                )
            ok, _rc, _elapsed, cores, byte_count, arch = records[0]
            if ok != b"true" or arch != b"riscv64" or int(byte_count) < 500_000:
                raise RuntimeError(
                    "BuildStorm result failed: "
                    f"ok={ok.decode()} cores={cores.decode()} "
                    f"bytes={byte_count.decode()} arch={arch.decode()}"
                )
            for required, context in (
                (b"BUILDSTORM_TOOLCHAIN ok", "BuildStorm toolchain gate"),
                (b"BUILDSTORM_MINIBUILD ok", "BuildStorm minibuild gate"),
                (
                    b"STARFIVE_BUILDSTORM_SAFE_CLONE ok",
                    "StarFive disposable workspace",
                ),
                (
                    b"STARFIVE_BUILDSTORM_SAFE_VALIDATION ok",
                    "StarFive BuildStorm validation",
                ),
                (
                    b"STARFIVE_BUILDSTORM_SAFE_RESULT status=0",
                    "StarFive BuildStorm wrapper result",
                ),
                (
                    b"FINAL: finished buildstorm-glibc (status=0)",
                    "BuildStorm group",
                ),
                (
                    b"FINAL: all enabled tests finished (status=0)",
                    "BuildStorm final status",
                ),
            ):
                require_text(full_log, required, context)
        elif args.mode == "sd-maintenance":
            full_log = bytes(log.all_bytes)
            records = SD_MAINTENANCE_FINAL_PATTERN.findall(full_log)
            if len(records) != 1:
                raise RuntimeError(
                    "expected one SD maintenance result, "
                    f"observed {len(records)}"
                )
            action, status, count = records[0]
            if action not in {b"list", b"delete"} or status != b"0":
                raise RuntimeError(
                    "SD maintenance failed: "
                    f"action={action.decode()} status={status.decode()} "
                    f"count={count.decode()}"
                )
        elif args.mode == "rust-smoke":
            full_log = bytes(log.all_bytes)
            passes = set(RUST_SMOKE_PASS_PATTERN.findall(full_log))
            results = set(RUST_SMOKE_RESULT_PATTERN.findall(full_log))
            expected = {
                (b"starfive-hello-cold", b"hello"),
                (b"starfive-hello-warm", b"hello"),
                (b"starfive-multicrate", b"multicrate"),
            }
            if passes != expected or results != expected:
                raise RuntimeError(
                    "Rust smoke markers incomplete: "
                    f"passes={sorted((a.decode(), b.decode()) for a, b in passes)} "
                    f"results={sorted((a.decode(), b.decode()) for a, b in results)}"
                )
            for required, context in (
                (
                    b"FINAL: finished rust-hello-cold (status=0)",
                    "Rust cold hello group",
                ),
                (
                    b"FINAL: finished rust-hello-warm (status=0)",
                    "Rust warm hello group",
                ),
                (
                    b"FINAL: finished rust-multicrate (status=0)",
                    "Rust multicrate group",
                ),
                (
                    b"FINAL: starfive rust smoke finished (status=0)",
                    "Rust smoke final status",
                ),
            ):
                require_text(full_log, required, context)
            if b"G0_RUST_HELLO_PERF_BEGIN" in full_log:
                manifest["rust_smoke_analysis"] = (
                    analyze_starfive_rust_smoke.analyze_text(
                        full_log.decode("utf-8", errors="replace")
                    )
                )
        manifest["result"] = "pass"
        manifest["serial_bytes"] = len(log.all_bytes)
        (run_dir / "manifest.json").write_text(
            json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
        print(f"\nStarFive {args.mode} passed; evidence: {run_dir}")
        return 0
    except Exception as error:  # noqa: BLE001 - persist every runner failure
        manifest["result"] = "failed"
        manifest["error"] = str(error)
        manifest["serial_bytes"] = len(log.all_bytes)
        (run_dir / "manifest.json").write_text(
            json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
        print(f"\nStarFive run failed: {error}", file=sys.stderr)
        print(f"Evidence: {run_dir}", file=sys.stderr)
        return 1
    finally:
        if reacquire_requested:
            try:
                log.reacquire_prompt_after_reboot(args.reboot_timeout)
                manifest["uboot_reacquired"] = True
                print("\nStarFive rebooted and is parked at the U-Boot prompt")
            except Exception as error:  # noqa: BLE001 - preserve the test result
                manifest["uboot_reacquired"] = False
                manifest["uboot_reacquire_error"] = str(error)
                print(f"\nStarFive U-Boot reacquire warning: {error}", file=sys.stderr)
            manifest["serial_bytes"] = len(log.all_bytes)
            (run_dir / "manifest.json").write_text(
                json.dumps(manifest, indent=2, sort_keys=True) + "\n",
                encoding="utf-8",
            )
        log.close()
        os.close(fd)


if __name__ == "__main__":
    raise SystemExit(main())
