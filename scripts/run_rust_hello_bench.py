#!/usr/bin/env python3
"""Run one dual-architecture-contract clean Rust Hello World benchmark cell."""

from __future__ import annotations

import argparse
import json
import os
import re
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from decimal import Decimal, InvalidOperation
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parents[1]
RUNNER_SOURCE = Path(__file__).resolve()
OS_MAKEFILE = REPO_ROOT / "os" / "Makefile"
GUEST_TEMPLATE = REPO_ROOT / "scripts" / "perf-gates" / "g0-rust-hello.sh"
TIMER_SOURCE = REPO_ROOT / "scripts" / "perf-gates" / "rust_build_timer.c"
MAX_CPUS = 12
PROCESS_STOP_TIMEOUT_SECONDS = 2.0
TOKEN_RE = re.compile(r"[A-Za-z0-9._-]+")
MEM_RE = re.compile(r"[1-9][0-9]*[MG]")
IMAGE_SIZE_RE = re.compile(r"[1-9][0-9]*[MG]")
ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")
PANIC_RE = re.compile(r"panicked at|kernel panic|assertion failed", re.IGNORECASE)
SHUTDOWN_RE = re.compile(
    r"smp shutdown: leader=(?P<leader>[0-9]+) "
    r"requested=(?P<requested>0x[0-9a-f]+) "
    r"stopped=(?P<stopped>0x[0-9a-f]+) "
    r"missing=(?P<missing>0x[0-9a-f]+) failure=(?P<failure>true|false)"
)
OVERLAY_RE = re.compile(
    r"WHUSP_QEMU_OVERLAY state=(?P<state>created|cleaned) "
    r"path=(?P<path>/[^\r\n ]+)"
)
IDENTITY_PATTERN = (
    r"run_id=(?P<run_id>[A-Za-z0-9._-]+) "
    r"arch=(?P<arch>rv|la) "
    r"kind=(?P<kind>warmup|measured) "
    r"sample=(?P<sample>[0-9]+) "
    r"smp=(?P<smp>[0-9]+) "
    r"mem=(?P<mem>[A-Za-z0-9._-]+)"
)
START_RE = re.compile(r"G0_RUST_HELLO_START " + IDENTITY_PATTERN)
PASS_RE = re.compile(r"G0_RUST_HELLO_PASS " + IDENTITY_PATTERN)
FAIL_RE = re.compile(
    r"G0_RUST_HELLO_FAIL "
    + IDENTITY_PATTERN
    + r" stage=(?P<stage>[A-Za-z0-9._-]+)"
    r" reason=(?P<reason>[A-Za-z0-9._-]+) rc=(?P<rc>-?[0-9]+)"
)
RESULT_RE = re.compile(
    r"G0_RUST_HELLO_RESULT "
    + IDENTITY_PATTERN
    + r" uname=(?P<uname>riscv64|loongarch64)"
    r" nproc=(?P<nproc>[0-9]+)"
    r" cargo_version=(?P<cargo_version>[A-Za-z0-9._-]+)"
    r" rustc_version=(?P<rustc_version>[A-Za-z0-9._-]+)"
    r" tmp_mount=(?P<tmp_mount>[01])"
    r" tmp_writable=(?P<tmp_writable>[01])"
    r" elapsed_ns=(?P<elapsed_ns>[0-9]+)"
    r" timer_exited=(?P<timer_exited>[01])"
    r" timer_exit_code=(?P<timer_exit_code>-?[0-9]+)"
    r" timer_signaled=(?P<timer_signaled>[01])"
    r" timer_signal=(?P<timer_signal>[0-9]+)"
    r" timer_rc=(?P<timer_rc>[0-9]+)"
    r" uptime_before=(?P<uptime_before>[0-9]+(?:\.[0-9]+)?)"
    r" uptime_after=(?P<uptime_after>[0-9]+(?:\.[0-9]+)?)"
    r" lock_created=(?P<lock_created>[01])"
    r" artifact_bytes=(?P<artifact_bytes>[0-9]+)"
    r" output_bytes=(?P<output_bytes>[0-9]+)"
    r" output_ok=(?P<output_ok>[01]) ok=(?P<ok>[01])"
)


class BenchmarkError(RuntimeError):
    pass


@dataclass(frozen=True)
class Architecture:
    name: str
    make_arch: str
    uname: str


ARCHITECTURES = {
    "rv": Architecture(
        name="rv",
        make_arch="riscv64",
        uname="riscv64",
    ),
    "la": Architecture(
        name="la",
        make_arch="loongarch64",
        uname="loongarch64",
    ),
}


@dataclass(frozen=True)
class Trial:
    ordinal: int
    kind: str
    sample: int

    @property
    def directory_name(self) -> str:
        return f"{self.ordinal:02d}-{self.kind}-{self.sample:02d}"


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def write_json(path: Path, value: Any) -> None:
    temporary = path.with_name(path.name + ".tmp")
    with temporary.open("w", encoding="utf-8") as output:
        json.dump(value, output, ensure_ascii=False, indent=2, sort_keys=True)
        output.write("\n")
    temporary.replace(path)


def command_text(command: list[str]) -> str:
    return shlex.join(command) + "\n"


def run_capture(command: list[str], *, cwd: Path = REPO_ROOT) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        command,
        cwd=cwd,
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )


def require_command(command: str) -> str:
    resolved = shutil.which(command)
    if not resolved:
        raise BenchmarkError(f"required host command is unavailable: {command}")
    return resolved


