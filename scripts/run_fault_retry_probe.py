#!/usr/bin/env python3
"""Run the mmap generation-retry and checked-usercopy regression probe."""

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
PROBE_SOURCE = REPO_ROOT / "scripts" / "perf-gates" / "fault-retry-probe.c"
GUEST_TEMPLATE = REPO_ROOT / "scripts" / "perf-gates" / "fault-retry-probe.sh"


def render_guest(args: argparse.Namespace) -> str:
    source = GUEST_TEMPLATE.read_text(encoding="utf-8")
    for placeholder, value in {
        "@RUN_ID@": args.run_id,
        "@ARCH@": args.arch,
        "@SMP@": str(args.smp),
        "@MEM@": args.mem,
    }.items():
        bench.validate_token(placeholder, value)
        if source.count(placeholder) != 1:
            raise bench.BenchmarkError(
                f"guest template placeholder mismatch: {placeholder}"
            )
        source = source.replace(placeholder, value)
    if re.search(r"@[A-Z][A-Z0-9_]*@", source):
        raise bench.BenchmarkError("guest template contains an unresolved placeholder")
    return source


def build_probe(args: argparse.Namespace, temp_root: Path, setup: Path) -> Path:
    output = temp_root / "fault_retry_probe"
    command = [
        str(bench.compiler_for(args.arch)),
        "-static",
        "-O2",
        "-pthread",
        "-Wall",
        "-Wextra",
        "-Werror",
        "-o",
        str(output),
        str(PROBE_SOURCE),
    ]
    (setup / "compile-command.txt").write_text(
        bench.command_text(command), encoding="utf-8"
    )
    completed = bench.run_capture(command)
    (setup / "compile.log").write_text(completed.stdout, encoding="utf-8")
    if completed.returncode != 0 or not output.is_file() or output.stat().st_size == 0:
        raise bench.BenchmarkError(f"probe compilation failed: {completed.returncode}")
    output.chmod(0o755)
    return output


