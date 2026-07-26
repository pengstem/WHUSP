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
GUEST_WORKLOAD_PATH = "/x1/g0-rust-hello.sh"
MAX_CPUS = 12
PROCESS_STOP_TIMEOUT_SECONDS = 2.0
TOKEN_RE = re.compile(r"[A-Za-z0-9._-]+")
MEM_RE = re.compile(r"[1-9][0-9]*[MG]")
IMAGE_SIZE_RE = re.compile(r"[1-9][0-9]*[MG]")
ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")
PANIC_RE = re.compile(r"panicked at|kernel panic|assertion failed", re.IGNORECASE)
FORBIDDEN_SHELL_DIAGNOSTICS = (
    "set: -u: invalid option",
    "waitpid: Interrupted system call",
)
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
BLOCK_IO_POLICY_RE = re.compile(
    r"(?:\[\s*INFO\]\s+)?"
    r"KERN: block io policy mode=(?P<block_io>auto|force-sync) "
    r"irq_ready=(?P<irq_ready>true|false) "
    r"nonblocking=(?P<nonblocking>true|false) "
    r"perf_counters=(?P<perf_counters>true|false)"
)
IDENTITY_PATTERN = (
    r"run_id=(?P<run_id>[A-Za-z0-9._-]+) "
    r"arch=(?P<arch>rv|la) "
    r"kind=(?P<kind>warmup|measured) "
    r"sample=(?P<sample>[0-9]+) "
    r"smp=(?P<smp>[0-9]+) "
    r"mem=(?P<mem>[A-Za-z0-9._-]+) "
    r"block_io=(?P<block_io>auto|force-sync) "
    r"perf=(?P<perf>[01])"
)
START_RE = re.compile(r"G0_RUST_HELLO_START " + IDENTITY_PATTERN)
PASS_RE = re.compile(r"G0_RUST_HELLO_PASS " + IDENTITY_PATTERN)
FAIL_RE = re.compile(
    r"G0_RUST_HELLO_FAIL " + IDENTITY_PATTERN + r" stage=(?P<stage>[A-Za-z0-9._-]+)"
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
PERF_BEGIN_RE = re.compile(
    r"G0_RUST_HELLO_PERF_BEGIN " + IDENTITY_PATTERN + r" point=(?P<point>before|after)"
)
PERF_END_RE = re.compile(
    r"G0_RUST_HELLO_PERF_END " + IDENTITY_PATTERN + r" point=(?P<point>before|after)"
)
PERF_VALUE_RE = re.compile(r"(?P<key>[a-z][a-z0-9_]*) (?P<value>[0-9]+)")
PERF_SYSCALL_KEY_RE = re.compile(
    r"profile_syscall_(?P<syscall>[0-9]+)_"
    r"(?P<metric>calls|total_ticks|total_us|avg_ns|max_us)"
)
PERF_SYSCALL_METRICS = ("calls", "total_ticks", "total_us", "avg_ns", "max_us")

# These keys freeze the IO0 causal contract and their relative procfs order.
# The parser also preserves every additional integer key/value line verbatim.
PERF_REQUIRED_KEYS = (
    "perf_counters_enabled",
    "scheduler_fetch_calls",
    "scheduler_scanned_tasks",
    "wakeup_front_successes",
    "wakeup_back_successes",
    "scheduler_normal_requeue_calls",
    "vfs_read_all_calls",
    "vfs_read_all_backend_reads",
    "vfs_read_backend_calls",
    "vfs_read_backend_bytes",
    "mmap_private_faults",
    "kperf_timing_enabled",
    "profile_ext4_read_calls",
    "profile_ext4_read_total_ticks",
    "profile_ext4_read_total_us",
    "profile_page_fault_calls",
    "profile_page_fault_total_ticks",
    "profile_page_fault_total_us",
    "profile_scheduler_fetch_calls",
    "profile_scheduler_fetch_total_ticks",
    "profile_scheduler_fetch_total_us",
    "profile_vfs_read_backend_calls",
    "profile_vfs_read_backend_total_ticks",
    "profile_vfs_read_backend_total_us",
    "profile_vfs_read_all_backend_calls",
    "profile_vfs_read_all_backend_total_ticks",
    "profile_vfs_read_all_backend_total_us",
    "profile_mmap_fault_read_calls",
    "profile_mmap_fault_read_total_ticks",
    "profile_mmap_fault_read_total_us",
    "kperf_syscall_timing_enabled",
    "block_cache_device_read_submit",
    "block_cache_device_read_blocks",
    "block_cache_device_write_submit",
    "block_cache_device_write_blocks",
    "block_io_nonblocking_requested",
    "block_io_nb_read_submits",
    "block_io_nb_write_submits",
    "block_io_nb_read_waits",
    "block_io_nb_write_waits",
    "block_io_nb_read_completions",
    "block_io_nb_write_completions",
    "block_io_fallback_sync_reads",
    "block_io_fallback_sync_writes",
    "block_io_fallback_unsafe_reads",
    "block_io_fallback_unsafe_writes",
    "block_io_fallback_no_ready_reads",
    "block_io_fallback_no_ready_writes",
    "block_io_sync_read_submits",
    "block_io_sync_write_submits",
    "block_io_irq_acks",
    "block_io_completion_signals",
    "block_io_completion_wakeups",
    "exec_elf_header_bytes_read",
    "exec_phdr_bytes_read",
    "exec_eager_segment_bytes_read",
    "exec_lazy_segment_faults",
    "exec_lazy_segment_bytes_read",
    "exec_lazy_page_cache_faults",
    "exec_lazy_page_cache_hits",
    "exec_lazy_page_cache_misses",
    "exec_lazy_page_cache_bytes_read",
)
PERF_SELECTED_DELTA_KEYS = tuple(
    key
    for key in PERF_REQUIRED_KEYS
    if key
    not in {
        "perf_counters_enabled",
        "kperf_timing_enabled",
        "kperf_syscall_timing_enabled",
        "block_io_nonblocking_requested",
    }
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


def run_capture(
    command: list[str], *, cwd: Path = REPO_ROOT
) -> subprocess.CompletedProcess[str]:
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
        "@BLOCK_IO_MODE@": identity["block_io"],
        "@PERF_COUNTERS@": identity["perf"],
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


def render_guest_launcher() -> str:
    return (
        "#!/musl/busybox sh\n"
        f"exec /musl/busybox ash {GUEST_WORKLOAD_PATH} || exit 127\n"
    )


def normalized_lines(log: str) -> list[str]:
    return [ANSI_RE.sub("", line.rstrip("\r")) for line in log.splitlines()]


def marker_identity_errors(
    groups: dict[str, str], expected: dict[str, str], label: str
) -> list[str]:
    errors: list[str] = []
    for field in (
        "run_id",
        "arch",
        "kind",
        "sample",
        "smp",
        "mem",
        "block_io",
        "perf",
    ):
        if groups[field] != expected[field]:
            errors.append(
                f"{label} {field} mismatch: {groups[field]!r} != {expected[field]!r}"
            )
    return errors


def parse_perf_snapshot(
    snapshot_lines: list[str], *, label: str
) -> tuple[dict[str, Any], list[str]]:
    errors: list[str] = []
    ordered_keys: list[str] = []
    values: dict[str, int] = {}
    if not snapshot_lines:
        errors.append(f"{label} perf snapshot is empty")
    if len(snapshot_lines) > 4096:
        errors.append(
            f"{label} perf snapshot has too many lines: {len(snapshot_lines)}"
        )
    for line in snapshot_lines:
        if len(line) > 256:
            errors.append(f"{label} perf snapshot line is too long")
            continue
        match = PERF_VALUE_RE.fullmatch(line)
        if match is None:
            errors.append(f"{label} perf snapshot has malformed line: {line!r}")
            continue
        key = match.group("key")
        if key in values:
            errors.append(f"{label} perf snapshot has duplicate key: {key}")
            continue
        ordered_keys.append(key)
        values[key] = int(match.group("value"))

    missing = [key for key in PERF_REQUIRED_KEYS if key not in values]
    if missing:
        errors.append(f"{label} perf snapshot is missing required keys: {missing!r}")
    else:
        required_indices = [ordered_keys.index(key) for key in PERF_REQUIRED_KEYS]
        if required_indices != sorted(required_indices):
            errors.append(f"{label} perf snapshot required keys are out of order")
    for enabled_key in (
        "perf_counters_enabled",
        "kperf_timing_enabled",
        "kperf_syscall_timing_enabled",
    ):
        if values.get(enabled_key) != 1:
            errors.append(f"{label} perf snapshot {enabled_key} must be 1")

    syscall_entries: list[tuple[int, str, str]] = []
    syscall_indices: list[int] = []
    for index, key in enumerate(ordered_keys):
        match = PERF_SYSCALL_KEY_RE.fullmatch(key)
        if match is None:
            continue
        syscall_indices.append(index)
        syscall_entries.append(
            (int(match.group("syscall")), match.group("metric"), key)
        )
    syscall_ids: list[int] = []
    if syscall_entries:
        sentinel_index = (
            ordered_keys.index("kperf_syscall_timing_enabled")
            if "kperf_syscall_timing_enabled" in values
            else -1
        )
        expected_indices = list(
            range(syscall_indices[0], syscall_indices[0] + len(syscall_indices))
        )
        if (
            syscall_indices != expected_indices
            or syscall_indices[0] != sentinel_index + 1
        ):
            errors.append(
                f"{label} perf snapshot syscall timing keys are not one contiguous suffix"
            )
        for offset in range(0, len(syscall_entries), len(PERF_SYSCALL_METRICS)):
            group = syscall_entries[offset : offset + len(PERF_SYSCALL_METRICS)]
            if len(group) != len(PERF_SYSCALL_METRICS):
                errors.append(
                    f"{label} perf snapshot has a partial syscall timing group"
                )
                break
            syscall_id = group[0][0]
            if syscall_id >= 512:
                errors.append(
                    f"{label} perf snapshot syscall ID is outside 0..511: {syscall_id}"
                )
            if [item[0] for item in group] != [syscall_id] * len(group) or [
                item[1] for item in group
            ] != list(PERF_SYSCALL_METRICS):
                errors.append(
                    f"{label} perf snapshot malformed syscall timing group at {syscall_id}"
                )
            syscall_ids.append(syscall_id)
        if syscall_ids != sorted(set(syscall_ids)):
            errors.append(
                f"{label} perf snapshot syscall timing IDs are not unique ascending"
            )

    return (
        {
            "raw_lines": snapshot_lines,
            "ordered_keys": ordered_keys,
            "values": values,
            "syscall_ids": syscall_ids,
        },
        errors,
    )


def validate_perf_snapshot_pair(
    before: dict[str, Any],
    after: dict[str, Any],
    *,
    block_io_mode: str,
) -> tuple[dict[str, int], list[str]]:
    errors: list[str] = []
    before_keys = [
        key
        for key in before["ordered_keys"]
        if PERF_SYSCALL_KEY_RE.fullmatch(key) is None
    ]
    after_keys = [
        key
        for key in after["ordered_keys"]
        if PERF_SYSCALL_KEY_RE.fullmatch(key) is None
    ]
    if before_keys != after_keys:
        errors.append("before/after perf snapshot non-syscall key set/order differs")
    if not set(before["syscall_ids"]).issubset(after["syscall_ids"]):
        errors.append("after perf snapshot lost a syscall timing group present before")

    expected_nonblocking = 1 if block_io_mode == "auto" else 0
    for label, snapshot in (("before", before), ("after", after)):
        actual = snapshot["values"].get("block_io_nonblocking_requested")
        if actual != expected_nonblocking:
            errors.append(
                f"{label} block_io_nonblocking_requested={actual!r}, "
                f"expected {expected_nonblocking}"
            )

    deltas: dict[str, int] = {}
    for key in PERF_SELECTED_DELTA_KEYS:
        if key not in before["values"] or key not in after["values"]:
            continue
        delta = after["values"][key] - before["values"][key]
        deltas[key] = delta
        if delta < 0:
            errors.append(f"selected perf counter moved backwards: {key} delta={delta}")
    if tuple(deltas) != PERF_SELECTED_DELTA_KEYS:
        missing = [key for key in PERF_SELECTED_DELTA_KEYS if key not in deltas]
        errors.append(f"selected perf deltas are incomplete: {missing!r}")

    nb_read_keys = (
        "block_io_nb_read_submits",
        "block_io_nb_read_waits",
        "block_io_nb_read_completions",
    )
    nb_write_keys = (
        "block_io_nb_write_submits",
        "block_io_nb_write_waits",
        "block_io_nb_write_completions",
    )
    if all(key in deltas for key in (*nb_read_keys, *nb_write_keys)):
        read_submits, read_waits, read_completions = (
            deltas[key] for key in nb_read_keys
        )
        write_submits, write_waits, write_completions = (
            deltas[key] for key in nb_write_keys
        )
        if block_io_mode == "auto":
            if read_submits <= 0:
                errors.append("auto mode requires block_io_nb_read_submits delta > 0")
            if not read_submits == read_waits == read_completions:
                errors.append(
                    "auto read nonblocking lifecycle mismatch: "
                    f"submits={read_submits} waits={read_waits} "
                    f"completions={read_completions}"
                )
            if not write_submits == write_waits == write_completions:
                errors.append(
                    "auto write nonblocking lifecycle mismatch: "
                    f"submits={write_submits} waits={write_waits} "
                    f"completions={write_completions}"
                )
        else:
            nonzero = {
                key: deltas[key]
                for key in (*nb_read_keys, *nb_write_keys)
                if deltas[key] != 0
            }
            if nonzero:
                errors.append(
                    f"force-sync mode requires zero nonblocking lifecycle deltas: {nonzero!r}"
                )
            if deltas.get("block_io_sync_read_submits", 0) <= 0:
                errors.append(
                    "force-sync mode requires block_io_sync_read_submits delta > 0"
                )

    for direction in ("read", "write"):
        device_key = f"block_cache_device_{direction}_submit"
        nonblocking_key = f"block_io_nb_{direction}_submits"
        synchronous_key = f"block_io_sync_{direction}_submits"
        accounting_keys = (device_key, nonblocking_key, synchronous_key)
        if all(key in deltas for key in accounting_keys):
            routed_submits = deltas[nonblocking_key] + deltas[synchronous_key]
            if deltas[device_key] != routed_submits:
                errors.append(
                    f"{direction} submit accounting mismatch: "
                    f"{device_key}={deltas[device_key]} != "
                    f"{nonblocking_key}+{synchronous_key}={routed_submits}"
                )
    return deltas, errors


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
    parsed: dict[str, Any] = {}
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
    perf_begins: list[tuple[int, re.Match[str]]] = []
    perf_ends: list[tuple[int, re.Match[str]]] = []
    policies: list[tuple[int, re.Match[str]]] = []
    malformed_policies: list[str] = []
    malformed: list[str] = []
    for index, line in enumerate(lines):
        if "KERN: block io policy" in line:
            policy_match = BLOCK_IO_POLICY_RE.fullmatch(line)
            if policy_match:
                policies.append((index, policy_match))
            else:
                malformed_policies.append(line)
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
        match = PERF_BEGIN_RE.fullmatch(line)
        if match:
            perf_begins.append((index, match))
            continue
        match = PERF_END_RE.fullmatch(line)
        if match:
            perf_ends.append((index, match))
            continue
        malformed.append(line)

    if malformed_policies:
        errors.append(f"malformed block IO policy lines: {malformed_policies!r}")
    if len(policies) != 1:
        errors.append(f"expected one block IO policy marker, found {len(policies)}")
    else:
        policy = policies[0][1].groupdict()
        expected_policy = {
            "block_io": identity["block_io"],
            "irq_ready": "true",
            "nonblocking": "true" if identity["block_io"] == "auto" else "false",
            "perf_counters": "true" if identity["perf"] == "1" else "false",
        }
        for field, expected in expected_policy.items():
            if policy[field] != expected:
                errors.append(
                    f"block IO policy {field} mismatch: "
                    f"{policy[field]!r} != {expected!r}"
                )
        parsed["block_io_policy"] = {
            "mode": policy["block_io"],
            "irq_ready": policy["irq_ready"] == "true",
            "nonblocking": policy["nonblocking"] == "true",
            "perf_counters": policy["perf_counters"] == "true",
        }
        if len(starts) == 1 and policies[0][0] >= starts[0][0]:
            errors.append("block IO policy marker did not precede guest START")

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
            errors.extend(
                marker_identity_errors(matches[0][1].groupdict(), identity, label)
            )
    for label, matches in (("PERF_BEGIN", perf_begins), ("PERF_END", perf_ends)):
        for _, match in matches:
            errors.extend(
                marker_identity_errors(
                    match.groupdict(), identity, f"{label}/{match.group('point')}"
                )
            )
    if (
        len(starts) == len(results) == len(passes) == 1
        and not starts[0][0] < results[0][0] < passes[0][0]
    ):
        errors.append("START, RESULT and PASS markers are out of order")

    if identity["perf"] == "0":
        if perf_begins or perf_ends:
            errors.append("perf=0 guest emitted a perf snapshot marker")
        parsed["perf_snapshots"] = None
    else:
        before_begins = [
            item for item in perf_begins if item[1].group("point") == "before"
        ]
        before_ends = [item for item in perf_ends if item[1].group("point") == "before"]
        after_begins = [
            item for item in perf_begins if item[1].group("point") == "after"
        ]
        after_ends = [item for item in perf_ends if item[1].group("point") == "after"]
        perf_parts = (
            ("before BEGIN", before_begins),
            ("before END", before_ends),
            ("after BEGIN", after_begins),
            ("after END", after_ends),
        )
        for label, matches in perf_parts:
            if len(matches) != 1:
                errors.append(f"expected one perf {label} marker, found {len(matches)}")
        if all(len(matches) == 1 for _, matches in perf_parts):
            before_begin_index = before_begins[0][0]
            before_end_index = before_ends[0][0]
            after_begin_index = after_begins[0][0]
            after_end_index = after_ends[0][0]
            if (
                len(starts) == len(results) == len(passes) == 1
                and not starts[0][0]
                < before_begin_index
                < before_end_index
                < after_begin_index
                < after_end_index
                < results[0][0]
                < passes[0][0]
            ):
                errors.append(
                    "START, perf before/after, RESULT and PASS markers are out of order"
                )
            before, before_errors = parse_perf_snapshot(
                lines[before_begin_index + 1 : before_end_index], label="before"
            )
            after, after_errors = parse_perf_snapshot(
                lines[after_begin_index + 1 : after_end_index], label="after"
            )
            errors.extend(before_errors)
            errors.extend(after_errors)
            selected_deltas, pair_errors = validate_perf_snapshot_pair(
                before, after, block_io_mode=identity["block_io"]
            )
            errors.extend(pair_errors)
            parsed["perf_snapshots"] = {
                "before": before,
                "after": after,
                "selected_deltas": selected_deltas,
            }

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
    for diagnostic in FORBIDDEN_SHELL_DIAGNOSTICS:
        if diagnostic in normalized_log:
            errors.append(f"forbidden guest shell diagnostic: {diagnostic}")
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
            errors.append(
                f"shutdown stopped mask {stopped:#x} != requested {requested:#x}"
            )
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
    entry.write_text(render_guest_launcher(), encoding="utf-8")
    entry.chmod(0o755)
    workload = staging / Path(GUEST_WORKLOAD_PATH).name
    workload.write_text(render_guest(identity), encoding="utf-8")
    workload.chmod(0o755)
    installed_timer = staging / "rust_build_timer"
    shutil.copy2(timer_binary, installed_timer)
    installed_timer.chmod(0o755)
    image = temp_root / "g0-rust-hello.img"
    output: list[str] = []
    try:
        run_setup_command(
            [require_command("truncate"), "-s", image_size, str(image)], output
        )
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
    block_io_mode: str,
    perf_counters: int,
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
        f"BLOCK_IO_MODE={block_io_mode}",
        f"PERF_COUNTERS={perf_counters}",
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
    block_io_mode: str,
    perf_counters: int,
    kernel: Path,
    disk: Path,
    timer_binary: Path,
    image_size: str,
    timeout: float,
    evidence_dir: Path,
) -> dict[str, Any]:
    if block_io_mode not in {"auto", "force-sync"}:
        raise BenchmarkError(f"unsupported block IO mode: {block_io_mode!r}")
    if perf_counters not in {0, 1}:
        raise BenchmarkError(f"perf_counters must be 0 or 1: {perf_counters!r}")
    trial_dir = evidence_dir / trial.directory_name
    trial_dir.mkdir()
    identity = {
        "run_id": run_id,
        "arch": architecture.name,
        "kind": trial.kind,
        "sample": str(trial.sample),
        "smp": str(smp),
        "mem": mem,
        "block_io": block_io_mode,
        "perf": str(perf_counters),
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
            block_io_mode=block_io_mode,
            perf_counters=perf_counters,
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


def aggregate(
    samples: list[dict[str, Any]], warmups: int, measured: int
) -> dict[str, Any]:
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
    all_valid = (
        complete and len(valid_warmups) == warmups and len(valid_measured) == measured
    )
    values = [int(sample["guest_result"]["elapsed_ns"]) for sample in valid_measured]
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
        "measured_elapsed_seconds": [
            format_seconds(Decimal(value)) for value in values
        ],
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
        raise BenchmarkError(
            f"version command produced no output: {shlex.join(command)}"
        )
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
        raise BenchmarkError(
            "G0-B runner/Makefile/guest template/timer source is missing"
        )
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
        "block_io_mode": args.block_io_mode,
        "perf_counters": args.perf_counters,
        "run_inner_make_parameters": {
            "MODE": "release",
            "BLOCK_IO_MODE": args.block_io_mode,
            "PERF_COUNTERS": str(args.perf_counters),
        },
        "kernel_elf_provenance": {
            "built_by_runner": False,
            "runtime_feature_identity_verified": False,
            "verification_scope": (
                "runtime BLOCK_IO_MODE/PERF_COUNTERS identity only; "
                "the runner does not build the kernel ELF"
            ),
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
                block_io_mode=args.block_io_mode,
                perf_counters=args.perf_counters,
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
            manifest["input_metadata_error"] = (
                "selected input metadata changed during run"
            )
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
        manifest["kernel_elf_provenance"]["runtime_feature_identity_verified"] = (
            final_aggregate["all_required_samples_valid"]
        )
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
        f"block_io={args.block_io_mode} perf={args.perf_counters} "
        f"median_s={final_aggregate['median_elapsed_seconds']} "
        f"goal_met={final_aggregate['goal_met']}",
        flush=True,
    )
    return 0


def marker_identity_text(identity: dict[str, str]) -> str:
    return " ".join(
        f"{key}={identity[key]}"
        for key in (
            "run_id",
            "arch",
            "kind",
            "sample",
            "smp",
            "mem",
            "block_io",
            "perf",
        )
    )


def synthetic_perf_snapshot(identity: dict[str, str], point: str) -> list[str]:
    if point not in {"before", "after"}:
        raise BenchmarkError(f"unsupported synthetic perf point: {point!r}")
    increment = 0 if point == "before" else 5
    lines: list[str] = []
    for ordinal, key in enumerate(PERF_REQUIRED_KEYS, start=1):
        if key in {
            "perf_counters_enabled",
            "kperf_timing_enabled",
            "kperf_syscall_timing_enabled",
        }:
            value = 1
        elif key == "block_io_nonblocking_requested":
            value = 1 if identity["block_io"] == "auto" else 0
        elif point == "after" and (
            (
                identity["block_io"] == "auto"
                and key in {"block_io_sync_read_submits", "block_io_sync_write_submits"}
            )
            or (
                identity["block_io"] == "force-sync"
                and key
                in {
                    "block_io_nb_read_submits",
                    "block_io_nb_write_submits",
                    "block_io_nb_read_waits",
                    "block_io_nb_write_waits",
                    "block_io_nb_read_completions",
                    "block_io_nb_write_completions",
                }
            )
        ):
            value = ordinal
        else:
            value = ordinal + increment
        lines.append(f"{key} {value}")
        if key == "kperf_syscall_timing_enabled":
            lines.extend(
                [
                    f"profile_syscall_17_calls {1 + increment}",
                    f"profile_syscall_17_total_ticks {100 + increment}",
                    f"profile_syscall_17_total_us {10 + increment}",
                    f"profile_syscall_17_avg_ns {1000 + increment}",
                    f"profile_syscall_17_max_us {2 + increment}",
                ]
            )
            if point == "after":
                lines.extend(
                    [
                        "profile_syscall_63_calls 1",
                        "profile_syscall_63_total_ticks 10",
                        "profile_syscall_63_total_us 1",
                        "profile_syscall_63_avg_ns 100",
                        "profile_syscall_63_max_us 1",
                    ]
                )
    return lines


def synthetic_log(
    identity: dict[str, str], architecture: Architecture, smp: int, mem: str
) -> str:
    mask = (1 << smp) - 1
    leader = 1 if smp > 1 else 0
    requested = mask & ~(1 << leader)
    nonblocking = "true" if identity["block_io"] == "auto" else "false"
    perf_counters = "true" if identity["perf"] == "1" else "false"
    identity_text = marker_identity_text(identity)
    lines = [
        (
            "board config: clock_freq=10000000, "
            f"memory_end={expected_memory_end(architecture.name, mem):#x}, "
            "uart=0x0, plic=0x0"
        ),
        (
            f"cpu topology: possible={smp} online=1 possible_mask={mask:#x} "
            "online_mask=0x1 boot_logical=0 boot_hw_id=0 hw_ids=[]"
        ),
        "smp invariants: boot_entries=1 global_init_entries=1",
        f"smp schedulers: active_mask={mask:#x} count={smp}",
        (
            f"[ INFO] KERN: block io policy mode={identity['block_io']} irq_ready=true "
            f"nonblocking={nonblocking} perf_counters={perf_counters}"
        ),
        f"G0_RUST_HELLO_START {identity_text}",
    ]
    if identity["perf"] == "1":
        for point in ("before", "after"):
            lines.append(f"G0_RUST_HELLO_PERF_BEGIN {identity_text} point={point}")
            lines.extend(synthetic_perf_snapshot(identity, point))
            lines.append(f"G0_RUST_HELLO_PERF_END {identity_text} point={point}")
    lines.extend(
        [
            (
                f"G0_RUST_HELLO_RESULT {identity_text} "
                f"uname={architecture.uname} nproc={smp} "
                "cargo_version=1.0.0 rustc_version=1.0.0 "
                "tmp_mount=1 tmp_writable=1 elapsed_ns=900000000 timer_exited=1 "
                "timer_exit_code=0 timer_signaled=0 timer_signal=0 timer_rc=0 "
                "uptime_before=10.00 uptime_after=10.90 lock_created=1 "
                "artifact_bytes=100 output_bytes=14 output_ok=1 ok=1"
            ),
            f"G0_RUST_HELLO_PASS {identity_text}",
            (
                f"smp shutdown: leader={leader} requested={requested:#x} "
                f"stopped={requested:#x} missing=0x0 failure=false"
            ),
        ]
    )
    return "\n".join(lines) + "\n"


def self_test() -> int:
    architecture = ARCHITECTURES["rv"]
    identity = {
        "run_id": "self-test",
        "arch": "rv",
        "kind": "measured",
        "sample": "1",
        "smp": "8",
        "mem": "8G",
        "block_io": "auto",
        "perf": "0",
    }
    render_guest(identity)
    expected_launcher = (
        "#!/musl/busybox sh\nexec /musl/busybox ash /x1/g0-rust-hello.sh || exit 127\n"
    )
    if render_guest_launcher() != expected_launcher:
        raise BenchmarkError("guest ash launcher does not match the frozen handoff")
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
    if (
        errors
        or parsed.get("elapsed_ns") != "900000000"
        or parsed.get("perf_snapshots") is not None
        or parsed.get("block_io_policy", {}).get("mode") != "auto"
    ):
        raise BenchmarkError(f"positive synthetic log failed: {errors}")
    pass_line = next(
        line for line in good.splitlines() if line.startswith("G0_RUST_HELLO_PASS ")
    )
    _, errors = validate_guest_log(
        good + pass_line + "\n",
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
    shell_error = good.replace(
        "G0_RUST_HELLO_START ",
        "ash: waitpid: Interrupted system call\nG0_RUST_HELLO_START ",
        1,
    )
    _, errors = validate_guest_log(
        shell_error, identity=identity, architecture=architecture, smp=8, mem="8G"
    )
    if not any("forbidden guest shell diagnostic" in error for error in errors):
        raise BenchmarkError("forbidden shell diagnostic was accepted")
    bad_clock = good.replace("uptime_after=10.90", "uptime_after=19.90")
    _, errors = validate_guest_log(
        bad_clock, identity=identity, architecture=architecture, smp=8, mem="8G"
    )
    if not any("disagreement" in error for error in errors):
        raise BenchmarkError("timer/uptime mismatch was accepted")

    force_sync_identity = {**identity, "block_io": "force-sync"}
    force_sync = synthetic_log(force_sync_identity, architecture, 8, "8G")
    parsed, errors = validate_guest_log(
        force_sync,
        identity=force_sync_identity,
        architecture=architecture,
        smp=8,
        mem="8G",
    )
    if errors or parsed["block_io_policy"]["nonblocking"]:
        raise BenchmarkError(f"positive force-sync synthetic log failed: {errors}")

    perf_identity = {**identity, "perf": "1"}
    perf_log = synthetic_log(perf_identity, architecture, 8, "8G")
    parsed, errors = validate_guest_log(
        perf_log,
        identity=perf_identity,
        architecture=architecture,
        smp=8,
        mem="8G",
    )
    perf_snapshots = parsed.get("perf_snapshots") or {}
    if (
        errors
        or perf_snapshots.get("selected_deltas", {}).get(
            "block_cache_device_read_submit"
        )
        != 5
        or perf_snapshots.get("after", {}).get("syscall_ids") != [17, 63]
    ):
        raise BenchmarkError(f"positive perf synthetic log failed: {errors}")

    perf_before = perf_snapshots["before"]
    perf_after = perf_snapshots["after"]
    no_auto_nb_read = {
        **perf_after,
        "values": {
            **perf_after["values"],
            "block_io_nb_read_submits": perf_before["values"][
                "block_io_nb_read_submits"
            ],
        },
    }
    _, errors = validate_perf_snapshot_pair(
        perf_before, no_auto_nb_read, block_io_mode="auto"
    )
    if not any(
        "auto mode requires block_io_nb_read_submits" in error for error in errors
    ):
        raise BenchmarkError("auto mode accepted zero nonblocking read submits")

    mismatched_auto_read = {
        **perf_after,
        "values": {
            **perf_after["values"],
            "block_io_nb_read_waits": perf_after["values"]["block_io_nb_read_waits"]
            - 1,
        },
    }
    _, errors = validate_perf_snapshot_pair(
        perf_before, mismatched_auto_read, block_io_mode="auto"
    )
    if not any("auto read nonblocking lifecycle mismatch" in error for error in errors):
        raise BenchmarkError("auto mode accepted mismatched read lifecycle counters")

    force_perf_identity = {**perf_identity, "block_io": "force-sync"}
    force_perf_log = synthetic_log(force_perf_identity, architecture, 8, "8G")
    force_parsed, errors = validate_guest_log(
        force_perf_log,
        identity=force_perf_identity,
        architecture=architecture,
        smp=8,
        mem="8G",
    )
    if errors:
        raise BenchmarkError(f"positive force-sync perf log failed: {errors}")
    force_snapshots = force_parsed["perf_snapshots"]
    force_before = force_snapshots["before"]
    force_after = force_snapshots["after"]

    force_with_nb_wait = {
        **force_after,
        "values": {
            **force_after["values"],
            "block_io_nb_read_waits": force_before["values"]["block_io_nb_read_waits"]
            + 1,
        },
    }
    _, errors = validate_perf_snapshot_pair(
        force_before, force_with_nb_wait, block_io_mode="force-sync"
    )
    if not any("requires zero nonblocking lifecycle" in error for error in errors):
        raise BenchmarkError("force-sync mode accepted a nonblocking lifecycle delta")

    force_without_sync_read = {
        **force_after,
        "values": {
            **force_after["values"],
            "block_io_sync_read_submits": force_before["values"][
                "block_io_sync_read_submits"
            ],
        },
    }
    _, errors = validate_perf_snapshot_pair(
        force_before, force_without_sync_read, block_io_mode="force-sync"
    )
    if not any("requires block_io_sync_read_submits" in error for error in errors):
        raise BenchmarkError("force-sync mode accepted zero synchronous read submits")

    bad_policy = good.replace("mode=auto", "mode=force-sync", 1)
    _, errors = validate_guest_log(
        bad_policy,
        identity=identity,
        architecture=architecture,
        smp=8,
        mem="8G",
    )
    if not any("policy block_io mismatch" in error for error in errors):
        raise BenchmarkError("mismatched block IO policy was accepted")

    duplicate_policy_line = next(
        line for line in good.splitlines() if "KERN: block io policy " in line
    )
    duplicate_policy = good + duplicate_policy_line + "\n"
    _, errors = validate_guest_log(
        duplicate_policy,
        identity=identity,
        architecture=architecture,
        smp=8,
        mem="8G",
    )
    if not any("policy marker, found 2" in error for error in errors):
        raise BenchmarkError("duplicate block IO policy was accepted")

    perf0_snapshot_marker = (
        f"G0_RUST_HELLO_PERF_BEGIN {marker_identity_text(identity)} point=before\n"
    )
    perf0_with_snapshot = good.replace(
        "G0_RUST_HELLO_RESULT ", perf0_snapshot_marker + "G0_RUST_HELLO_RESULT ", 1
    )
    _, errors = validate_guest_log(
        perf0_with_snapshot,
        identity=identity,
        architecture=architecture,
        smp=8,
        mem="8G",
    )
    if not any("perf=0 guest emitted" in error for error in errors):
        raise BenchmarkError("perf=0 snapshot marker was accepted")

    malformed_snapshot = perf_log.replace(
        "scheduler_fetch_calls 2", "scheduler_fetch_calls not-an-integer", 1
    )
    _, errors = validate_guest_log(
        malformed_snapshot,
        identity=perf_identity,
        architecture=architecture,
        smp=8,
        mem="8G",
    )
    if not any("malformed line" in error for error in errors):
        raise BenchmarkError("malformed perf counter was accepted")

    wrong_perf_policy = perf_log.replace("perf_counters=true", "perf_counters=false", 1)
    _, errors = validate_guest_log(
        wrong_perf_policy,
        identity=perf_identity,
        architecture=architecture,
        smp=8,
        mem="8G",
    )
    if not any("policy perf_counters mismatch" in error for error in errors):
        raise BenchmarkError("mismatched perf policy was accepted")
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
    print("IO0-A synthetic self-test PASS")
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
    parser.add_argument(
        "--block-io-mode", choices=("auto", "force-sync"), default="auto"
    )
    parser.add_argument("--perf-counters", type=int, choices=(0, 1), default=0)
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