def compiler_for(arch: str) -> Path:
    if arch == "rv":
        resolved = shutil.which("riscv64-linux-musl-gcc")
        if resolved:
            return Path(resolved).resolve()
    else:
        resolved = shutil.which("loongarch64-linux-musl-gcc")
        if resolved:
            return Path(resolved).resolve()
        bundled = (
            REPO_ROOT
            / "tools"
            / "loongarch64-linux-musl-cross"
            / "bin"
            / "loongarch64-linux-musl-gcc"
        )
        if bundled.is_file():
            return bundled.resolve()
    raise BenchmarkError(f"{arch} static musl compiler is unavailable")


def file_metadata(path: Path) -> dict[str, Any]:
    status = path.stat()
    return {
        "path": str(path),
        "size_bytes": status.st_size,
        "mtime_ns": status.st_mtime_ns,
        "mode": oct(status.st_mode & 0o7777),
    }


def selected_input_metadata(
    *, kernel: Path, disk: Path, compiler: Path
) -> dict[str, dict[str, Any]]:
    return {
        "kernel": file_metadata(kernel),
        "test_disk": file_metadata(disk),
        "runner": file_metadata(RUNNER_SOURCE),
        "os_makefile": file_metadata(OS_MAKEFILE),
        "guest_template": file_metadata(GUEST_TEMPLATE),
        "timer_source": file_metadata(TIMER_SOURCE),
        "timer_compiler": file_metadata(compiler),
    }


def host_load_snapshot() -> dict[str, Any]:
    fields = Path("/proc/loadavg").read_text(encoding="ascii").strip().split()
    return {
        "captured_at": utc_now(),
        "load_1m": fields[0],
        "load_5m": fields[1],
        "load_15m": fields[2],
        "running_over_total": fields[3],
    }


def expected_memory_end(arch: str, mem: str) -> int:
    scale = 1024**2 if mem.endswith("M") else 1024**3
    memory_bytes = int(mem[:-1]) * scale
    base = 0x8000_0000 if arch == "rv" else 0x9000_0000_7000_0000
    return base + memory_bytes


def validate_token(name: str, value: str) -> None:
    if not TOKEN_RE.fullmatch(value):
        raise BenchmarkError(f"{name} is not a safe marker/template token: {value!r}")


def render_guest(identity: dict[str, str]) -> str:
    for name, value in identity.items():
        validate_token(name, value)
    source = GUEST_TEMPLATE.read_text(encoding="utf-8")
    replacements = {
        "@RUN_ID@": identity["run_id"],
        "@ARCH@": identity["arch"],
        "@KIND@": identity["kind"],
        "@SAMPLE@": identity["sample"],
        "@SMP@": identity["smp"],
        "@MEM@": identity["mem"],
    }
    for placeholder, replacement in replacements.items():
        count = source.count(placeholder)
        if count != 1:
            raise BenchmarkError(
                f"guest template must contain {placeholder} exactly once; found {count}"
            )
        source = source.replace(placeholder, replacement)
    if re.search(r"@[A-Z][A-Z0-9_]*@", source):
        raise BenchmarkError("guest template contains an unresolved placeholder")
    return source


def normalized_lines(log: str) -> list[str]:
    return [ANSI_RE.sub("", line.rstrip("\r")) for line in log.splitlines()]


def marker_identity_errors(
    groups: dict[str, str], expected: dict[str, str], label: str
) -> list[str]:
    errors: list[str] = []
    for field in ("run_id", "arch", "kind", "sample", "smp", "mem"):
        if groups[field] != expected[field]:
            errors.append(
                f"{label} {field} mismatch: {groups[field]!r} != {expected[field]!r}"
            )
    return errors


