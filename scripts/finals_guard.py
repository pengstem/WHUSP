#!/usr/bin/env python3
"""Offline preflight, runner, and log checker for the 2026 final round.

This tool intentionally uses only the Python standard library.  It is meant to
remain useful when the contest machine has no network access and AI assistance
is unavailable.
"""

from __future__ import annotations

import argparse
import collections
import dataclasses
import datetime as dt
import json
import os
import re
import selectors
import shutil
import signal
import subprocess
import sys
import time
from collections.abc import Iterable
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
FINAL_SMP = 8
MAX_CPUS = 12
FINAL_MEM = "8G"
MIN_IMAGE_BYTES = 8 * 1024**3
MIN_HOST_FREE_BYTES = 20 * 1024**3
WARN_HOST_FREE_BYTES = 40 * 1024**3
MIN_AVAILABLE_MEMORY_BYTES = 10 * 1024**3
WARN_AVAILABLE_MEMORY_BYTES = 12 * 1024**3

ARCHES = ("rv", "la")
IMAGE_PATHS = {
    "rv": REPO_ROOT / "sdcard-rv-pub.img",
    "la": REPO_ROOT / "sdcard-la-pub.img",
}
KERNEL_PATHS = {
    "rv": REPO_ROOT / "kernel-rv",
    "la": REPO_ROOT / "kernel-la",
}
ELF_MACHINES = {"rv": "RISC-V", "la": "LoongArch"}
GUEST_ARCHES = {"rv": "riscv64", "la": "loongarch64"}

# Values published by the contestant-facing final-2026 judge on 2026-08-10.
# The platform judge may use different values, so these are self-check estimates.
REFERENCE_BASELINES = {"riscv64": 1616.09, "loongarch64": 1985.21}
OFFICIAL_SNAPSHOT_COMMIT = "b5ec6ef8497e1818cbdec3b54bb722f036e57972"

CAGENT_TESTS = collections.OrderedDict(
    [
        ("factorial", ("easy", 13.5, 20_000)),
        ("date", ("easy", 13.5, 20_000)),
        ("network", ("medium", 20.0, 25_000)),
        ("cpu", ("easy", 13.5, 20_000)),
        ("kernel", ("easy", 13.5, 20_000)),
        ("fs-create", ("medium", 20.0, 25_000)),
        ("fs-readwrite", ("medium", 20.0, 30_000)),
        ("fs-directory", ("medium", 20.0, 30_000)),
        ("fs-search", ("hard", 27.0, 35_000)),
        ("fs-usage", ("medium", 20.0, 25_000)),
    ]
)

CAGENT_RECORD_RE = re.compile(
    r"testcase\s+cagent\s+(\S+)\s+(pass|reject)\s+(\d+)"
)
BUILDSTORM_COMPILE_RE = re.compile(r"^BUILDSTORM_COMPILE\s+(.+)$", re.MULTILINE)
MAKE_ASSIGN_RE = re.compile(r"^(?P<name>[A-Z0-9_]+)\s*\?=\s*(?P<value>\S+)", re.MULTILINE)


@dataclasses.dataclass
class Check:
    level: str
    name: str
    detail: str
    remedy: str = ""

    def as_dict(self) -> dict[str, str]:
        return dataclasses.asdict(self)


