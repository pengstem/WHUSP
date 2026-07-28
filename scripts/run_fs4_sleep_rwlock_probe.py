#!/usr/bin/env python3
"""Run the feature-gated FS4 SleepRwLock kernel probe in one guest cell."""

from __future__ import annotations

import argparse
import json
import re
import shutil
import sys
import tempfile
from pathlib import Path
from typing import Any

import run_rust_hello_bench as bench


REPO_ROOT = Path(__file__).resolve().parents[1]
PROBE_SOURCE = REPO_ROOT / "scripts" / "perf-gates" / "fs4_sleep_rwlock_probe.c"
GUEST_TEMPLATE = REPO_ROOT / "scripts" / "perf-gates" / "fs4-sleep-rwlock-probe.sh"
CASES = ("parallel_readers", "writer_exclusion", "writer_fairness", "interruption_cleanup")


def render_guest(args: argparse.Namespace) -> str:
    source = GUEST_TEMPLATE.read_text(encoding="utf-8")
    for placeholder, value in {
        "@RUN_ID@": args.run_id,
        "@ARCH@": args.arch,
        "@SMP@": str(args.smp),
        "@MEM@": args.mem,
        "@BLOCK_IO_MODE@": args.block_io_mode,
    }.items():
        bench.validate_token(placeholder, value)
        if source.count(placeholder) != 1:
            raise bench.BenchmarkError(f"guest template placeholder mismatch: {placeholder}")
        source = source.replace(placeholder, value)
    if re.search(r"@[A-Z][A-Z0-9_]*@", source):
        raise bench.BenchmarkError("guest template contains an unresolved placeholder")
    return source


def build_probe(args: argparse.Namespace, temp_root: Path, setup: Path) -> Path:
    output = temp_root / "fs4_sleep_rwlock_probe"
    command = [
        str(bench.compiler_for(args.arch)), "-static", "-O2", "-Wall", "-Wextra", "-Werror",
        "-o", str(output), str(PROBE_SOURCE),
    ]
    (setup / "compile-command.txt").write_text(bench.command_text(command), encoding="utf-8")
    completed = bench.run_capture(command)
    (setup / "compile.log").write_text(completed.stdout, encoding="utf-8")
    if completed.returncode != 0 or not output.is_file() or output.stat().st_size == 0:
        raise bench.BenchmarkError(f"probe compilation failed: {completed.returncode}")
    output.chmod(0o755)
    return output


def build_disk(args: argparse.Namespace, temp_root: Path, probe: Path, setup: Path) -> Path:
    staging = temp_root / "staging"
    staging.mkdir()
    entry = staging / "entry.sh"
    entry.write_text(
        "#!/musl/busybox sh\nexec /musl/busybox ash /x1/fs4-sleep-rwlock-probe.sh || exit 127\n",
        encoding="utf-8",
    )
    entry.chmod(0o755)
    guest = staging / "fs4-sleep-rwlock-probe.sh"
    guest.write_text(render_guest(args), encoding="utf-8")
    guest.chmod(0o755)
    installed = staging / "fs4_sleep_rwlock_probe"
    shutil.copy2(probe, installed)
    installed.chmod(0o755)
    image = temp_root / "fs4-sleep-rwlock-probe.img"
    output: list[str] = []
    try:
        bench.run_setup_command([bench.require_command("truncate"), "-s", "64M", str(image)], output)
        bench.run_setup_command(
            [bench.require_command("mkfs.ext4"), "-q", "-F", "-N", "8192", "-O",
             "^orphan_file,^metadata_csum_seed,^metadata_csum,^64bit,^has_journal",
             "-d", str(staging), str(image)],
            output,
        )
    finally:
        (setup / "disk-build.log").write_text("".join(output), encoding="utf-8")
    return image