def validate_guest_log(
    log: str,
    *,
    identity: dict[str, str],
    architecture: Architecture,
    smp: int,
    mem: str,
) -> tuple[dict[str, Any], list[str]]:
    errors: list[str] = []
    lines = normalized_lines(log)
    if "\ufffd" in log:
        errors.append("serial log contains a UTF-8 replacement character")
    if PANIC_RE.search(ANSI_RE.sub("", log)):
        errors.append("kernel panic/assertion text found")
    for contamination in ("BUILDSTORM_", "CAGENT_", "buildstorm-glibc"):
        if contamination in log:
            errors.append(f"unexpected workload contamination marker: {contamination}")

    starts: list[tuple[int, re.Match[str]]] = []
    results: list[tuple[int, re.Match[str]]] = []
    passes: list[tuple[int, re.Match[str]]] = []
    failures: list[tuple[int, re.Match[str]]] = []
    malformed: list[str] = []
    for index, line in enumerate(lines):
        if "G0_RUST_HELLO_" not in line:
            continue
        match = START_RE.fullmatch(line)
        if match:
            starts.append((index, match))
            continue
        match = RESULT_RE.fullmatch(line)
        if match:
            results.append((index, match))
            continue
        match = PASS_RE.fullmatch(line)
        if match:
            passes.append((index, match))
            continue
        match = FAIL_RE.fullmatch(line)
        if match:
            failures.append((index, match))
            continue
        malformed.append(line)

    if malformed:
        errors.append(f"malformed guest marker lines: {malformed!r}")
    if len(starts) != 1:
        errors.append(f"expected one START marker, found {len(starts)}")
    if len(results) != 1:
        errors.append(f"expected one RESULT marker, found {len(results)}")
    if len(passes) != 1:
        errors.append(f"expected one PASS marker, found {len(passes)}")
    if failures:
        descriptions = [
            f"{item.group('stage')}/{item.group('reason')}/rc={item.group('rc')}"
            for _, item in failures
        ]
        errors.append(f"guest emitted FAIL marker(s): {descriptions}")

    for label, matches in (("START", starts), ("RESULT", results), ("PASS", passes)):
        if len(matches) == 1:
            errors.extend(marker_identity_errors(matches[0][1].groupdict(), identity, label))
    if (
        len(starts) == len(results) == len(passes) == 1
        and not starts[0][0] < results[0][0] < passes[0][0]
    ):
        errors.append("START, RESULT and PASS markers are out of order")

    parsed: dict[str, Any] = {}
    if len(results) == 1:
        fields = results[0][1].groupdict()
        parsed.update(fields)
        exact_one_fields = (
            "tmp_mount",
            "tmp_writable",
            "timer_exited",
            "lock_created",
            "output_ok",
            "ok",
        )
        for field in exact_one_fields:
            if fields[field] != "1":
                errors.append(f"RESULT {field} must be 1")
        for field in ("timer_exit_code", "timer_signaled", "timer_signal", "timer_rc"):
            if fields[field] != "0":
                errors.append(f"RESULT {field} must be 0")
        if fields["uname"] != architecture.uname:
            errors.append(f"RESULT uname mismatch: {fields['uname']!r}")
        if int(fields["nproc"]) != smp:
            errors.append(f"RESULT nproc mismatch: {fields['nproc']}")
        if int(fields["artifact_bytes"]) <= 0:
            errors.append("RESULT artifact_bytes must be positive")
        if int(fields["output_bytes"]) != 14:
            errors.append("RESULT output_bytes must be 14")
        elapsed_ns = int(fields["elapsed_ns"])
        if elapsed_ns <= 0:
            errors.append("RESULT elapsed_ns must be positive")
        try:
            uptime_before = Decimal(fields["uptime_before"])
            uptime_after = Decimal(fields["uptime_after"])
            elapsed_seconds = Decimal(elapsed_ns) / Decimal(1_000_000_000)
            uptime_delta = uptime_after - uptime_before
            tolerance = max(Decimal("0.100"), elapsed_seconds * Decimal("0.10"))
            difference = abs(uptime_delta - elapsed_seconds)
            if uptime_delta < 0:
                errors.append("/proc/uptime moved backwards")
            if difference > tolerance:
                errors.append(
                    "timer/uptime disagreement: "
                    f"timer={elapsed_seconds}s uptime={uptime_delta}s "
                    f"difference={difference}s tolerance={tolerance}s"
                )
            parsed.update(
                {
                    "elapsed_seconds": str(elapsed_seconds),
                    "uptime_delta_seconds": str(uptime_delta),
                    "clock_difference_seconds": str(difference),
                    "clock_tolerance_seconds": str(tolerance),
                }
            )
        except (InvalidOperation, ValueError) as error:
            errors.append(f"invalid timer/uptime number: {error}")

    possible_mask = (1 << smp) - 1
    topology = (
        f"cpu topology: possible={smp} online=1 "
        f"possible_mask={possible_mask:#x} online_mask=0x1"
    )
    normalized_log = "\n".join(lines)
    if normalized_log.count(topology) != 1:
        errors.append(f"expected one truthful early topology line: {topology}")
    memory = f"memory_end={expected_memory_end(architecture.name, mem):#x}"
    if normalized_log.count(memory) != 1:
        errors.append(f"expected one memory-size diagnostic: {memory}")
    schedulers = f"smp schedulers: active_mask={possible_mask:#x} count={smp}"
    if normalized_log.count(schedulers) != 1:
        errors.append(f"expected one active-scheduler diagnostic: {schedulers}")
    invariant = "smp invariants: boot_entries=1 global_init_entries=1"
    if normalized_log.count(invariant) != 1:
        errors.append(f"expected one boot/global invariant line: {invariant}")

    shutdowns = [match for line in lines if (match := SHUTDOWN_RE.fullmatch(line))]
    if len(shutdowns) != 1:
        errors.append(f"expected one shutdown summary, found {len(shutdowns)}")
    else:
        shutdown = shutdowns[0].groupdict()
        leader = int(shutdown["leader"])
        requested = int(shutdown["requested"], 16)
        stopped = int(shutdown["stopped"], 16)
        missing = int(shutdown["missing"], 16)
        if not 0 <= leader < smp:
            errors.append(f"shutdown leader is outside topology: {leader}")
        else:
            expected_requested = possible_mask & ~(1 << leader)
            if requested != expected_requested:
                errors.append(
                    f"shutdown requested mask {requested:#x} != {expected_requested:#x}"
                )
        if stopped != requested:
            errors.append(f"shutdown stopped mask {stopped:#x} != requested {requested:#x}")
        if missing != 0:
            errors.append(f"shutdown missing mask is nonzero: {missing:#x}")
        if shutdown["failure"] != "false":
            errors.append("shutdown reported failure=true")
        parsed["shutdown"] = {
            "leader": leader,
            "requested": f"{requested:#x}",
            "stopped": f"{stopped:#x}",
            "missing": f"{missing:#x}",
            "failure": shutdown["failure"],
        }
    return parsed, errors