def command_output(command: list[str], timeout: float = 10.0) -> tuple[int, str]:
    try:
        proc = subprocess.run(
            command,
            cwd=REPO_ROOT,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            errors="replace",
            timeout=timeout,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        return 127, str(error)
    return proc.returncode, proc.stdout.strip()


def human_bytes(size: int) -> str:
    value = float(size)
    for suffix in ("B", "KiB", "MiB", "GiB", "TiB"):
        if value < 1024 or suffix == "TiB":
            return f"{value:.1f} {suffix}"
        value /= 1024
    raise AssertionError("unreachable")


def read_make_defaults(path: Path) -> dict[str, str]:
    text = path.read_text(encoding="utf-8", errors="replace")
    return {
        match.group("name"): match.group("value")
        for match in MAKE_ASSIGN_RE.finditer(text)
    }


def physical_core_count() -> int:
    cores: set[tuple[str, str]] = set()
    for cpu_path in Path("/sys/devices/system/cpu").glob("cpu[0-9]*"):
        topology = cpu_path / "topology"
        try:
            package_id = (topology / "physical_package_id").read_text().strip()
            core_id = (topology / "core_id").read_text().strip()
        except OSError:
            continue
        cores.add((package_id, core_id))
    return len(cores) if cores else (os.cpu_count() or 0)


def mem_available_bytes() -> int:
    try:
        text = Path("/proc/meminfo").read_text(encoding="ascii")
    except OSError:
        return 0
    match = re.search(r"^MemAvailable:\s+(\d+)\s+kB$", text, re.MULTILINE)
    return int(match.group(1)) * 1024 if match else 0


def ext4_magic_ok(path: Path) -> bool:
    try:
        with path.open("rb") as image:
            image.seek(1024 + 56)
            return image.read(2) == b"\x53\xef"
    except OSError:
        return False


def running_qemu_processes() -> list[str]:
    processes = []
    proc_root = Path("/proc")
    for child in proc_root.iterdir():
        if not child.name.isdigit() or int(child.name) == os.getpid():
            continue
        try:
            command = (child / "cmdline").read_bytes().replace(b"\0", b" ").decode(
                errors="replace"
            )
        except OSError:
            continue
        if "qemu-system-riscv64" in command or "qemu-system-loongarch64" in command:
            processes.append(f"pid={child.name} {command.strip()}")
    return processes


def check_tool(name: str) -> Check:
    path = shutil.which(name)
    if path is None:
        return Check("FAIL", f"tool:{name}", "not found in PATH", f"install {name} before going offline")
    return Check("PASS", f"tool:{name}", path)


def check_kernel(arch: str, required: bool) -> Check:
    path = KERNEL_PATHS[arch]
    if not path.is_file():
        level = "FAIL" if required else "WARN"
        return Check(level, f"kernel:{arch}", f"missing {path.name}", f"run make kernel-{arch}")
    rc, header = command_output(["readelf", "-h", str(path)])
    expected = ELF_MACHINES[arch]
    if rc != 0 or expected not in header:
        return Check(
            "FAIL",
            f"kernel:{arch}",
            f"{path.name} is not an expected {expected} ELF",
            f"rebuild with make kernel-{arch}",
        )
    return Check("PASS", f"kernel:{arch}", f"{path.name}, {human_bytes(path.stat().st_size)}, machine={expected}")


def collect_preflight(require_kernels: bool, strict: bool) -> list[Check]:
    checks: list[Check] = []

    for tool in (
        "python3",
        "make",
        "cargo",
        "rustc",
        "qemu-system-riscv64",
        "qemu-system-loongarch64",
        "qemu-img",
        "readelf",
        "mkfs.ext4",
        "flock",
        "timeout",
    ):
        checks.append(check_tool(tool))

    root_defaults = read_make_defaults(REPO_ROOT / "Makefile")
    os_defaults = read_make_defaults(REPO_ROOT / "os" / "Makefile")
    expected_defaults = {
        "MEM_RV": FINAL_MEM,
        "MEM_LA": FINAL_MEM,
        "SMP": str(FINAL_SMP),
        "PERF_COUNTERS": "0",
        "BLOCK_IO_MODE": "force-sync",
        "RUN_CAGENT": "1",
        "RUN_BUILDSTORM": "1",
    }
    mismatches = [
        f"{name}={root_defaults.get(name, '<missing>')} (want {value})"
        for name, value in expected_defaults.items()
        if root_defaults.get(name) != value
    ]
    if os_defaults.get("MEM") != FINAL_MEM:
        mismatches.append(f"os/Makefile MEM={os_defaults.get('MEM', '<missing>')} (want {FINAL_MEM})")
    if mismatches:
        checks.append(
            Check(
                "FAIL",
                "final-profile",
                "; ".join(mismatches),
                "restore the published 8C/8G, PERF=0, force-sync defaults",
            )
        )
    else:
        checks.append(
            Check(
                "PASS",
                "final-profile",
                "smp=8 mem=8G perf=0 block_io=force-sync cagent=1 buildstorm=1",
            )
        )

    cores = physical_core_count()
    if cores < FINAL_SMP:
        checks.append(
            Check(
                "FAIL",
                "host-physical-cores",
                f"detected {cores}, need at least {FINAL_SMP}",
                "move performance validation to a host with at least 8 physical cores",
            )
        )
    else:
        checks.append(Check("PASS", "host-physical-cores", f"detected {cores}"))

    available = mem_available_bytes()
    if available and available < MIN_AVAILABLE_MEMORY_BYTES:
        checks.append(
            Check(
                "FAIL",
                "host-memory",
                f"MemAvailable={human_bytes(available)}",
                "stop unrelated workloads; an 8G guest needs host overhead",
            )
        )
    elif available and available < WARN_AVAILABLE_MEMORY_BYTES:
        checks.append(
            Check(
                "WARN",
                "host-memory",
                f"MemAvailable={human_bytes(available)}; little QEMU headroom",
                "stop unrelated workloads before a timed run",
            )
        )
    else:
        checks.append(Check("PASS", "host-memory", f"MemAvailable={human_bytes(available)}"))

    free = shutil.disk_usage(REPO_ROOT).free
    if free < MIN_HOST_FREE_BYTES:
        checks.append(
            Check(
                "FAIL",
                "host-disk",
                f"free={human_bytes(free)}",
                "free at least 20 GiB for qcow2 build writes and logs",
            )
        )
    elif free < WARN_HOST_FREE_BYTES:
        checks.append(
            Check(
                "WARN",
                "host-disk",
                f"free={human_bytes(free)}; BuildStorm overlay headroom is narrow",
                "prefer at least 40 GiB free",
            )
        )
    else:
        checks.append(Check("PASS", "host-disk", f"free={human_bytes(free)}"))

    for arch in ARCHES:
        image = IMAGE_PATHS[arch]
        if not image.is_file():
            checks.append(
                Check(
                    "FAIL",
                    f"image:{arch}",
                    f"missing {image.name}",
                    "restore the final-round public image from the offline backup",
                )
            )
            continue
        size = image.stat().st_size
        if size < MIN_IMAGE_BYTES or not ext4_magic_ok(image):
            checks.append(
                Check(
                    "FAIL",
                    f"image:{arch}",
                    f"{image.name}, size={human_bytes(size)}, ext4_magic={ext4_magic_ok(image)}",
                    "do not boot it; restore a verified image copy",
                )
            )
        else:
            checks.append(Check("PASS", f"image:{arch}", f"{image.name}, {human_bytes(size)}, ext4"))
        checks.append(check_kernel(arch, require_kernels))

    runner = REPO_ROOT / "scripts" / "export_contest_case_scripts.py"
    runner_text = runner.read_text(encoding="utf-8", errors="replace")
    runner_needles = (
        "/glibc/cagent_testcode.sh",
        "/glibc/buildstorm_testcode.sh",
        'echo "FINAL: all enabled tests finished (status=$overall_status)"',
    )
    missing_needles = [needle for needle in runner_needles if needle not in runner_text]
    if missing_needles:
        checks.append(
            Check(
                "FAIL",
                "guest-runner",
                f"missing contracts: {missing_needles}",
                "repair the generated final runner before booting",
            )
        )
    else:
        checks.append(Check("PASS", "guest-runner", "CAgent + BuildStorm paths and final status marker present"))

    offline_files = (
        REPO_ROOT / "vendor" / "config.toml",
        REPO_ROOT / "vendor" / "crates",
        REPO_ROOT / "rust-toolchain.toml",
        REPO_ROOT / "tools" / "loongarch64-linux-musl-cross" / "bin" / "loongarch64-linux-musl-gcc",
    )
    missing_offline = [str(path.relative_to(REPO_ROOT)) for path in offline_files if not path.exists()]
    if missing_offline:
        checks.append(
            Check(
                "FAIL",
                "offline-build-assets",
                f"missing: {', '.join(missing_offline)}",
                "restore the vendored crates/toolchain before network access is removed",
            )
        )
    else:
        checks.append(Check("PASS", "offline-build-assets", "vendored crates, Rust pin, and LA cross compiler present"))

    official_reference = REPO_ROOT / "tools" / "final-2026-reference"
    if not official_reference.is_dir():
        checks.append(
            Check(
                "WARN",
                "official-reference",
                "offline final-2026 source snapshot is absent",
                "before going offline, clone the official final-2026 branch into tools/final-2026-reference",
            )
        )
    else:
        rc, reference_head = command_output(
            ["git", "-C", str(official_reference), "rev-parse", "HEAD"]
        )
        required_reference_files = (
            official_reference / "README.md",
            official_reference / "scripts" / "cagent_testcode.sh",
            official_reference / "scripts" / "buildstorm_testcode.sh",
            official_reference / "judge" / "judge_cagent-glibc.py",
            official_reference / "judge" / "judge_buildstorm-glibc.py",
        )
        missing_reference_files = [
            str(path.relative_to(official_reference))
            for path in required_reference_files
            if not path.is_file()
        ]
        if rc != 0 or missing_reference_files:
            checks.append(
                Check(
                    "FAIL",
                    "official-reference",
                    f"head={reference_head or 'unknown'} missing={missing_reference_files}",
                    "restore the complete frozen final-2026 source snapshot",
                )
            )
        else:
            drift = ""
            if reference_head != OFFICIAL_SNAPSHOT_COMMIT:
                drift = f" (different from documented {OFFICIAL_SNAPSHOT_COMMIT[:12]})"
            checks.append(
                Check(
                    "PASS",
                    "official-reference",
                    f"head={reference_head}{drift}; scripts and judges present",
                )
            )

    rc, version = command_output(["qemu-system-riscv64", "--version"])
    first_line = version.splitlines()[0] if version else "unknown"
    checks.append(Check("PASS" if rc == 0 else "FAIL", "qemu-version", first_line))

    qemu = running_qemu_processes()
    if qemu:
        checks.append(
            Check(
                "WARN",
                "running-qemu",
                " | ".join(qemu),
                "identify it before a timed run; never kill an unknown process blindly",
            )
        )
    else:
        checks.append(Check("PASS", "running-qemu", "none"))

    overlays = sorted(Path("/tmp").glob("whusp-qemu.*"))
    if overlays:
        checks.append(
            Check(
                "WARN",
                "stale-overlays",
                ", ".join(str(path) for path in overlays),
                "confirm no QEMU owns them, then remove only the confirmed stale directories",
            )
        )
    else:
        checks.append(Check("PASS", "stale-overlays", "none"))

    rc, head = command_output(["git", "rev-parse", "--short=12", "HEAD"])
    checks.append(Check("PASS" if rc == 0 else "FAIL", "git-head", head or "unknown"))
    rc, status = command_output(["git", "status", "--short"])
    if rc != 0:
        checks.append(Check("FAIL", "git-status", status))
    elif status:
        checks.append(
            Check(
                "FAIL" if strict else "WARN",
                "git-status",
                f"working tree has {len(status.splitlines())} changed/untracked paths",
                "record and review the exact diff; do not reset it blindly",
            )
        )
    else:
        checks.append(Check("PASS", "git-status", "clean"))

    return checks


def print_checks(checks: Iterable[Check]) -> None:
    checks = list(checks)
    width = max((len(check.name) for check in checks), default=0)
    for check in checks:
        print(f"[{check.level:<4}] {check.name:<{width}}  {check.detail}")
        if check.remedy:
            print(f"       {'':<{width}}  action: {check.remedy}")
    totals = collections.Counter(check.level for check in checks)
    print(
        "PREFLIGHT_SUMMARY "
        f"pass={totals['PASS']} warn={totals['WARN']} fail={totals['FAIL']}"
    )


def preflight_exit_code(checks: Iterable[Check]) -> int:
    return 2 if any(check.level == "FAIL" for check in checks) else 0


def parse_key_values(text: str) -> dict[str, str]:
    values: dict[str, str] = {}
    for field in text.split():
        if "=" not in field:
            continue
        key, value = field.split("=", 1)
        values[key] = value
    return values


def parse_cagent(text: str) -> dict:
    occurrences: dict[str, list[dict[str, int | str]]] = collections.defaultdict(list)
    unknown = []
    for match in CAGENT_RECORD_RE.finditer(text):
        name, status, elapsed_text = match.groups()
        record = {"status": status, "elapsed_ms": int(elapsed_text)}
        if name in CAGENT_TESTS:
            occurrences[name].append(record)
        else:
            unknown.append(name)

    cases = []
    issues = []
    total_score = 0.0
    for name, (difficulty, weight, timeout_ms) in CAGENT_TESTS.items():
        records = occurrences.get(name, [])
        if not records:
            issues.append(f"missing CAgent case: {name}")
            cases.append(
                {
                    "name": name,
                    "difficulty": difficulty,
                    "status": "missing",
                    "elapsed_ms": None,
                    "score": 0.0,
                    "bonus": False,
                }
            )
            continue
        if len(records) > 1:
            issues.append(f"duplicate CAgent case: {name} ({len(records)} records; judge uses last)")
        record = records[-1]
        elapsed_ms = int(record["elapsed_ms"])
        passed = record["status"] == "pass"
        bonus = passed and 0 < elapsed_ms < timeout_ms / 2
        score = round(weight * (1.1 if bonus else 1.0), 2) if passed else 0.0
        total_score += score
        if not passed:
            issues.append(f"rejected CAgent case: {name} ({elapsed_ms} ms)")
        elif elapsed_ms >= timeout_ms:
            issues.append(f"CAgent duration is at/over timeout: {name} ({elapsed_ms}/{timeout_ms} ms)")
        cases.append(
            {
                "name": name,
                "difficulty": difficulty,
                "status": record["status"],
                "elapsed_ms": elapsed_ms,
                "timeout_ms": timeout_ms,
                "score": score,
                "bonus": bonus,
            }
        )

    # The current public images use cagent-glibc while the latest published
    # final-2026 source snapshot uses cagent.  Both carry the same case records.
    start_count = len(
        re.findall(r"#### OS COMP TEST GROUP START cagent(?:-glibc)? ####", text)
    )
    end_count = len(
        re.findall(r"#### OS COMP TEST GROUP END cagent(?:-glibc)? ####", text)
    )
    if start_count != 1 or end_count != 1:
        issues.append(f"CAgent group markers start={start_count} end={end_count}, expected 1/1")
    if unknown:
        issues.append(f"unknown CAgent records: {sorted(set(unknown))}")

    return {
        "present": bool(occurrences) or start_count > 0,
        "valid": not issues,
        "score": round(total_score, 2),
        "max_score_from_published_formula": 199.1,
        "cases": cases,
        "issues": issues,
        "group_markers": {"start": start_count, "end": end_count},
    }


def buildstorm_time_score(elapsed: float, baseline: float) -> float:
    return round(120.0 * max(0.0, min(1.0, (2 * baseline - elapsed) / baseline)), 1)


def parse_buildstorm(
    text: str,
    arch_hint: str | None,
    baseline: float | None,
    expected_cores: int = FINAL_SMP,
) -> dict:
    toolchain = bool(
        re.search(r"^BUILDSTORM_TOOLCHAIN\s+ok\s*$", text, re.MULTILINE)
        or re.search(r"^TOOLCHAIN_RESULT\s+status=OK\s*$", text, re.MULTILINE)
    )
    minibuild = bool(
        re.search(r"^BUILDSTORM_MINIBUILD\s+ok\s*$", text, re.MULTILINE)
        or re.search(r"^MINIBUILD_RESULT\s+status=OK\s*$", text, re.MULTILINE)
    )
    compile_lines = BUILDSTORM_COMPILE_RE.findall(text)
    compile_values = parse_key_values(compile_lines[-1]) if compile_lines else {}
    issues = []

    present = toolchain or minibuild or bool(compile_lines) or "BUILDSTORM_BEGIN" in text
    if present and not toolchain:
        issues.append("missing or failed BUILDSTORM_TOOLCHAIN gate")
    if present and not minibuild:
        issues.append("missing or failed BUILDSTORM_MINIBUILD gate")
    if present and not compile_lines:
        issues.append("missing BUILDSTORM_COMPILE result")
    if len(compile_lines) > 1:
        issues.append(f"multiple BUILDSTORM_COMPILE records ({len(compile_lines)}; judge uses last)")

    compile_ok = compile_values.get("ok") == "true"
    if compile_lines and not compile_ok:
        issues.append(f"BuildStorm compile failed: {compile_values}")

    def parse_number(name: str, value_type: type[int | float]) -> int | float | None:
        value = compile_values.get(name)
        if value is None:
            return None
        try:
            return value_type(value)
        except ValueError:
            issues.append(f"invalid BuildStorm {name}: {value}")
            return None

    elapsed = parse_number("elapsed_s", float)
    cores = parse_number("cores", int)
    byte_count = parse_number("bytes", int)
    guest_arch = compile_values.get("arch")
    expected_arch = GUEST_ARCHES.get(arch_hint) if arch_hint else None
    if compile_ok and (elapsed is None or elapsed <= 0):
        issues.append(f"invalid BuildStorm elapsed_s: {elapsed}")
    if compile_ok and byte_count is not None and byte_count < 500_000:
        issues.append(f"BuildStorm artifact too small: {byte_count} bytes")
    if compile_ok and byte_count is None:
        issues.append("missing BuildStorm bytes field")
    if compile_lines and cores != expected_cores:
        issues.append(f"BuildStorm cores={cores}, expected {expected_cores}")
    if compile_lines and expected_arch and guest_arch != expected_arch:
        issues.append(f"BuildStorm arch={guest_arch}, expected {expected_arch}")

    selected_baseline = baseline
    if selected_baseline is None and guest_arch in REFERENCE_BASELINES:
        selected_baseline = REFERENCE_BASELINES[guest_arch]
    time_score = 0.0
    if compile_ok and isinstance(elapsed, float) and selected_baseline:
        time_score = buildstorm_time_score(elapsed, selected_baseline)
    script_score = (8.0 if toolchain else 0.0) + (12.0 if minibuild else 0.0)
    if compile_ok:
        script_score += 40.0 + time_score

    start_count = len(
        re.findall(r"#### OS COMP TEST GROUP START buildstorm(?:-glibc)? ####", text)
    )
    end_count = len(
        re.findall(r"#### OS COMP TEST GROUP END buildstorm(?:-glibc)? ####", text)
    )
    if present and (start_count != 1 or end_count != 1):
        issues.append(f"BuildStorm group markers start={start_count} end={end_count}, expected 1/1")

    return {
        "present": present,
        "valid": present and not issues,
        "toolchain_ok": toolchain,
        "minibuild_ok": minibuild,
        "compile": compile_values,
        "compile_ok": compile_ok,
        "elapsed_s": elapsed,
        "cores": cores,
        "expected_cores": expected_cores,
        "bytes": byte_count,
        "arch": guest_arch,
        "reference_baseline_s": selected_baseline,
        "reference_script_score": round(script_score, 1),
        "reference_script_max": 180.0,
        "score_warning": "self-check only; platform baselines and final score may differ",
        "issues": issues,
        "group_markers": {"start": start_count, "end": end_count},
    }


def diagnose_log(text: str, cagent: dict, buildstorm: dict) -> list[str]:
    diagnoses = []
    patterns = (
        (r"Bad address|\bEFAULT\b", "EFAULT/user-copy: inspect the first failing address, PTE, access direction, and task address space"),
        (r"Out of memory|Cannot allocate memory|\bENOMEM\b", "memory pressure/leak: inspect frame, heap, process, and kernel-stack accounting before increasing RAM"),
        (r"Input/output error|VirtIO.*error|\bEIO\b", "block I/O: inspect the first failed request, completion path, device index, and overlay health"),
        (r"error while loading shared libraries|not found.*ld-linux|No such file or directory.*loader", "dynamic loader/rootfs: verify /glibc, architecture loader links, and x0/x1 mount order"),
        (r"Address already in use", "CAgent server port conflict: confirm an old simple_llm_server process is not retained"),
        (r"panicked at|kernel panic|KERNEL PANIC", "kernel panic: trust the first panic file/line plus trap cause and bad address, not the last screen"),
        (r"QEMU: Terminated|Terminated", "external termination/timeout: the run is incomplete even if earlier gates passed"),
    )
    for pattern, message in patterns:
        if re.search(pattern, text, re.IGNORECASE):
            diagnoses.append(message)

    if not text.strip():
        diagnoses.append("empty log: verify QEMU command, ELF architecture, firmware, UART, and log redirection")
    elif "boot hart_id=" not in text and not cagent["present"] and not buildstorm["present"]:
        diagnoses.append("did not reach Rust boot marker: separate QEMU/firmware/ELF/UART failure from a kernel init failure")
    elif not cagent["present"] and not buildstorm["present"]:
        diagnoses.append("kernel booted but no final group started: inspect block discovery, root mount, /x1 mount, and init runner exec")
    elif "FINAL: all enabled tests finished (status=0)" not in text:
        diagnoses.append("final success marker is absent or nonzero: use the last group/case marker as the failure boundary")
    return diagnoses


def infer_arch(path: Path, text: str) -> str | None:
    name = path.name.lower()
    if re.search(r"(?:^|[-_.])rv(?:[-_.]|$)", name) or "arch=riscv64" in text:
        return "rv"
    if re.search(r"(?:^|[-_.])la(?:[-_.]|$)", name) or "arch=loongarch64" in text:
        return "la"
    return None


def parse_log(
    path: Path,
    arch: str | None,
    baseline: float | None,
    expected_cores: int = FINAL_SMP,
) -> dict:
    text = path.read_text(encoding="utf-8", errors="replace")
    arch_hint = arch or infer_arch(path, text)
    cagent = parse_cagent(text)
    buildstorm = parse_buildstorm(text, arch_hint, baseline, expected_cores)
    final_statuses = re.findall(r"FINAL: all enabled tests finished \(status=(\d+)\)", text)
    runner_ok = bool(final_statuses) and final_statuses[-1] == "0"
    requested_results = [result for result in (cagent, buildstorm) if result["present"]]
    valid = bool(requested_results) and all(result["valid"] for result in requested_results)
    if final_statuses and not runner_ok:
        valid = False
    return {
        "path": str(path),
        "arch": arch_hint,
        "valid": valid,
        "runner_final_status": int(final_statuses[-1]) if final_statuses else None,
        "cagent": cagent,
        "buildstorm": buildstorm,
        "diagnoses": diagnose_log(text, cagent, buildstorm),
    }


def print_log_report(report: dict) -> None:
    print(f"LOG {report['path']} arch={report['arch'] or 'unknown'} valid={str(report['valid']).lower()}")
    cagent = report["cagent"]
    if cagent["present"]:
        print(
            f"  CAGENT valid={str(cagent['valid']).lower()} "
            f"score={cagent['score']}/{cagent['max_score_from_published_formula']}"
        )
        for case in cagent["cases"]:
            elapsed = "-" if case["elapsed_ms"] is None else f"{case['elapsed_ms']}ms"
            bonus = "+bonus" if case["bonus"] else ""
            print(
                f"    {case['name']:<14} {case['status']:<7} "
                f"{elapsed:>9} score={case['score']}{bonus}"
            )
        for issue in cagent["issues"]:
            print(f"    ISSUE: {issue}")
    buildstorm = report["buildstorm"]
    if buildstorm["present"]:
        print(
            f"  BUILDSTORM valid={str(buildstorm['valid']).lower()} "
            f"toolchain={buildstorm['toolchain_ok']} minibuild={buildstorm['minibuild_ok']} "
            f"compile={buildstorm['compile_ok']} elapsed={buildstorm['elapsed_s']}s "
            f"cores={buildstorm['cores']} bytes={buildstorm['bytes']} arch={buildstorm['arch']}"
        )
        print(
            f"    reference_score={buildstorm['reference_script_score']}/"
            f"{buildstorm['reference_script_max']} baseline={buildstorm['reference_baseline_s']}s "
            "(self-check only)"
        )
        for issue in buildstorm["issues"]:
            print(f"    ISSUE: {issue}")
    if not cagent["present"] and not buildstorm["present"]:
        print("  ISSUE: no CAgent or BuildStorm result records found")
    for diagnosis in report["diagnoses"]:
        print(f"  NEXT: {diagnosis}")
    print(f"FINAL_LOG_CHECK valid={str(report['valid']).lower()}")


def terminate_process_group(proc: subprocess.Popen[str]) -> None:
    if proc.poll() is not None:
        return
    try:
        os.killpg(proc.pid, signal.SIGTERM)
        proc.wait(timeout=5)
    except (ProcessLookupError, subprocess.TimeoutExpired):
        if proc.poll() is None:
            try:
                os.killpg(proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            proc.wait(timeout=5)


def run_and_log(command: list[str], log_path: Path, timeout_seconds: int) -> tuple[int, bool]:
    print(f"RUN command={' '.join(command)}")
    print(f"RUN log={log_path}")
    environment = os.environ.copy()
    environment["CARGO_NET_OFFLINE"] = "true"
    start = time.monotonic()
    timed_out = False
    with log_path.open("w", encoding="utf-8") as log_file:
        proc = subprocess.Popen(
            command,
            cwd=REPO_ROOT,
            env=environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            errors="replace",
            bufsize=1,
            start_new_session=True,
        )
        assert proc.stdout is not None
        selector = selectors.DefaultSelector()
        selector.register(proc.stdout, selectors.EVENT_READ)
        try:
            while proc.poll() is None:
                if time.monotonic() - start > timeout_seconds:
                    timed_out = True
                    print(f"RUN timeout after {timeout_seconds}s; terminating process group")
                    terminate_process_group(proc)
                    break
                for key, _ in selector.select(timeout=0.5):
                    line = key.fileobj.readline()
                    if not line:
                        continue
                    sys.stdout.write(line)
                    sys.stdout.flush()
                    log_file.write(line)
                    log_file.flush()
            for line in proc.stdout:
                sys.stdout.write(line)
                log_file.write(line)
        except KeyboardInterrupt:
            print("RUN interrupted; terminating only this run's process group")
            terminate_process_group(proc)
            raise
        finally:
            selector.close()
    return (124 if timed_out else proc.wait()), timed_out


def default_timeout(suite: str) -> int:
    return 900 if suite == "cagent" else 18_000


def run_final(args: argparse.Namespace) -> int:
    checks = collect_preflight(require_kernels=args.no_build, strict=False)
    print_checks(checks)
    if preflight_exit_code(checks) != 0:
        print("RUN refused because preflight has failures; use the printed action lines")
        return 2

    arches = list(ARCHES) if args.arch == "all" else [args.arch]
    timeout_seconds = args.timeout or default_timeout(args.suite)
    run_stamp = dt.datetime.now().astimezone().strftime("%Y%m%d-%H%M%S")
    output_dir = (args.output_dir or REPO_ROOT / "tools" / "finals_runs" / run_stamp).resolve()
    if output_dir.exists():
        print(f"RUN refused because output directory already exists: {output_dir}")
        return 2
    output_dir.mkdir(parents=True)

    run_cagent = "1" if args.suite in ("cagent", "all") else "0"
    run_buildstorm = "1" if args.suite in ("buildstorm", "all") else "0"
    reports = []
    commands = []
    overall_ok = True
    started_at = dt.datetime.now().astimezone().isoformat()

    for arch in arches:
        command = [
            "make",
            "--no-print-directory",
            f"run-{arch}",
            f"MEM={FINAL_MEM}",
            f"SMP={FINAL_SMP}",
            "PERF_COUNTERS=0",
            "BLOCK_IO_MODE=force-sync",
            f"RUN_CAGENT={run_cagent}",
            f"RUN_BUILDSTORM={run_buildstorm}",
        ]
        if args.no_build:
            command.append("NO_BUILD=1")
        commands.append(command)
        log_path = output_dir / f"{arch}-{args.suite}.log"
        try:
            return_code, timed_out = run_and_log(command, log_path, timeout_seconds)
        except KeyboardInterrupt:
            print(f"RUN stopped; preserved partial log at {log_path}")
            return 130
        report = parse_log(log_path, arch, args.baseline)
        report["command"] = command
        report["return_code"] = return_code
        report["timed_out"] = timed_out
        reports.append(report)
        print_log_report(report)
        if return_code != 0 or not report["valid"]:
            overall_ok = False
            if not args.keep_going:
                break

    rc, head = command_output(["git", "rev-parse", "HEAD"])
    rc_status, status = command_output(["git", "status", "--short"])
    manifest = {
        "started_at": started_at,
        "finished_at": dt.datetime.now().astimezone().isoformat(),
        "git_head": head if rc == 0 else None,
        "git_status": status if rc_status == 0 else "unavailable",
        "profile": {
            "smp": FINAL_SMP,
            "memory": FINAL_MEM,
            "perf_counters": 0,
            "block_io_mode": "force-sync",
            "suite": args.suite,
            "cargo_net_offline": True,
        },
        "commands": commands,
        "reports": reports,
        "valid": overall_ok,
    }
    manifest_path = output_dir / "manifest.json"
    manifest_path.write_text(json.dumps(manifest, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print(f"RUN evidence={output_dir}")
    print(f"FINAL_RUN valid={str(overall_ok).lower()}")
    return 0 if overall_ok else 1


def add_common_log_options(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--arch", choices=("rv", "la"), default=None, help="expected architecture")
    parser.add_argument(
        "--baseline",
        type=float,
        default=None,
        help="BuildStorm Linux baseline in seconds; otherwise use the public self-check value",
    )
    parser.add_argument(
        "--expected-cores",
        type=int,
        choices=range(1, MAX_CPUS + 1),
        default=FINAL_SMP,
        metavar=f"1..{MAX_CPUS}",
        help=f"expected guest core count (default: final profile {FINAL_SMP})",
    )


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    preflight = subparsers.add_parser("preflight", help="check the offline final environment")
    preflight.add_argument("--require-kernels", action="store_true", help="fail if root kernel artifacts are absent")
    preflight.add_argument("--strict", action="store_true", help="treat a dirty worktree as a failure")
    preflight.add_argument("--json", type=Path, default=None, help="also write machine-readable results")

    check_log = subparsers.add_parser("check-log", help="check saved serial logs and print next actions")
    check_log.add_argument("logs", nargs="+", type=Path)
    add_common_log_options(check_log)
    check_log.add_argument("--json", type=Path, default=None, help="also write machine-readable results")

    run = subparsers.add_parser("run", help="run final suites serially with the official profile")
    run.add_argument("--arch", choices=("rv", "la", "all"), default="all")
    run.add_argument("--suite", choices=("cagent", "buildstorm", "all"), default="cagent")
    run.add_argument("--no-build", action="store_true", help="reuse root kernel artifacts")
    run.add_argument("--keep-going", action="store_true", help="continue to the other architecture after a failure")
    run.add_argument("--timeout", type=int, default=None, help="per-architecture host timeout in seconds")
    run.add_argument("--output-dir", type=Path, default=None)
    run.add_argument("--baseline", type=float, default=None, help="BuildStorm Linux baseline in seconds")

    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.command == "preflight":
        checks = collect_preflight(args.require_kernels, args.strict)
        print_checks(checks)
        if args.json:
            args.json.parent.mkdir(parents=True, exist_ok=True)
            args.json.write_text(
                json.dumps([check.as_dict() for check in checks], indent=2) + "\n",
                encoding="utf-8",
            )
        return preflight_exit_code(checks)
    if args.command == "check-log":
        reports = []
        for log_path in args.logs:
            if not log_path.is_file():
                print(f"error: log does not exist: {log_path}", file=sys.stderr)
                return 2
            report = parse_log(
                log_path.resolve(), args.arch, args.baseline, args.expected_cores
            )
            reports.append(report)
            print_log_report(report)
        if args.json:
            args.json.parent.mkdir(parents=True, exist_ok=True)
            args.json.write_text(
                json.dumps(reports, indent=2, ensure_ascii=False) + "\n",
                encoding="utf-8",
            )
        return 0 if all(report["valid"] for report in reports) else 1
    if args.command == "run":
        return run_final(args)
    raise AssertionError(f"unknown command: {args.command}")


if __name__ == "__main__":
    raise SystemExit(main())
