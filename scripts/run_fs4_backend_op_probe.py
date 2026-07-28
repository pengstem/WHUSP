#!/usr/bin/env python3
"""Run the FS4 barriered backend-operation microprobe in one guest cell."""

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
PROBE_SOURCE = REPO_ROOT / "scripts" / "perf-gates" / "fs4_backend_op_probe.c"
GUEST_TEMPLATE = REPO_ROOT / "scripts" / "perf-gates" / "fs4-backend-op-probe.sh"
MODES = ("independent_file", "independent_dir", "same_inode", "same_dir")
WORKERS = (1, 2, 4, 8)
OPS = (
    "lookup",
    "stat_basic",
    "stat_full",
    "read_plan",
    "read_fallback",
    "readlink",
    "readdir",
    "write",
    "truncate_allocate",
    "namespace_mutation",
    "inode_lifetime",
    "sync",
)
OP_METRICS = (
    "calls",
    "contended",
    "wait_us",
    "wait_max_us",
    "hold_us",
    "hold_max_us",
    "read_calls",
    "read_blocks",
    "read_bytes",
    "write_calls",
    "write_blocks",
    "write_bytes",
)
CELL_RE = re.compile(
    r"FS4_BACKEND_OP_CELL mode=(?P<mode>[a-z_]+) "
    r"workers=(?P<workers>[0-9]+) iterations=(?P<iterations>[0-9]+) "
    r"operations=(?P<operations>[0-9]+) elapsed_ns=(?P<elapsed_ns>[0-9]+) "
    r"throughput_ops_per_s=(?P<throughput>[0-9]+) errors=(?P<errors>[0-9]+)"
)


def render_guest(args: argparse.Namespace) -> str:
    source = GUEST_TEMPLATE.read_text(encoding="utf-8")
    replacements = {
        "@RUN_ID@": args.run_id,
        "@ARCH@": args.arch,
        "@SMP@": str(args.smp),
        "@MEM@": args.mem,
        "@BLOCK_IO_MODE@": args.block_io_mode,
        "@PERF_COUNTERS@": str(args.perf_counters),
        "@ITERATIONS@": str(args.iterations),
    }
    for placeholder, value in replacements.items():
        bench.validate_token(placeholder, value)
        if source.count(placeholder) != 1:
            raise bench.BenchmarkError(f"guest template placeholder mismatch: {placeholder}")
        source = source.replace(placeholder, value)
    if re.search(r"@[A-Z][A-Z0-9_]*@", source):
        raise bench.BenchmarkError("guest template contains an unresolved placeholder")
    return source


def build_probe(compiler: Path, temp_root: Path, setup_dir: Path) -> Path:
    output = temp_root / "fs4_backend_op_probe"
    command = [
        str(compiler),
        "-static",
        "-O2",
        "-Wall",
        "-Wextra",
        "-Werror",
        "-o",
        str(output),
        str(PROBE_SOURCE),
    ]
    (setup_dir / "probe-compile-command.txt").write_text(
        bench.command_text(command), encoding="utf-8"
    )
    completed = bench.run_capture(command)
    (setup_dir / "probe-compile.log").write_text(completed.stdout, encoding="utf-8")
    if completed.returncode != 0:
        raise bench.BenchmarkError(f"probe compiler exited with {completed.returncode}")
    if not output.is_file() or output.stat().st_size == 0:
        raise bench.BenchmarkError("probe compiler produced no binary")
    output.chmod(0o755)
    return output