def validate_overlay_log(log: str, overlay_root: Path) -> tuple[str | None, list[str]]:
    errors: list[str] = []
    lines = normalized_lines(log)
    markers: list[tuple[int, re.Match[str]]] = []
    malformed: list[str] = []
    for index, line in enumerate(lines):
        if "WHUSP_QEMU_OVERLAY" not in line:
            continue
        match = OVERLAY_RE.fullmatch(line)
        if match:
            markers.append((index, match))
        else:
            malformed.append(line)
    if malformed:
        errors.append(f"malformed overlay marker lines: {malformed!r}")
    created = [item for item in markers if item[1].group("state") == "created"]
    cleaned = [item for item in markers if item[1].group("state") == "cleaned"]
    if len(created) != 1:
        errors.append(f"expected one overlay created marker, found {len(created)}")
    if len(cleaned) != 1:
        errors.append(f"expected one overlay cleaned marker, found {len(cleaned)}")
    if len(created) != 1 or len(cleaned) != 1:
        return None, errors
    created_path = Path(created[0][1].group("path"))
    cleaned_path = Path(cleaned[0][1].group("path"))
    if created_path != cleaned_path:
        errors.append("overlay created/cleaned marker paths differ")
    if created[0][0] >= cleaned[0][0]:
        errors.append("overlay cleaned marker did not follow created marker")
    canonical_root = overlay_root.resolve()
    if created_path.parent != canonical_root:
        errors.append(
            f"overlay path parent {created_path.parent} != owned root {canonical_root}"
        )
    if not created_path.name.startswith("whusp-qemu."):
        errors.append(f"unexpected overlay directory name: {created_path.name}")
    if created_path.exists():
        errors.append(f"overlay directory remains after run: {created_path}")
    return str(created_path), errors