def build_disk(
    args: argparse.Namespace, temp_root: Path, probe: Path, setup: Path
) -> Path:
    staging = temp_root / "staging"
    staging.mkdir()
    entry = staging / "entry.sh"
    entry.write_text(
        "#!/musl/busybox sh\n"
        "exec /musl/busybox ash /x1/fault-retry-probe.sh || exit 127\n",
        encoding="utf-8",
    )
    entry.chmod(0o755)
    guest = staging / "fault-retry-probe.sh"
    guest.write_text(render_guest(args), encoding="utf-8")
    guest.chmod(0o755)
    installed = staging / "fault_retry_probe"
    shutil.copy2(probe, installed)
    installed.chmod(0o755)
    image = temp_root / "fault-retry-probe.img"
    output: list[str] = []
    try:
        bench.run_setup_command(
            [bench.require_command("truncate"), "-s", "64M", str(image)], output
        )
        bench.run_setup_command(
            [
                bench.require_command("mkfs.ext4"),
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
        (setup / "disk-build.log").write_text("".join(output), encoding="utf-8")
    return image


def perf_snapshot(lines: list[str], point: str) -> tuple[dict[str, int], list[str]]:
    begin = f"FAULT_RETRY_PERF_BEGIN point={point}"
    end = f"FAULT_RETRY_PERF_END point={point}"
    begins = [index for index, line in enumerate(lines) if line == begin]
    ends = [index for index, line in enumerate(lines) if line == end]
    if len(begins) != 1 or len(ends) != 1 or begins[0] >= ends[0]:
        return {}, [f"invalid {point} perf snapshot markers"]
    values: dict[str, int] = {}
    errors: list[str] = []
    for line in lines[begins[0] + 1 : ends[0]]:
        match = bench.PERF_VALUE_RE.fullmatch(line)
        if match is None:
            continue
        key = match.group("key")
        if key in values:
            errors.append(f"duplicate {point} perf key: {key}")
        values[key] = int(match.group("value"))
    return values, errors


def validate(
    log: str, args: argparse.Namespace, overlay_root: Path
) -> dict[str, Any]:
    lines = [bench.ANSI_RE.sub("", line.rstrip("\r")) for line in log.splitlines()]
    errors: list[str] = []
    start = (
        f"FAULT_RETRY_GUEST_START run_id={args.run_id} arch={args.arch} "
        f"smp={args.smp} mem={args.mem}"
    )
    passed = (
        f"FAULT_RETRY_GUEST_PASS run_id={args.run_id} arch={args.arch} "
        f"smp={args.smp} mem={args.mem}"
    )
    if lines.count(start) != 1 or lines.count(passed) != 1:
        errors.append("guest start/pass marker mismatch")
    probe_passes = [
        line for line in lines if line.startswith("FAULT_RETRY_PROBE_PASS ")
    ]
    if (
        len(probe_passes) != 1
        or "bad_address=EFAULT" not in probe_passes[0]
        or "segments=0,1,2,N" not in probe_passes[0]
        or "mid_fault=128" not in probe_passes[0]
        or "append=PASS" not in probe_passes[0]
        or "truncate=PASS" not in probe_passes[0]
    ):
        errors.append(f"probe pass marker mismatch: {probe_passes}")
    if any("FAULT_RETRY_PROBE_FAIL" in line or "FAULT_RETRY_GUEST_FAIL" in line for line in lines):
        errors.append("guest emitted a failure marker")
    if bench.PANIC_RE.search(log):
        errors.append("kernel panic signature present")

    traces = [line for line in lines if line.startswith("FAULT_RETRY origin=Usercopy ")]
    generation_traces = [
        line for line in traces if "reason=GenerationUnstable " in line
    ]
    if not generation_traces:
        errors.append("no usercopy GenerationUnstable trace was observed")

    before, before_errors = perf_snapshot(lines, "before")
    after, after_errors = perf_snapshot(lines, "after")
    errors.extend(before_errors)
    errors.extend(after_errors)
    perf_delta = {
        key: after[key] - before.get(key, 0)
        for key in after
        if key in before and after[key] >= before[key]
    }
    required_positive = (
        "fault_usercopy_retries",
        "fault_retry_generation_unstable",
        "fault_usercopy_retry_waits",
        "fault_usercopy_retry_resolved",
        "fault_usercopy_max_consecutive_retry",
        "usercopy_translated_empty_calls",
        "usercopy_translated_inline_one_calls",
        "usercopy_translated_many_calls",
        "usercopy_translated_many_segments",
        "usercopy_segment_vec_allocs",
        "usercopy_segment_vec_slots",
    )
    for key in required_positive:
        if perf_delta.get(key, 0) == 0:
            errors.append(f"required probe counter did not increase: {key}")
    if perf_delta.get("fault_usercopy_retry_fatal", 0) != 0:
        errors.append("retryable usercopy fault became fatal")
    if perf_delta.get("usercopy_translated_many_segments", 0) < 2 * perf_delta.get(
        "usercopy_translated_many_calls", 0
    ):
        errors.append("Many carrier shape conservation failed")
    if perf_delta.get("usercopy_segment_vec_slots", 0) < 2 * perf_delta.get(
        "usercopy_segment_vec_allocs", 0
    ):
        errors.append("segment Vec allocation slot conservation failed")

    policies = list(bench.BLOCK_IO_POLICY_RE.finditer(log))
    if (
        len(policies) != 1
        or policies[0].group("block_io") != args.block_io_mode
        or policies[0].group("perf_counters") != "true"
    ):
        errors.append("block IO policy identity mismatch")
    shutdowns = list(bench.SHUTDOWN_RE.finditer(log))
    if len(shutdowns) != 1 or shutdowns[0].group("failure") != "false":
        errors.append("clean shutdown marker mismatch")
    overlay_path, overlay_errors = bench.validate_overlay_log(log, overlay_root)
    errors.extend(overlay_errors)
    return {
        "valid": not errors,
        "errors": errors,
        "trace_count": len(traces),
        "generation_trace_count": len(generation_traces),
        "perf_before": before,
        "perf_after": after,
        "perf_delta": perf_delta,
        "overlay_path": overlay_path,
    }


def run(args: argparse.Namespace) -> int:
    output_dir = args.output_dir.resolve()
    if output_dir.exists() and any(output_dir.iterdir()):
        raise bench.BenchmarkError(f"output directory is not empty: {output_dir}")
    output_dir.mkdir(parents=True, exist_ok=True)
    setup = output_dir / "setup"
    setup.mkdir()
    temp_root = Path(tempfile.mkdtemp(prefix="whusp-g0b-fault-retry-"))
    overlay_root = temp_root / "overlays"
    overlay_root.mkdir()
    sample: dict[str, Any] = {
        "schema_version": 1,
        "started_at": bench.utc_now(),
        "identity": {
            "run_id": args.run_id,
            "arch": args.arch,
            "smp": args.smp,
            "mem": args.mem,
            "block_io_mode": args.block_io_mode,
            "perf_counters": 1,
            "fault_trace": True,
        },
        "kernel": bench.file_metadata(args.kernel_elf),
        "test_disk": bench.file_metadata(args.test_disk),
        "probe_source": bench.file_metadata(PROBE_SOURCE),
    }
    try:
        probe = build_probe(args, temp_root, setup)
        disk = build_disk(args, temp_root, probe, setup)
        command = bench.qemu_command(
            architecture=bench.ARCHITECTURES[args.arch],
            smp=args.smp,
            mem=args.mem,
            block_io_mode=args.block_io_mode,
            perf_counters=1,
            kernel=args.kernel_elf.resolve(),
            disk=args.test_disk.resolve(),
            aux_disk=disk,
            overlay_root=overlay_root,
            gdb_port=None,
        )
        (output_dir / "command.txt").write_text(
            bench.command_text(command), encoding="utf-8"
        )
        process = bench.run_logged(command, output_dir / "serial.log", args.timeout)
        sample["process"] = process
        log = (output_dir / "serial.log").read_text(
            encoding="utf-8", errors="replace"
        )
        result = validate(log, args, overlay_root)
        sample["result"] = result
        errors = list(result["errors"])
        if process["returncode"] != 0 or process["timed_out"]:
            errors.append(
                f"QEMU failed returncode={process['returncode']} "
                f"timeout={process['timed_out']}"
            )
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
    print(
        f"fault retry probe {'PASS' if sample['valid'] else 'FAIL'} arch={args.arch}"
    )
    if not sample["valid"]:
        print(json.dumps(sample["errors"], indent=2), file=sys.stderr)
        return 1
    return 0


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--arch", required=True, choices=("rv", "la"))
    parser.add_argument("--smp", type=int, default=8)
    parser.add_argument("--mem", default="8G")
    parser.add_argument(
        "--block-io-mode", choices=("auto", "force-sync"), default="force-sync"
    )
    parser.add_argument("--kernel-elf", type=Path, required=True)
    parser.add_argument("--test-disk", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--timeout", type=float, default=300.0)
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
