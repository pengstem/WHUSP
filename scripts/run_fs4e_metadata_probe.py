#!/usr/bin/env python3
"""Run the FS4-E versioned dentry/metadata cache probe."""

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
PROBE_SOURCE = REPO_ROOT / "scripts" / "perf-gates" / "fs4e_metadata_probe.c"
GUEST_TEMPLATE = REPO_ROOT / "scripts" / "perf-gates" / "fs4e-metadata-probe.sh"
SCALE_WORKERS = (1, 2, 4, 8)


def render_guest(args: argparse.Namespace) -> str:
    source = GUEST_TEMPLATE.read_text(encoding="utf-8")
    for placeholder, value in {
        "@RUN_ID@": args.run_id,
        "@ARCH@": args.arch,
        "@SMP@": str(args.smp),
        "@MEM@": args.mem,
        "@BLOCK_IO_MODE@": args.block_io_mode,
        "@PERF_COUNTERS@": str(args.perf_counters),
        "@EXPECTATION@": args.expectation,
    }.items():
        bench.validate_token(placeholder, value)
        if source.count(placeholder) == 0:
            raise bench.BenchmarkError(f"guest template placeholder mismatch: {placeholder}")
        source = source.replace(placeholder, value)
    if re.search(r"@[A-Z][A-Z0-9_]*@", source):
        raise bench.BenchmarkError("guest template contains an unresolved placeholder")
    return source


def build_probe(args: argparse.Namespace, temp_root: Path, setup: Path) -> Path:
    output = temp_root / "fs4e_metadata_probe"
    command = [
        str(bench.compiler_for(args.arch)),
        "-static",
        "-pthread",
        "-O2",
        "-Wall",
        "-Wextra",
        "-Werror",
        "-o",
        str(output),
        str(PROBE_SOURCE),
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
        "#!/musl/busybox sh\nexec /musl/busybox ash /x1/fs4e-metadata-probe.sh || exit 127\n",
        encoding="utf-8",
    )
    entry.chmod(0o755)
    guest = staging / "fs4e-metadata-probe.sh"
    guest.write_text(render_guest(args), encoding="utf-8")
    guest.chmod(0o755)
    installed = staging / "fs4e_metadata_probe"
    shutil.copy2(probe, installed)
    installed.chmod(0o755)
    image = temp_root / "fs4e-metadata-probe.img"
    output: list[str] = []
    try:
        bench.run_setup_command([bench.require_command("truncate"), "-s", "64M", str(image)], output)
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


def validate(log: str, args: argparse.Namespace, overlay_root: Path) -> dict[str, Any]:
    lines = [bench.ANSI_RE.sub("", line.rstrip("\r")) for line in log.splitlines()]
    errors: list[str] = []
    start = (
        f"FS4E_METADATA_GUEST_START run_id={args.run_id} arch={args.arch} smp={args.smp} "
        f"mem={args.mem} block_io={args.block_io_mode} perf={args.perf_counters} "
        f"expectation={args.expectation} affinity=pinned-sequential timing=barrier-work-only"
    )
    passed = (
        f"FS4E_METADATA_GUEST_PASS run_id={args.run_id} arch={args.arch} smp={args.smp} "
        f"mem={args.mem} block_io={args.block_io_mode} perf={args.perf_counters} "
        f"expectation={args.expectation} affinity=pinned-sequential timing=barrier-work-only"
    )
    if lines.count(start) != 1 or lines.count(passed) != 1:
        errors.append("guest start/pass marker mismatch")
    cache_hits = [line for line in lines if line.startswith("FS4E_CACHE_HIT_RESULT ")]
    single_flights = [line for line in lines if line.startswith("FS4E_SINGLE_FLIGHT_RESULT ")]
    mutations = [line for line in lines if line.startswith("FS4E_MUTATION_PASS ")]
    counter_lane = args.expectation != "performance"
    if len(cache_hits) != int(counter_lane):
        errors.append("cache-hit result marker mismatch")
    if len(single_flights) != int(counter_lane):
        errors.append("same-key single-flight result marker mismatch")
    if args.expectation == "candidate":
        if not cache_hits or "lookup_delta=0 stat_delta=0" not in cache_hits[0]:
            errors.append("cache-hit zero-backend contract failed")
        single_flight_counts = (
            re.search(r"\blookup_delta=(\d+) stat_delta=(\d+)\b", single_flights[0])
            if single_flights
            else None
        )
        if (
            single_flight_counts is None
            or int(single_flight_counts.group(1)) != 1
            or int(single_flight_counts.group(2)) > 1
        ):
            errors.append("same-key single-flight contract failed")
    if len(mutations) != 1:
        errors.append("mutation marker mismatch")
    scale_re = re.compile(
        r"^FS4E_SCALE workers=(?P<workers>\d+) operations=(?P<operations>\d+) "
        r"elapsed_ns=(?P<elapsed>\d+) ops_per_second=(?P<throughput>\d+)$"
    )
    scaling: dict[int, dict[str, int]] = {}
    for line in lines:
        match = scale_re.match(line)
        if match:
            workers = int(match.group("workers"))
            scaling[workers] = {
                "operations": int(match.group("operations")),
                "elapsed_ns": int(match.group("elapsed")),
                "ops_per_second": int(match.group("throughput")),
            }
    if tuple(sorted(scaling)) != SCALE_WORKERS or any(
        cell["elapsed_ns"] <= 0 or cell["ops_per_second"] <= 0 for cell in scaling.values()
    ):
        errors.append(f"scaling matrix mismatch: {sorted(scaling)}")
    if lines.count("FS4E_METADATA_PROBE_PASS") != 1 or any(
        "FS4E_METADATA_PROBE_FAIL" in line for line in lines
    ):
        errors.append("probe pass/fail marker mismatch")
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
    return {
        "valid": not errors,
        "errors": errors,
        "scaling": scaling,
        "cache_hit": cache_hits[0] if cache_hits else None,
        "single_flight": single_flights[0] if single_flights else None,
        "overlay_path": overlay_path,
    }


def run(args: argparse.Namespace) -> int:
    output_dir = args.output_dir.resolve()
    if output_dir.exists() and any(output_dir.iterdir()):
        raise bench.BenchmarkError(f"output directory is not empty: {output_dir}")
    output_dir.mkdir(parents=True, exist_ok=True)
    setup = output_dir / "setup"
    setup.mkdir()
    temp_root = Path(tempfile.mkdtemp(prefix="whusp-g0b-fs4e-metadata-"))
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
            "expectation": args.expectation,
            "affinity": "pinned-sequential",
            "timing": "barrier-work-only",
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
            perf_counters=args.perf_counters,
            kernel=args.kernel_elf.resolve(),
            disk=args.test_disk.resolve(),
            aux_disk=disk,
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
    print(f"FS4-E metadata probe {'PASS' if sample['valid'] else 'FAIL'} arch={args.arch}")
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
    parser.add_argument(
        "--expectation", choices=("candidate", "baseline", "performance"), default="candidate"
    )
    parser.add_argument("--kernel-elf", type=Path, required=True)
    parser.add_argument("--test-disk", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--timeout", type=float, default=180.0)
    args = parser.parse_args()
    if args.smp < 2 or args.smp > bench.MAX_CPUS:
        parser.error("--smp must be in 2..12")
    if args.expectation == "performance" and args.perf_counters != 0:
        parser.error("performance expectation requires --perf-counters 0")
    if args.expectation != "performance" and args.perf_counters != 1:
        parser.error("candidate/baseline expectations require --perf-counters 1")
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