def process_group_exists(group: int) -> bool:
    try:
        os.killpg(group, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def stop_process_group(process: subprocess.Popen[bytes]) -> str:
    if process.poll() is not None and not process_group_exists(process.pid):
        return "already-exited"
    action = "sigterm"
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    if process.poll() is None:
        try:
            process.wait(timeout=PROCESS_STOP_TIMEOUT_SECONDS)
        except subprocess.TimeoutExpired:
            pass
    deadline = time.monotonic() + PROCESS_STOP_TIMEOUT_SECONDS
    while process_group_exists(process.pid) and time.monotonic() < deadline:
        time.sleep(0.05)
    if process_group_exists(process.pid):
        action = "sigkill"
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        if process.poll() is None:
            try:
                process.wait(timeout=PROCESS_STOP_TIMEOUT_SECONDS)
            except subprocess.TimeoutExpired:
                action = "sigkill-reap-timeout"
        deadline = time.monotonic() + PROCESS_STOP_TIMEOUT_SECONDS
        while process_group_exists(process.pid) and time.monotonic() < deadline:
            time.sleep(0.05)
    return action


def run_logged(command: list[str], log_path: Path, timeout: float) -> dict[str, Any]:
    started = time.monotonic()
    timed_out = False
    interrupted = False
    termination = "none"
    with log_path.open("wb", buffering=0) as output:
        process = subprocess.Popen(
            command,
            cwd=REPO_ROOT,
            stdout=output,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        try:
            returncode = process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
            termination = stop_process_group(process)
            returncode = 124
        except KeyboardInterrupt:
            interrupted = True
            termination = stop_process_group(process)
            returncode = 130
    lingering_group = process_group_exists(process.pid)
    lingering_process = process.poll() is None
    if lingering_group or lingering_process:
        termination = stop_process_group(process)
        lingering_group = process_group_exists(process.pid)
        lingering_process = process.poll() is None
    return {
        "returncode": returncode,
        "timed_out": timed_out,
        "interrupted": interrupted,
        "termination": termination,
        "process_group_cleanup": not lingering_group and not lingering_process,
        "host_wall_seconds": time.monotonic() - started,
    }


def remove_owned_temp(path: Path) -> bool:
    resolved = path.resolve()
    temp_parent = Path(tempfile.gettempdir()).resolve()
    if resolved.parent != temp_parent or not resolved.name.startswith("whusp-g0b-"):
        raise BenchmarkError(f"refusing to remove non-owned temp path: {resolved}")
    if resolved.exists():
        shutil.rmtree(resolved)
    return not resolved.exists()


def build_timer(compiler: Path, controller_root: Path, setup_dir: Path) -> Path:
    output = controller_root / "rust_build_timer"
    command = [
        str(compiler),
        "-static",
        "-O2",
        "-Wall",
        "-Wextra",
        "-Werror",
        "-o",
        str(output),
        str(TIMER_SOURCE),
    ]
    (setup_dir / "timer-compile-command.txt").write_text(
        command_text(command), encoding="utf-8"
    )
    completed = run_capture(command)
    (setup_dir / "timer-compile.log").write_text(completed.stdout, encoding="utf-8")
    if completed.returncode != 0:
        raise BenchmarkError(f"timer compiler exited with {completed.returncode}")
    if not output.is_file() or output.stat().st_size <= 0:
        raise BenchmarkError("timer compiler did not produce a non-empty binary")
    output.chmod(0o755)
    return output


def run_setup_command(command: list[str], log: list[str]) -> None:
    log.append("+ " + command_text(command))
    completed = run_capture(command)
    log.append(completed.stdout)
    if completed.returncode != 0:
        raise BenchmarkError(
            f"setup command exited with {completed.returncode}: {shlex.join(command)}"
        )


def build_script_disk(
    *,
    temp_root: Path,
    timer_binary: Path,
    identity: dict[str, str],
    image_size: str,
    setup_log: Path,
) -> Path:
    staging = temp_root / "staging"
    staging.mkdir()
    entry = staging / "entry.sh"
    entry.write_text(render_guest(identity), encoding="utf-8")
    entry.chmod(0o755)
    installed_timer = staging / "rust_build_timer"
    shutil.copy2(timer_binary, installed_timer)
    installed_timer.chmod(0o755)
    image = temp_root / "g0-rust-hello.img"
    output: list[str] = []
    try:
        run_setup_command([require_command("truncate"), "-s", image_size, str(image)], output)
        run_setup_command(
            [
                require_command("mkfs.ext4"),
                "-q",
                "-F",
                "-N",
                "8192",
                "-O",
                "^orphan_file,^metadata_csum_seed,^metadata_csum,^64bit,^has_journal",
                "-d",
                str(staging),
                str(image),
            ],
            output,
        )
    finally:
        setup_log.write_text("".join(output), encoding="utf-8")
    return image


def qemu_command(
    *,
    architecture: Architecture,
    smp: int,
    mem: str,
    kernel: Path,
    disk: Path,
    aux_disk: Path,
    overlay_root: Path,
) -> list[str]:
    return [
        require_command("make"),
        "--no-print-directory",
        "-C",
        str(REPO_ROOT / "os"),
        f"ARCH={architecture.make_arch}",
        "MODE=release",
        "PERF_COUNTERS=0",
        f"SMP={smp}",
        f"MEM={mem}",
        f"KERNEL_ELF={kernel}",
        f"PRIMARY_DISK={disk}",
        f"AUX_DISK={aux_disk}",
        f"QEMU_OVERLAY_ROOT={overlay_root}",
        "run-inner",
    ]


def run_trial(
    *,
    trial: Trial,
    run_id: str,
    architecture: Architecture,
    smp: int,
    mem: str,
    kernel: Path,
    disk: Path,
    timer_binary: Path,
    image_size: str,
    timeout: float,
    evidence_dir: Path,
) -> dict[str, Any]:
    trial_dir = evidence_dir / trial.directory_name
    trial_dir.mkdir()
    identity = {
        "run_id": run_id,
        "arch": architecture.name,
        "kind": trial.kind,
        "sample": str(trial.sample),
        "smp": str(smp),
        "mem": mem,
    }
    temp_root: Path | None = None
    overlay_root: Path | None = None
    sample: dict[str, Any] = {
        "schema_version": 1,
        "state": "prepared",
        "valid": False,
        "identity": identity,
        "trial_directory": trial.directory_name,
        "temp_root": None,
        "started_at": utc_now(),
        "errors": [],
    }
    errors: list[str] = []
    interrupted = False
    try:
        temp_root = Path(tempfile.mkdtemp(prefix="whusp-g0b-"))
        sample["temp_root"] = str(temp_root)
        overlay_root = temp_root / "overlays"
        overlay_root.mkdir()
        sample["host_load_before"] = host_load_snapshot()
        write_json(trial_dir / "sample.json", sample)
        aux_disk = build_script_disk(
            temp_root=temp_root,
            timer_binary=timer_binary,
            identity=identity,
            image_size=image_size,
            setup_log=trial_dir / "setup.log",
        )
        command = qemu_command(
            architecture=architecture,
            smp=smp,
            mem=mem,
            kernel=kernel,
            disk=disk,
            aux_disk=aux_disk,
            overlay_root=overlay_root,
        )
        (trial_dir / "command.txt").write_text(command_text(command), encoding="utf-8")
        process = run_logged(command, trial_dir / "serial.log", timeout)
        sample["process"] = process
        interrupted = bool(process["interrupted"])
        if process["timed_out"]:
            errors.append("QEMU timed out")
        if process["returncode"] != 0:
            errors.append(f"QEMU/make exited with {process['returncode']}")
        if not process["process_group_cleanup"]:
            errors.append("QEMU process group remains after reap/cleanup")
        raw_log = (trial_dir / "serial.log").read_bytes()
        log = raw_log.decode("utf-8", errors="replace")
        guest_result, guest_errors = validate_guest_log(
            log,
            identity=identity,
            architecture=architecture,
            smp=smp,
            mem=mem,
        )
        overlay_path, overlay_errors = validate_overlay_log(log, overlay_root)
        sample["guest_result"] = guest_result
        sample["qemu_overlay_dir"] = overlay_path
        errors.extend(guest_errors)
        errors.extend(overlay_errors)
    except (BenchmarkError, OSError, ValueError) as error:
        errors.append(str(error))
    finally:
        try:
            overlay_root_empty = (
                True
                if overlay_root is None or not overlay_root.is_dir()
                else not any(overlay_root.iterdir())
            )
        except OSError as error:
            overlay_root_empty = False
            errors.append(f"cannot inspect owned overlay root: {error}")
        sample["overlay_root_empty_before_host_cleanup"] = overlay_root_empty
        if not overlay_root_empty:
            errors.append(f"owned overlay root retained entries: {overlay_root}")
        if temp_root is None:
            sample["temp_cleanup"] = True
        else:
            try:
                sample["temp_cleanup"] = remove_owned_temp(temp_root)
            except (BenchmarkError, OSError) as error:
                sample["temp_cleanup"] = False
                errors.append(f"temp cleanup failed: {error}")
        if not sample["temp_cleanup"]:
            errors.append(f"owned temp root remains: {temp_root}")
        try:
            sample["host_load_after"] = host_load_snapshot()
        except (BenchmarkError, OSError) as error:
            errors.append(f"cannot capture final host load: {error}")
        sample["finished_at"] = utc_now()
        sample["errors"] = errors
        sample["valid"] = not errors
        sample["state"] = "valid" if sample["valid"] else "invalid"
        write_json(trial_dir / "sample.json", sample)
    if interrupted:
        raise KeyboardInterrupt
    return sample


def format_seconds(nanoseconds: Decimal) -> str:
    return f"{nanoseconds / Decimal(1_000_000_000):.9f}"


def aggregate(samples: list[dict[str, Any]], warmups: int, measured: int) -> dict[str, Any]:
    valid_warmups = [
        sample
        for sample in samples
        if sample["identity"]["kind"] == "warmup" and sample["valid"]
    ]
    valid_measured = [
        sample
        for sample in samples
        if sample["identity"]["kind"] == "measured" and sample["valid"]
    ]
    complete = len(samples) == warmups + measured
    all_valid = complete and len(valid_warmups) == warmups and len(valid_measured) == measured
    values = [
        int(sample["guest_result"]["elapsed_ns"]) for sample in valid_measured
    ]
    result: dict[str, Any] = {
        "schema_version": 1,
        "updated_at": utc_now(),
        "required_warmups": warmups,
        "required_measured": measured,
        "completed_trials": len(samples),
        "valid_warmups": len(valid_warmups),
        "valid_measured": len(valid_measured),
        "all_required_samples_valid": all_valid,
        "run_valid": None,
        "measured_elapsed_ns": values,
        "measured_elapsed_seconds": [format_seconds(Decimal(value)) for value in values],
        "median_elapsed_ns": None,
        "median_elapsed_seconds": None,
        "min_elapsed_seconds": None,
        "max_elapsed_seconds": None,
        "spread_seconds": None,
        "goal_threshold_seconds": "1.000000000",
        "goal_met": None,
    }
    if all_valid:
        ordered = sorted(values)
        count = len(ordered)
        if count % 2:
            median_ns = Decimal(ordered[count // 2])
        else:
            median_ns = Decimal(ordered[count // 2 - 1] + ordered[count // 2]) / 2
        minimum = Decimal(ordered[0])
        maximum = Decimal(ordered[-1])
        result.update(
            {
                "median_elapsed_ns": str(median_ns),
                "median_elapsed_seconds": format_seconds(median_ns),
                "min_elapsed_seconds": format_seconds(minimum),
                "max_elapsed_seconds": format_seconds(maximum),
                "spread_seconds": format_seconds(maximum - minimum),
                "goal_met": median_ns < Decimal(1_000_000_000),
            }
        )
    return result


def trial_plan(warmups: int, measured: int) -> list[Trial]:
    trials: list[Trial] = []
    ordinal = 0
    for sample in range(1, warmups + 1):
        trials.append(Trial(ordinal=ordinal, kind="warmup", sample=sample))
        ordinal += 1
    for sample in range(1, measured + 1):
        trials.append(Trial(ordinal=ordinal, kind="measured", sample=sample))
        ordinal += 1
    return trials


def version_line(command: list[str]) -> str:
    completed = run_capture(command)
    if completed.returncode != 0:
        raise BenchmarkError(f"version command failed: {shlex.join(command)}")
    lines = completed.stdout.splitlines()
    if not lines:
        raise BenchmarkError(f"version command produced no output: {shlex.join(command)}")
    return lines[0]


def current_git_head() -> str:
    completed = run_capture([require_command("git"), "rev-parse", "HEAD"])
    if completed.returncode != 0:
        raise BenchmarkError("cannot resolve current Git HEAD")
    return completed.stdout.strip()


def run_cell(args: argparse.Namespace) -> int:
    assert args.arch is not None
    assert args.output_dir is not None
    architecture = ARCHITECTURES[args.arch]
    run_id = args.run_id or (
        f"g0b-{args.arch}-{args.smp}c-"
        + datetime.now(timezone.utc).strftime("%Y%m%dt%H%M%Sz")
        + f"-{os.getpid()}"
    )
    validate_token("run_id", run_id)
    assert args.kernel_elf is not None
    assert args.test_disk is not None
    kernel = args.kernel_elf.resolve()
    disk = args.test_disk.resolve()
    output_dir = args.output_dir.resolve()
    if output_dir.exists():
        raise BenchmarkError(f"output directory already exists: {output_dir}")
    if not kernel.is_file():
        raise BenchmarkError(f"kernel ELF does not exist: {kernel}")
    if not disk.is_file():
        raise BenchmarkError(f"test disk does not exist: {disk}")
    if not all(
        path.is_file()
        for path in (RUNNER_SOURCE, OS_MAKEFILE, GUEST_TEMPLATE, TIMER_SOURCE)
    ):
        raise BenchmarkError("G0-B runner/Makefile/guest template/timer source is missing")
    compiler = compiler_for(args.arch)
    require_command("qemu-img")
    qemu_command_name = (
        "qemu-system-riscv64" if args.arch == "rv" else "qemu-system-loongarch64"
    )
    require_command(qemu_command_name)
    require_command("mkfs.ext4")
    require_command("truncate")
    output_dir.mkdir(parents=True)
    setup_dir = output_dir / "setup"
    setup_dir.mkdir()
    input_metadata_before = selected_input_metadata(
        kernel=kernel, disk=disk, compiler=compiler
    )
    manifest = {
        "schema_version": 1,
        "run_id": run_id,
        "started_at": utc_now(),
        "git_head": current_git_head(),
        "architecture": args.arch,
        "make_arch": architecture.make_arch,
        "smp": args.smp,
        "mem": args.mem,
        "run_inner_make_parameters": {"MODE": "release", "PERF_COUNTERS": "0"},
        "kernel_elf_provenance": {
            "built_by_runner": False,
            "build_configuration_verified_by_runner": False,
        },
        "warmups": args.warmups,
        "measured_samples": args.samples,
        "timeout_seconds": args.timeout,
        "image_size": args.image_size,
        "cache_contract": {
            "guest_cache": "cold-fresh-qemu-and-tmp-scratch",
            "host_cache": (
                "warm-after-cell-warmup"
                if args.warmups > 0
                else "current-host-cache-without-cell-warmup"
            ),
            "host_page_cache_evicted": False,
            "public_prebuild_sequence": [
                "rustc --version",
                "cargo --version",
                "cargo new --vcs none /tmp/minibuild",
            ],
        },
        "input_metadata_before": input_metadata_before,
        "timer_compiler_version": version_line([str(compiler), "--version"]),
        "qemu_version": version_line([qemu_command_name, "--version"]),
        "host_load_before": host_load_snapshot(),
    }
    write_json(output_dir / "manifest.json", manifest)
    controller_root: Path | None = None
    samples: list[dict[str, Any]] = []
    aggregate_path = output_dir / "aggregate.json"
    failure = False
    interrupted = False
    try:
        write_json(aggregate_path, aggregate(samples, args.warmups, args.samples))
        controller_root = Path(tempfile.mkdtemp(prefix="whusp-g0b-controller-"))
        timer_binary = build_timer(compiler, controller_root, setup_dir)
        for trial in trial_plan(args.warmups, args.samples):
            print(
                f"[{utc_now()}] {args.arch} {args.smp}C {args.mem} "
                f"{trial.kind} sample={trial.sample} start",
                flush=True,
            )
            sample = run_trial(
                trial=trial,
                run_id=run_id,
                architecture=architecture,
                smp=args.smp,
                mem=args.mem,
                kernel=kernel,
                disk=disk,
                timer_binary=timer_binary,
                image_size=args.image_size,
                timeout=args.timeout,
                evidence_dir=output_dir,
            )
            samples.append(sample)
            current = aggregate(samples, args.warmups, args.samples)
            write_json(aggregate_path, current)
            elapsed = sample.get("guest_result", {}).get("elapsed_seconds", "invalid")
            print(
                f"[{utc_now()}] {args.arch} {args.smp}C {trial.kind} "
                f"sample={trial.sample} valid={sample['valid']} elapsed_s={elapsed}",
                flush=True,
            )
            if not sample["valid"]:
                failure = True
                break
    except KeyboardInterrupt:
        interrupted = True
        failure = True
    except (BenchmarkError, OSError, subprocess.SubprocessError) as error:
        failure = True
        manifest["setup_or_runner_error"] = str(error)
        print(f"G0-B runner error: {error}", file=sys.stderr, flush=True)
    finally:
        if controller_root is None:
            controller_cleanup = True
        else:
            try:
                controller_cleanup = remove_owned_temp(controller_root)
            except (BenchmarkError, OSError) as error:
                controller_cleanup = False
                manifest["controller_cleanup_error"] = str(error)
                failure = True
        manifest["controller_temp_root"] = (
            str(controller_root) if controller_root is not None else None
        )
        manifest["controller_cleanup"] = controller_cleanup
        input_metadata_after = selected_input_metadata(
            kernel=kernel, disk=disk, compiler=compiler
        )
        manifest["input_metadata_after"] = input_metadata_after
        manifest["input_metadata_stable"] = (
            input_metadata_after == input_metadata_before
        )
        if not manifest["input_metadata_stable"]:
            manifest["input_metadata_error"] = "selected input metadata changed during run"
            failure = True
        manifest["git_head_after"] = current_git_head()
        manifest["git_head_stable"] = manifest["git_head_after"] == manifest["git_head"]
        if not manifest["git_head_stable"]:
            manifest["git_head_error"] = "Git HEAD changed during run"
            failure = True
        manifest["host_load_after"] = host_load_snapshot()
        manifest["finished_at"] = utc_now()
        manifest["interrupted"] = interrupted
        final_aggregate = aggregate(samples, args.warmups, args.samples)
        final_aggregate["run_valid"] = (
            not failure
            and controller_cleanup
            and final_aggregate["all_required_samples_valid"]
        )
        if not final_aggregate["run_valid"]:
            final_aggregate["all_required_samples_valid"] = False
            final_aggregate["goal_met"] = None
        write_json(aggregate_path, final_aggregate)
        write_json(output_dir / "manifest.json", manifest)
    if interrupted:
        return 130
    if failure or not final_aggregate["all_required_samples_valid"]:
        return 1
    print(
        f"G0-B cell PASS arch={args.arch} smp={args.smp} mem={args.mem} "
        f"median_s={final_aggregate['median_elapsed_seconds']} "
        f"goal_met={final_aggregate['goal_met']}",
        flush=True,
    )
    return 0


def synthetic_log(identity: dict[str, str], architecture: Architecture, smp: int, mem: str) -> str:
    mask = (1 << smp) - 1
    leader = 1 if smp > 1 else 0
    requested = mask & ~(1 << leader)
    return "\n".join(
        [
            f"board config: clock_freq=10000000, memory_end={expected_memory_end(architecture.name, mem):#x}, uart=0x0, plic=0x0",
            f"cpu topology: possible={smp} online=1 possible_mask={mask:#x} online_mask=0x1 boot_logical=0 boot_hw_id=0 hw_ids=[]",
            "smp invariants: boot_entries=1 global_init_entries=1",
            f"smp schedulers: active_mask={mask:#x} count={smp}",
            "G0_RUST_HELLO_START "
            + " ".join(f"{key}={identity[key]}" for key in ("run_id", "arch", "kind", "sample", "smp", "mem")),
            "G0_RUST_HELLO_RESULT "
            + " ".join(f"{key}={identity[key]}" for key in ("run_id", "arch", "kind", "sample", "smp", "mem"))
            + f" uname={architecture.uname} nproc={smp} cargo_version=1.0.0 rustc_version=1.0.0"
            " tmp_mount=1 tmp_writable=1 elapsed_ns=900000000 timer_exited=1"
            " timer_exit_code=0 timer_signaled=0 timer_signal=0 timer_rc=0"
            " uptime_before=10.00 uptime_after=10.90 lock_created=1"
            " artifact_bytes=100 output_bytes=14 output_ok=1 ok=1",
            "G0_RUST_HELLO_PASS "
            + " ".join(f"{key}={identity[key]}" for key in ("run_id", "arch", "kind", "sample", "smp", "mem")),
            f"smp shutdown: leader={leader} requested={requested:#x} stopped={requested:#x} missing=0x0 failure=false",
        ]
    ) + "\n"


def self_test() -> int:
    architecture = ARCHITECTURES["rv"]
    identity = {
        "run_id": "self-test",
        "arch": "rv",
        "kind": "measured",
        "sample": "1",
        "smp": "8",
        "mem": "8G",
    }
    render_guest(identity)
    try:
        render_guest({**identity, "run_id": "bad'\nG0_RUST_HELLO_PASS"})
    except BenchmarkError:
        pass
    else:
        raise BenchmarkError("unsafe template token was accepted")
    good = synthetic_log(identity, architecture, 8, "8G")
    parsed, errors = validate_guest_log(
        good, identity=identity, architecture=architecture, smp=8, mem="8G"
    )
    if errors or parsed.get("elapsed_ns") != "900000000":
        raise BenchmarkError(f"positive synthetic log failed: {errors}")
    _, errors = validate_guest_log(
        good + good.splitlines()[6] + "\n",
        identity=identity,
        architecture=architecture,
        smp=8,
        mem="8G",
    )
    if not any("PASS" in error for error in errors):
        raise BenchmarkError("duplicate PASS marker was accepted")
    malformed = good.replace("G0_RUST_HELLO_PASS ", "G0_RUST_HELLO_PASS malformed ")
    _, errors = validate_guest_log(
        malformed, identity=identity, architecture=architecture, smp=8, mem="8G"
    )
    if not any("malformed" in error for error in errors):
        raise BenchmarkError("malformed marker was accepted")
    bad_clock = good.replace("uptime_after=10.90", "uptime_after=19.90")
    _, errors = validate_guest_log(
        bad_clock, identity=identity, architecture=architecture, smp=8, mem="8G"
    )
    if not any("disagreement" in error for error in errors):
        raise BenchmarkError("timer/uptime mismatch was accepted")
    with tempfile.TemporaryDirectory(prefix="whusp-g0b-selftest-") as directory:
        root = Path(directory)
        overlay = root / "whusp-qemu.abcdef"
        overlay_log = (
            f"WHUSP_QEMU_OVERLAY state=created path={overlay}\n"
            f"WHUSP_QEMU_OVERLAY state=cleaned path={overlay}\n"
        )
        _, errors = validate_overlay_log(overlay_log, root)
        if errors:
            raise BenchmarkError(f"positive overlay markers failed: {errors}")
    print("G0-B synthetic self-test PASS")
    return 0


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Run one clean Rust Hello World architecture/SMP cell. Each warmup "
            "or measured sample boots an independent guest."
        )
    )
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--arch", choices=("rv", "la"))
    parser.add_argument("--smp", type=int, default=8)
    parser.add_argument("--mem", default="8G")
    parser.add_argument("--kernel-elf", type=Path)
    parser.add_argument("--test-disk", type=Path)
    parser.add_argument("--output-dir", type=Path)
    parser.add_argument("--run-id")
    parser.add_argument("--warmups", type=int, default=1)
    parser.add_argument("--samples", type=int, default=3)
    parser.add_argument("--timeout", type=float, default=180.0)
    parser.add_argument("--image-size", default="64M")
    args = parser.parse_args()
    if args.self_test:
        return args
    if args.arch is None:
        parser.error("--arch is required")
    if args.output_dir is None:
        parser.error("--output-dir is required")
    if args.kernel_elf is None:
        parser.error("--kernel-elf is required")
    if args.test_disk is None:
        parser.error("--test-disk is required")
    if not 1 <= args.smp <= MAX_CPUS:
        parser.error(f"--smp must be in 1..{MAX_CPUS}")
    if not MEM_RE.fullmatch(args.mem):
        parser.error("--mem must be a positive QEMU size ending in M or G")
    if args.warmups < 0:
        parser.error("--warmups must be nonnegative")
    if args.samples < 1:
        parser.error("--samples must be positive")
    if args.timeout <= 0:
        parser.error("--timeout must be positive")
    if not IMAGE_SIZE_RE.fullmatch(args.image_size):
        parser.error("--image-size must be a positive size ending in M or G")
    if args.run_id is not None and not TOKEN_RE.fullmatch(args.run_id):
        parser.error("--run-id must match [A-Za-z0-9._-]+")
    return args


def main() -> int:
    args = parse_args()
    try:
        if args.self_test:
            return self_test()
        return run_cell(args)
    except BenchmarkError as error:
        print(f"G0-B benchmark failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