def build_aux_disk(
    args: argparse.Namespace, temp_root: Path, probe: Path, setup_log: Path
) -> Path:
    staging = temp_root / "staging"
    staging.mkdir()
    entry = staging / "entry.sh"
    entry.write_text(
        "#!/musl/busybox sh\nexec /musl/busybox ash /x1/fs4-backend-op-probe.sh || exit 127\n",
        encoding="utf-8",
    )
    entry.chmod(0o755)
    guest = staging / "fs4-backend-op-probe.sh"
    guest.write_text(render_guest(args), encoding="utf-8")
    guest.chmod(0o755)
    installed_probe = staging / "fs4_backend_op_probe"
    shutil.copy2(probe, installed_probe)
    installed_probe.chmod(0o755)
    image = temp_root / "fs4-backend-op-probe.img"
    output: list[str] = []
    try:
        bench.run_setup_command(
            [bench.require_command("truncate"), "-s", args.image_size, str(image)], output
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
        setup_log.write_text("".join(output), encoding="utf-8")
    return image


def perf_snapshot(lines: list[str], point: str) -> tuple[dict[str, int], list[str]]:
    begin = f"FS4_BACKEND_OP_PERF_BEGIN point={point}"
    end = f"FS4_BACKEND_OP_PERF_END point={point}"
    errors: list[str] = []
    begin_indexes = [i for i, line in enumerate(lines) if line == begin]
    end_indexes = [i for i, line in enumerate(lines) if line == end]
    if len(begin_indexes) != 1 or len(end_indexes) != 1:
        return {}, [f"expected one {point} perf snapshot"]
    first, last = begin_indexes[0], end_indexes[0]
    if first >= last:
        return {}, [f"invalid {point} perf marker order"]
    values: dict[str, int] = {}
    for line in lines[first + 1 : last]:
        match = bench.PERF_VALUE_RE.fullmatch(line)
        if match is None:
            continue
        key = match.group("key")
        if key in values:
            errors.append(f"duplicate {point} perf key: {key}")
            continue
        values[key] = int(match.group("value"))
    return values, errors


def validate_log(log: str, args: argparse.Namespace, overlay_root: Path) -> dict[str, Any]:
    lines = [bench.ANSI_RE.sub("", line.rstrip("\r")) for line in log.splitlines()]
    errors: list[str] = []
    start = (
        f"FS4_BACKEND_OP_GUEST_START run_id={args.run_id} arch={args.arch} smp={args.smp} "
        f"mem={args.mem} block_io={args.block_io_mode} perf={args.perf_counters}"
    )
    passed = (
        f"FS4_BACKEND_OP_PASS run_id={args.run_id} arch={args.arch} smp={args.smp} "
        f"mem={args.mem} block_io={args.block_io_mode} perf={args.perf_counters}"
    )
    if lines.count(start) != 1:
        errors.append("missing or duplicate guest start marker")
    if lines.count(passed) != 1:
        errors.append("missing or duplicate guest pass marker")
    if any("FS4_BACKEND_OP_FAIL" in line for line in lines):
        errors.append("guest emitted fail marker")
    if bench.PANIC_RE.search(log):
        errors.append("kernel panic signature present")

    cells: list[dict[str, Any]] = []
    seen: set[tuple[str, int]] = set()
    for line in lines:
        match = CELL_RE.fullmatch(line)
        if match is None:
            continue
        cell = {
            "mode": match.group("mode"),
            "workers": int(match.group("workers")),
            "iterations": int(match.group("iterations")),
            "operations": int(match.group("operations")),
            "elapsed_ns": int(match.group("elapsed_ns")),
            "throughput_ops_per_s": int(match.group("throughput")),
            "errors": int(match.group("errors")),
        }
        key = (cell["mode"], cell["workers"])
        if key in seen:
            errors.append(f"duplicate cell: {key}")
        seen.add(key)
        if cell["mode"] not in MODES or cell["workers"] not in WORKERS:
            errors.append(f"unexpected cell: {key}")
        if cell["iterations"] != args.iterations or cell["errors"] != 0:
            errors.append(f"invalid cell result: {key}")
        if cell["operations"] != cell["workers"] * args.iterations * 4:
            errors.append(f"operation count mismatch: {key}")
        cells.append(cell)
    expected = {(mode, workers) for mode in MODES for workers in WORKERS}
    if seen != expected:
        errors.append(f"cell matrix mismatch: missing={sorted(expected - seen)}")

    before, before_errors = perf_snapshot(lines, "before")
    after, after_errors = perf_snapshot(lines, "after")
    errors.extend(before_errors)
    errors.extend(after_errors)
    required = {"perf_counters_enabled"}
    if args.perf_counters == 1:
        required.update({
        "mount_backend_contended_acquisitions",
        "profile_mount_backend_hold_calls",
        "backend_try_successful_calls",
        "pending_release_drain_calls",
        "pending_release_drain_entries",
        "pending_release_drain_released",
        "block_io_device_inflight",
        "block_io_device_inflight_high_watermark",
        "ext4_block_read_calls",
        "ext4_block_read_blocks",
        "ext4_block_read_bytes",
        "ext4_block_write_calls",
        "ext4_block_write_blocks",
        "ext4_block_write_bytes",
        })
        required.update(f"backend_op_{op}_{metric}" for op in OPS for metric in OP_METRICS)
        for prefix in ("backend_lock_held_data_io", "backend_lock_held_metadata_io"):
            required.update(
                f"{prefix}_{metric}"
                for metric in (
                    "read_calls",
                    "read_blocks",
                    "read_bytes",
                    "write_calls",
                    "write_blocks",
                    "write_bytes",
                )
            )
    missing = sorted(required - before.keys()) + sorted(required - after.keys())
    if missing:
        errors.append(f"missing perf keys: {missing}")
    delta = {key: after[key] - before.get(key, 0) for key in after}
    if (
        before.get("perf_counters_enabled") != args.perf_counters
        or after.get("perf_counters_enabled") != args.perf_counters
    ):
        errors.append("perf counter identity mismatch in guest")

    if args.perf_counters == 1 and not missing:
        op_calls = sum(delta[f"backend_op_{op}_calls"] for op in OPS)
        expected_calls = (
            delta["profile_mount_backend_hold_calls"] + delta["backend_try_successful_calls"]
        )
        if op_calls != expected_calls:
            errors.append(f"backend call conservation failed: {op_calls} != {expected_calls}")
        op_contended = sum(delta[f"backend_op_{op}_contended"] for op in OPS)
        if op_contended != delta["mount_backend_contended_acquisitions"]:
            errors.append("backend contention conservation failed")
        for direction in ("read", "write"):
            for measure in ("calls", "blocks", "bytes"):
                held = sum(
                    delta[f"backend_lock_held_{kind}_io_{direction}_{measure}"]
                    for kind in ("data", "metadata")
                )
                ext4 = delta[f"ext4_block_{direction}_{measure}"]
                if held != ext4:
                    errors.append(
                        f"adapter IO conservation failed for {direction}_{measure}: {held} != {ext4}"
                    )
        if after["block_io_device_inflight"] != 0:
            errors.append("block IO remained inflight after probe")
        if after["block_io_device_inflight_high_watermark"] < 1:
            errors.append("block IO high watermark was not observed")

    policies = list(bench.BLOCK_IO_POLICY_RE.finditer(log))
    if len(policies) != 1 or policies[0].group("block_io") != args.block_io_mode:
        errors.append("block IO policy identity mismatch")
    shutdowns = list(bench.SHUTDOWN_RE.finditer(log))
    if len(shutdowns) != 1 or shutdowns[0].group("failure") != "false":
        errors.append("clean shutdown marker mismatch")
    overlay_path, overlay_errors = bench.validate_overlay_log(log, overlay_root)
    errors.extend(overlay_errors)
    return {
        "valid": not errors,
        "errors": errors,
        "cells": cells,
        "perf": {"before": before, "after": after, "delta": delta},
        "overlay_path": overlay_path,
    }


def run(args: argparse.Namespace) -> int:
    output_dir = args.output_dir.resolve()
    if output_dir.exists() and any(output_dir.iterdir()):
        raise bench.BenchmarkError(f"output directory is not empty: {output_dir}")
    output_dir.mkdir(parents=True, exist_ok=True)
    setup_dir = output_dir / "setup"
    setup_dir.mkdir()
    compiler = bench.compiler_for(args.arch)
    architecture = bench.ARCHITECTURES[args.arch]
    temp_root = Path(tempfile.mkdtemp(prefix="whusp-g0b-fs4-"))
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
            "perf_counters": args.perf_counters,
            "iterations": args.iterations,
        },
        "host_load_before": bench.host_load_snapshot(),
        "kernel": bench.file_metadata(args.kernel_elf),
        "test_disk": bench.file_metadata(args.test_disk),
        "probe_source": bench.file_metadata(PROBE_SOURCE),
        "guest_template": bench.file_metadata(GUEST_TEMPLATE),
    }
    try:
        probe = build_probe(compiler, temp_root, setup_dir)
        aux_disk = build_aux_disk(args, temp_root, probe, setup_dir / "disk-build.log")
        command = bench.qemu_command(
            architecture=architecture,
            smp=args.smp,
            mem=args.mem,
            block_io_mode=args.block_io_mode,
            perf_counters=args.perf_counters,
            kernel=args.kernel_elf.resolve(),
            disk=args.test_disk.resolve(),
            aux_disk=aux_disk,
            overlay_root=overlay_root,
        )
        (output_dir / "command.txt").write_text(
            bench.command_text(command), encoding="utf-8"
        )
        process = bench.run_logged(command, output_dir / "serial.log", args.timeout)
        sample["process"] = process
        log = (output_dir / "serial.log").read_text(encoding="utf-8", errors="replace")
        result = validate_log(log, args, overlay_root)
        sample["result"] = result
        errors = list(result["errors"])
        if process["returncode"] != 0:
            errors.append(f"QEMU/make exited with {process['returncode']}")
        if process["timed_out"]:
            errors.append("QEMU timed out")
        if not process["process_group_cleanup"]:
            errors.append("QEMU process group cleanup failed")
        if overlay_root.is_dir() and any(overlay_root.iterdir()):
            errors.append("owned overlay root retained entries")
        sample["errors"] = errors
        sample["valid"] = not errors
    finally:
        sample["temp_cleanup"] = bench.remove_owned_temp(temp_root)
        sample["host_load_after"] = bench.host_load_snapshot()
        sample["finished_at"] = bench.utc_now()
        bench.write_json(output_dir / "sample.json", sample)
    print(
        f"FS4 backend-op probe {'PASS' if sample['valid'] else 'FAIL'} "
        f"arch={args.arch} cells={len(sample.get('result', {}).get('cells', []))}"
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
    parser.add_argument("--block-io-mode", choices=("auto", "force-sync"), default="force-sync")
    parser.add_argument("--perf-counters", type=int, choices=(0, 1), default=1)
    parser.add_argument("--kernel-elf", type=Path, required=True)
    parser.add_argument("--test-disk", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--iterations", type=int, default=200)
    parser.add_argument("--timeout", type=float, default=240.0)
    parser.add_argument("--image-size", default="64M")
    args = parser.parse_args()
    if args.smp < 8 or args.smp > bench.MAX_CPUS:
        parser.error("--smp must be in 8..12 so the 1/2/4/8 matrix is complete")
    if args.iterations < 1:
        parser.error("--iterations must be positive")
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