def validate(log: str, args: argparse.Namespace, overlay_root: Path) -> dict[str, Any]:
    lines = [bench.ANSI_RE.sub("", line.rstrip("\r")) for line in log.splitlines()]
    errors: list[str] = []
    start = f"FS4_SLEEP_RWLOCK_GUEST_START run_id={args.run_id} arch={args.arch} smp={args.smp} mem={args.mem} block_io={args.block_io_mode}"
    passed = f"FS4_SLEEP_RWLOCK_GUEST_PASS run_id={args.run_id} arch={args.arch} smp={args.smp} mem={args.mem} block_io={args.block_io_mode}"
    if lines.count(start) != 1 or lines.count(passed) != 1:
        errors.append("guest start/pass marker mismatch")
    seen = {line.removeprefix("FS4_SLEEP_RWLOCK_CASE_PASS case=") for line in lines
            if line.startswith("FS4_SLEEP_RWLOCK_CASE_PASS case=")}
    if seen != set(CASES):
        errors.append(f"case matrix mismatch: {sorted(seen)}")
    if lines.count("FS4_SLEEP_RWLOCK_PROBE_PASS cases=4") != 1:
        errors.append("probe pass marker mismatch")
    if any("FS4_SLEEP_RWLOCK_CASE_FAIL" in line or "FS4_SLEEP_RWLOCK_FAIL" in line for line in lines):
        errors.append("guest emitted failure marker")
    if bench.PANIC_RE.search(log):
        errors.append("kernel panic signature present")
    policies = list(bench.BLOCK_IO_POLICY_RE.finditer(log))
    if len(policies) != 1 or policies[0].group("block_io") != args.block_io_mode:
        errors.append("block IO policy identity mismatch")
    shutdowns = list(bench.SHUTDOWN_RE.finditer(log))
    if len(shutdowns) != 1 or shutdowns[0].group("failure") != "false":
        errors.append("clean shutdown marker mismatch")
    overlay_path, overlay_errors = bench.validate_overlay_log(log, overlay_root)
    errors.extend(overlay_errors)
    return {"valid": not errors, "errors": errors, "cases": sorted(seen), "overlay_path": overlay_path}


def run(args: argparse.Namespace) -> int:
    output_dir = args.output_dir.resolve()
    if output_dir.exists() and any(output_dir.iterdir()):
        raise bench.BenchmarkError(f"output directory is not empty: {output_dir}")
    output_dir.mkdir(parents=True, exist_ok=True)
    setup = output_dir / "setup"
    setup.mkdir()
    temp_root = Path(tempfile.mkdtemp(prefix="whusp-g0b-fs4-rwlock-"))
    overlay_root = temp_root / "overlays"
    overlay_root.mkdir()
    sample: dict[str, Any] = {
        "schema_version": 1, "started_at": bench.utc_now(),
        "identity": {"run_id": args.run_id, "arch": args.arch, "smp": args.smp,
                     "mem": args.mem, "block_io_mode": args.block_io_mode},
        "kernel": bench.file_metadata(args.kernel_elf),
        "test_disk": bench.file_metadata(args.test_disk),
        "probe_source": bench.file_metadata(PROBE_SOURCE),
    }
    try:
        probe = build_probe(args, temp_root, setup)
        disk = build_disk(args, temp_root, probe, setup)
        command = bench.qemu_command(
            architecture=bench.ARCHITECTURES[args.arch], smp=args.smp, mem=args.mem,
            block_io_mode=args.block_io_mode, perf_counters=0,
            kernel=args.kernel_elf.resolve(), disk=args.test_disk.resolve(), aux_disk=disk,
            overlay_root=overlay_root,
        )
        (output_dir / "command.txt").write_text(bench.command_text(command), encoding="utf-8")
        process = bench.run_logged(command, output_dir / "serial.log", args.timeout)
        sample["process"] = process
        log = (output_dir / "serial.log").read_text(encoding="utf-8", errors="replace")
        result = validate(log, args, overlay_root)
        sample["result"] = result
        errors = list(result["errors"])
        if process["returncode"] != 0 or process["timed_out"]:
            errors.append(f"QEMU failed returncode={process['returncode']} timeout={process['timed_out']}")
        if not process["process_group_cleanup"]:
            errors.append("QEMU process group cleanup failed")
        if any(overlay_root.iterdir()):
            errors.append("owned overlay root retained entries")
        sample["errors"] = errors
        sample["valid"] = not errors
    finally:
        sample["temp_cleanup"] = bench.remove_owned_temp(temp_root)
        sample["finished_at"] = bench.utc_now()
        bench.write_json(output_dir / "sample.json", sample)
    print(f"FS4 SleepRwLock probe {'PASS' if sample['valid'] else 'FAIL'} arch={args.arch}")
    if not sample["valid"]:
        print(json.dumps(sample["errors"], indent=2), file=sys.stderr)
        return 1
    return 0


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--arch", required=True, choices=("rv", "la"))
    parser.add_argument("--smp", type=int, default=8)
    parser.add_argument("--mem", default="8G")
    parser.add_argument("--block-io-mode", choices=("auto", "force-sync"), default="force-sync")
    parser.add_argument("--kernel-elf", type=Path, required=True)
    parser.add_argument("--test-disk", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--timeout", type=float, default=120.0)
    args = parser.parse_args()
    if args.smp < 2 or args.smp > bench.MAX_CPUS:
        parser.error("--smp must be in 2..12")
    if not args.kernel_elf.is_file() or not args.test_disk.is_file():
        parser.error("--kernel-elf and --test-disk must exist")
    bench.validate_token("run_id", args.run_id)
    bench.validate_token("mem", args.mem)
    return args


if __name__ == "__main__":
    try:
        raise SystemExit(run(parse_args()))
    except (bench.BenchmarkError, OSError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(2)
