#!/usr/bin/env python3
"""Run one frozen IO0-A block-I/O policy benchmark lane."""

from __future__ import annotations

import argparse
import fcntl
import itertools
import os
import subprocess
import sys
import tempfile
import time
from collections.abc import Iterator
from contextlib import contextmanager
from dataclasses import dataclass
from datetime import datetime, timezone
from decimal import Decimal
from pathlib import Path
from typing import Any

import run_rust_hello_bench as bench

REPO_ROOT = Path(__file__).resolve().parents[1]
CONTROLLER_SOURCE = Path(__file__).resolve()
SMP = 8
MEM = "8G"
WARMUPS = 1
MEASURED = 5
POLICIES = ("auto", "force-sync")
NOISE_THRESHOLD_PERCENT = Decimal(5)
WORKLOAD_COMPARABILITY_THRESHOLD_PERCENT = Decimal(2)
QEMU_PROCESS_PREFIX = "qemu-system-"
PERCENT_DECIMAL_PLACES = 6


@contextmanager
def repository_controller_lock() -> Iterator[dict[str, Any]]:
    """Hold one advisory lock on this repository directory for the whole lane."""
    flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0)
    try:
        descriptor = os.open(REPO_ROOT, flags)
    except OSError as error:
        raise bench.BenchmarkError(
            f"cannot open repository directory for controller lock: {error}"
        ) from error
    locked = False
    try:
        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise bench.BenchmarkError(
                "another IO0-A controller already holds the repository lock"
            ) from error
        except OSError as error:
            raise bench.BenchmarkError(
                f"cannot acquire repository controller lock: {error}"
            ) from error
        locked = True
        yield {
            "scope": str(REPO_ROOT),
            "mechanism": "flock-repository-directory",
            "acquired": True,
            "owner_pid": os.getpid(),
            "held_for_entire_lane": True,
        }
    finally:
        release_error: OSError | None = None
        if locked:
            try:
                fcntl.flock(descriptor, fcntl.LOCK_UN)
            except OSError as error:
                release_error = error
        try:
            os.close(descriptor)
        except OSError as error:
            release_error = release_error or error
        if release_error is not None:
            raise bench.BenchmarkError(
                f"cannot release repository controller lock: {release_error}"
            ) from release_error


def is_qemu_process_name(name: str) -> bool:
    return Path(name.removesuffix(" (deleted)")).name.startswith(QEMU_PROCESS_PREFIX)


def foreign_qemu_processes() -> list[dict[str, Any]]:
    processes: list[dict[str, Any]] = []
    proc_root = Path("/proc")
    try:
        process_directories = sorted(
            (entry for entry in proc_root.iterdir() if entry.name.isdecimal()),
            key=lambda entry: int(entry.name),
        )
    except OSError as error:
        raise bench.BenchmarkError(
            f"cannot enumerate host processes: {error}"
        ) from error

    for process_directory in process_directories:
        pid = int(process_directory.name)
        if pid == os.getpid():
            continue
        observed_names: list[str] = []
        try:
            observed_names.append(os.readlink(process_directory / "exe"))
        except OSError:
            pass
        try:
            command = (process_directory / "cmdline").read_bytes().split(b"\0", 1)[0]
            if command:
                observed_names.append(os.fsdecode(command))
        except OSError:
            pass
        try:
            observed_names.append(
                (process_directory / "comm").read_text(encoding="ascii").strip()
            )
        except (OSError, UnicodeError):
            pass
        if not any(is_qemu_process_name(name) for name in observed_names):
            continue
        processes.append(
            {
                "pid": pid,
                "observed_names": list(dict.fromkeys(observed_names)),
            }
        )
    return processes


def require_no_foreign_qemu() -> dict[str, Any]:
    processes = foreign_qemu_processes()
    result = {
        "checked_at": bench.utc_now(),
        "processes": processes,
        "passed": not processes,
    }
    if processes:
        pids = [process["pid"] for process in processes]
        raise bench.BenchmarkError(
            f"foreign QEMU process detected before IO0-A lane start: pids={pids!r}"
        )
    return result


@dataclass(frozen=True, order=True)
class Cell:
    arch: str
    policy: str
    perf: int

    @property
    def key(self) -> str:
        return f"p{self.perf}:{self.arch}:{self.policy}"

    @property
    def directory(self) -> Path:
        return Path(f"{self.arch}-{self.policy}")


@dataclass(frozen=True)
class TrialSpec:
    sequence: int
    phase: str
    sample: int
    cell: Cell

    @property
    def kind(self) -> str:
        return "warmup" if self.phase == "W" else "measured"

    @property
    def trial(self) -> bench.Trial:
        ordinal = 0 if self.kind == "warmup" else self.sample
        return bench.Trial(ordinal=ordinal, kind=self.kind, sample=self.sample)

    @property
    def label(self) -> str:
        return f"p{self.cell.perf}-{self.phase}-{self.cell.arch}-{self.cell.policy}"


def cells_for_perf(perf: int) -> dict[str, Cell]:
    if perf not in {0, 1}:
        raise bench.BenchmarkError(f"unsupported perf lane: {perf}")
    return {
        "RV-A": Cell("rv", "auto", perf),
        "RV-S": Cell("rv", "force-sync", perf),
        "LA-A": Cell("la", "auto", perf),
        "LA-S": Cell("la", "force-sync", perf),
    }


def frozen_lane_rows(perf: int) -> tuple[tuple[str, tuple[str, ...]], ...]:
    if perf == 0:
        return (
            ("W", ("RV-A", "RV-S", "LA-S", "LA-A")),
            ("R1", ("RV-A", "RV-S", "LA-A", "LA-S")),
            ("R2", ("LA-S", "LA-A", "RV-S", "RV-A")),
            ("R3", ("RV-S", "RV-A", "LA-S", "LA-A")),
            ("R4", ("LA-A", "LA-S", "RV-A", "RV-S")),
            ("R5", ("RV-A", "RV-S", "LA-A", "LA-S")),
        )
    if perf == 1:
        return (
            ("W", ("LA-A", "LA-S", "RV-S", "RV-A")),
            ("R1", ("LA-S", "LA-A", "RV-S", "RV-A")),
            ("R2", ("RV-A", "RV-S", "LA-A", "LA-S")),
            ("R3", ("LA-A", "LA-S", "RV-A", "RV-S")),
            ("R4", ("RV-S", "RV-A", "LA-S", "LA-A")),
            ("R5", ("LA-S", "LA-A", "RV-S", "RV-A")),
        )
    raise bench.BenchmarkError(f"unsupported perf lane: {perf}")


def frozen_schedule(perf: int) -> list[TrialSpec]:
    cells = cells_for_perf(perf)
    schedule: list[TrialSpec] = []
    for phase, row in frozen_lane_rows(perf):
        sample = 1 if phase == "W" else int(phase[1:])
        for name in row:
            schedule.append(
                TrialSpec(
                    sequence=len(schedule) + 1,
                    phase=phase,
                    sample=sample,
                    cell=cells[name],
                )
            )
    validate_schedule(schedule, perf)
    return schedule


def lane_cells(perf: int) -> tuple[Cell, ...]:
    return tuple(
        Cell(arch, policy, perf) for arch in ("rv", "la") for policy in POLICIES
    )


def validate_schedule(schedule: list[TrialSpec], perf: int) -> None:
    if len(schedule) != 24:
        raise bench.BenchmarkError(
            f"p{perf} frozen schedule has {len(schedule)} trials instead of 24"
        )
    if [spec.sequence for spec in schedule] != list(range(1, 25)):
        raise bench.BenchmarkError("frozen schedule sequence is not contiguous")
    for cell in lane_cells(perf):
        selected = [spec for spec in schedule if spec.cell == cell]
        warmups = [spec for spec in selected if spec.kind == "warmup"]
        measured = [spec for spec in selected if spec.kind == "measured"]
        if len(warmups) != WARMUPS or len(measured) != MEASURED:
            raise bench.BenchmarkError(
                f"{cell.key} does not have one warmup and five measured trials"
            )
        if [spec.sample for spec in measured] != list(range(1, MEASURED + 1)):
            raise bench.BenchmarkError(f"{cell.key} measured samples are out of order")


def schedule_record(spec: TrialSpec) -> dict[str, Any]:
    return {
        "sequence": spec.sequence,
        "lane": f"p{spec.cell.perf}",
        "phase": spec.phase,
        "kind": spec.kind,
        "sample": spec.sample,
        "cell": spec.cell.key,
        "arch": spec.cell.arch,
        "block_io_mode": spec.cell.policy,
        "perf_counters": spec.cell.perf,
        "trial_directory": str(spec.cell.directory / spec.trial.directory_name),
    }


def finish_ledger(ledger: dict[str, Any]) -> None:
    ledger["finished_at"] = bench.utc_now()
    ledger["monotonic_finished_ns"] = time.monotonic_ns()


def artifact_flag(cell: Cell) -> str:
    return f"{cell.arch}_{cell.policy.replace('-', '_')}_kernel"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Run one fixed IO0-A 8C/8G auto versus force-sync lane. "
            "Every cell uses one warmup and five measured fresh guests."
        )
    )
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--perf-counters", type=int, choices=(0, 1))
    for arch in ("rv", "la"):
        for policy in POLICIES:
            option = f"--{arch}-{policy}-kernel"
            destination = f"{arch}_{policy.replace('-', '_')}_kernel"
            parser.add_argument(option, dest=destination, type=Path)
    parser.add_argument("--rv-disk", type=Path)
    parser.add_argument("--la-disk", type=Path)
    parser.add_argument("--output-dir", type=Path)
    parser.add_argument("--run-id")
    parser.add_argument("--timeout", type=float, default=240.0)
    parser.add_argument("--image-size", default="64M")
    args = parser.parse_args()
    if args.self_test:
        return args
    if args.perf_counters is None:
        parser.error("--perf-counters is required")
    missing = [
        f"--{cell.arch}-{cell.policy}-kernel"
        for cell in lane_cells(args.perf_counters)
        if getattr(args, artifact_flag(cell)) is None
    ]
    for option, value in (
        ("--rv-disk", args.rv_disk),
        ("--la-disk", args.la_disk),
        ("--output-dir", args.output_dir),
    ):
        if value is None:
            missing.append(option)
    if missing:
        parser.error("missing required arguments: " + ", ".join(missing))
    if args.timeout <= 0:
        parser.error("--timeout must be positive")
    if not bench.IMAGE_SIZE_RE.fullmatch(args.image_size):
        parser.error("--image-size must be a positive size ending in M or G")
    if args.run_id is not None and not bench.TOKEN_RE.fullmatch(args.run_id):
        parser.error("--run-id must match [A-Za-z0-9._-]+")
    return args


def resolve_inputs(
    args: argparse.Namespace,
) -> tuple[dict[Cell, Path], dict[str, Path], Path]:
    cells = lane_cells(args.perf_counters)
    artifacts = {cell: getattr(args, artifact_flag(cell)).resolve() for cell in cells}
    disks = {"rv": args.rv_disk.resolve(), "la": args.la_disk.resolve()}
    output_dir = args.output_dir.resolve()
    if output_dir.exists():
        raise bench.BenchmarkError(f"output directory already exists: {output_dir}")
    missing = [
        str(path)
        for path in (*artifacts.values(), *disks.values())
        if not path.is_file()
    ]
    if missing:
        raise bench.BenchmarkError(f"required input files are missing: {missing!r}")
    if len(set(artifacts.values())) != len(artifacts):
        raise bench.BenchmarkError("the four kernel artifact paths must be distinct")
    if disks["rv"] == disks["la"]:
        raise bench.BenchmarkError("RISC-V and LoongArch disk paths must be distinct")
    return artifacts, disks, output_dir


def selected_input_metadata(
    *,
    artifacts: dict[Cell, Path],
    disks: dict[str, Path],
    compilers: dict[str, Path],
) -> dict[str, dict[str, Any]]:
    selected = {
        f"artifact:{cell.key}": path
        for cell, path in sorted(artifacts.items(), key=lambda item: item[0])
    }
    selected.update(
        {
            "disk:rv": disks["rv"],
            "disk:la": disks["la"],
            "controller": CONTROLLER_SOURCE,
            "base_runner": bench.RUNNER_SOURCE,
            "guest_template": bench.GUEST_TEMPLATE,
            "timer_source": bench.TIMER_SOURCE,
            "os_makefile": bench.OS_MAKEFILE,
            "timer_compiler:rv": compilers["rv"],
            "timer_compiler:la": compilers["la"],
        }
    )
    return {name: bench.file_metadata(path) for name, path in selected.items()}


def git_status() -> str:
    completed = bench.run_capture(
        [bench.require_command("git"), "status", "--short"], cwd=REPO_ROOT
    )
    if completed.returncode != 0:
        raise bench.BenchmarkError("cannot inspect tracked worktree status")
    return completed.stdout


def git_status_is_stable(before: str, after: str | None) -> bool:
    return before == "" and after == before


def percent_text(value: Decimal) -> str:
    return f"{value:.{PERCENT_DECIMAL_PLACES}f}"


def decimal_text(value: Decimal) -> str:
    return format(value, "f")


def median_decimal(values: list[Decimal]) -> Decimal:
    ordered = sorted(values)
    count = len(ordered)
    if count == 0:
        raise bench.BenchmarkError("cannot compute a median from no values")
    if count % 2:
        return ordered[count // 2]
    return (ordered[count // 2 - 1] + ordered[count // 2]) / 2


def measured_samples_by_round(
    samples: list[dict[str, Any]],
) -> dict[int, dict[str, Any]]:
    measured: dict[int, dict[str, Any]] = {}
    for sample in samples:
        if sample["identity"]["kind"] != "measured" or not sample["valid"]:
            continue
        measured[int(sample["identity"]["sample"])] = sample
    return measured


def elapsed_ns_by_round(samples: list[dict[str, Any]]) -> dict[int, int]:
    return {
        round_number: int(sample["guest_result"]["elapsed_ns"])
        for round_number, sample in measured_samples_by_round(samples).items()
    }


def signed_uplift_percent(reference: Decimal, candidate: Decimal) -> Decimal | None:
    if reference <= 0:
        return None
    return (reference - candidate) * Decimal(100) / reference


def absolute_difference_percent(
    reference: Decimal, candidate: Decimal
) -> Decimal | None:
    if reference == 0:
        return Decimal(0) if candidate == 0 else None
    return abs(reference - candidate) * Decimal(100) / reference


def cell_time_statistics(samples: list[dict[str, Any]]) -> dict[str, Any]:
    elapsed = elapsed_ns_by_round(samples)
    expected_rounds = list(range(1, MEASURED + 1))
    result: dict[str, Any] = {
        "complete": sorted(elapsed) == expected_rounds,
        "measured_rounds": sorted(elapsed),
        "elapsed_ns_by_round": {
            str(round_number): str(value)
            for round_number, value in sorted(elapsed.items())
        },
        "median_elapsed_ns": None,
        "min_elapsed_ns": None,
        "max_elapsed_ns": None,
        "spread_percent": None,
        "noise_within_threshold": None,
    }
    if not result["complete"]:
        return result

    values = [Decimal(elapsed[round_number]) for round_number in expected_rounds]
    median = median_decimal(values)
    minimum = min(values)
    maximum = max(values)
    if median <= 0:
        spread = None
    else:
        spread = (maximum - minimum) * Decimal(100) / median
    result.update(
        {
            "median_elapsed_ns": decimal_text(median),
            "min_elapsed_ns": decimal_text(minimum),
            "max_elapsed_ns": decimal_text(maximum),
            "spread_percent": percent_text(spread) if spread is not None else None,
            "noise_within_threshold": bool(
                spread is not None and spread <= NOISE_THRESHOLD_PERCENT
            ),
        }
    )
    return result


def architecture_time_comparison(
    *, arch: str, perf: int, samples: dict[Cell, list[dict[str, Any]]]
) -> dict[str, Any]:
    auto = elapsed_ns_by_round(samples[Cell(arch, "auto", perf)])
    force_sync = elapsed_ns_by_round(samples[Cell(arch, "force-sync", perf)])
    expected_rounds = list(range(1, MEASURED + 1))
    complete = sorted(auto) == expected_rounds and sorted(force_sync) == expected_rounds
    result: dict[str, Any] = {
        "architecture": arch,
        "complete": complete,
        "uplift_basis": "(auto-force-sync)/auto*100",
        "pairing_basis": "same measured round",
        "auto_median_elapsed_ns": None,
        "force_sync_median_elapsed_ns": None,
        "median_uplift_percent": None,
        "paired_rounds": [],
        "paired_uplift_median_percent": None,
    }
    if not complete:
        return result

    auto_values = [Decimal(auto[round_number]) for round_number in expected_rounds]
    force_sync_values = [
        Decimal(force_sync[round_number]) for round_number in expected_rounds
    ]
    auto_median = median_decimal(auto_values)
    force_sync_median = median_decimal(force_sync_values)
    median_uplift = signed_uplift_percent(auto_median, force_sync_median)
    paired_uplifts: list[Decimal] = []
    paired_rounds: list[dict[str, Any]] = []
    for round_number, auto_value, force_sync_value in zip(
        expected_rounds, auto_values, force_sync_values, strict=True
    ):
        uplift = signed_uplift_percent(auto_value, force_sync_value)
        if uplift is not None:
            paired_uplifts.append(uplift)
        paired_rounds.append(
            {
                "round": round_number,
                "auto_elapsed_ns": decimal_text(auto_value),
                "force_sync_elapsed_ns": decimal_text(force_sync_value),
                "uplift_percent": (
                    percent_text(uplift) if uplift is not None else None
                ),
            }
        )
    paired_median = (
        median_decimal(paired_uplifts)
        if len(paired_uplifts) == len(expected_rounds)
        else None
    )
    result.update(
        {
            "auto_median_elapsed_ns": decimal_text(auto_median),
            "force_sync_median_elapsed_ns": decimal_text(force_sync_median),
            "median_uplift_percent": (
                percent_text(median_uplift) if median_uplift is not None else None
            ),
            "paired_rounds": paired_rounds,
            "paired_uplift_median_percent": (
                percent_text(paired_median) if paired_median is not None else None
            ),
        }
    )
    return result


def noise_verdict(*, perf: int, cells: dict[str, dict[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {
        "applicable": perf == 0,
        "spread_basis": "(max-min)/median*100",
        "threshold_percent": percent_text(NOISE_THRESHOLD_PERCENT),
        "verdict": "NOT_APPLICABLE_CAUSAL_LANE" if perf == 1 else "PENDING",
        "rerun_required": None,
        "cells_above_threshold": [],
    }
    if perf != 0 or not all(cell["complete"] for cell in cells.values()):
        return result
    above = [
        key for key, cell in cells.items() if not bool(cell["noise_within_threshold"])
    ]
    result.update(
        {
            "verdict": "RERUN_REQUIRED" if above else "ACCEPTED",
            "rerun_required": bool(above),
            "cells_above_threshold": above,
        }
    )
    return result


def perf_delta_by_round(samples: list[dict[str, Any]], key: str) -> dict[int, int]:
    result: dict[int, int] = {}
    for round_number, sample in measured_samples_by_round(samples).items():
        selected = sample["guest_result"]["perf_snapshots"]["selected_deltas"]
        if key in selected:
            result[round_number] = int(selected[key])
    return result


def workload_metric_comparison(
    *,
    key: str,
    auto_samples: list[dict[str, Any]],
    force_sync_samples: list[dict[str, Any]],
) -> dict[str, Any]:
    auto = perf_delta_by_round(auto_samples, key)
    force_sync = perf_delta_by_round(force_sync_samples, key)
    expected_rounds = list(range(1, MEASURED + 1))
    complete = sorted(auto) == expected_rounds and sorted(force_sync) == expected_rounds
    result: dict[str, Any] = {
        "counter": key,
        "complete": complete,
        "comparison_basis": "absolute(auto-force-sync)/auto*100",
        "threshold_percent": percent_text(WORKLOAD_COMPARABILITY_THRESHOLD_PERCENT),
        "paired_rounds": [],
        "all_rounds_within_threshold": None,
        "auto_median": None,
        "force_sync_median": None,
        "median_difference_percent": None,
        "median_within_threshold": None,
        "comparable": None,
    }
    if not complete:
        return result

    paired_rounds: list[dict[str, Any]] = []
    round_verdicts: list[bool] = []
    for round_number in expected_rounds:
        auto_value = Decimal(auto[round_number])
        force_sync_value = Decimal(force_sync[round_number])
        difference = absolute_difference_percent(auto_value, force_sync_value)
        within_threshold = bool(
            difference is not None
            and difference <= WORKLOAD_COMPARABILITY_THRESHOLD_PERCENT
        )
        round_verdicts.append(within_threshold)
        paired_rounds.append(
            {
                "round": round_number,
                "auto": decimal_text(auto_value),
                "force_sync": decimal_text(force_sync_value),
                "difference_percent": (
                    percent_text(difference) if difference is not None else None
                ),
                "within_threshold": within_threshold,
            }
        )
    auto_median = median_decimal([Decimal(auto[item]) for item in expected_rounds])
    force_sync_median = median_decimal(
        [Decimal(force_sync[item]) for item in expected_rounds]
    )
    median_difference = absolute_difference_percent(auto_median, force_sync_median)
    median_within_threshold = bool(
        median_difference is not None
        and median_difference <= WORKLOAD_COMPARABILITY_THRESHOLD_PERCENT
    )
    all_rounds_within_threshold = all(round_verdicts)
    result.update(
        {
            "paired_rounds": paired_rounds,
            "all_rounds_within_threshold": all_rounds_within_threshold,
            "auto_median": decimal_text(auto_median),
            "force_sync_median": decimal_text(force_sync_median),
            "median_difference_percent": (
                percent_text(median_difference)
                if median_difference is not None
                else None
            ),
            "median_within_threshold": median_within_threshold,
            "comparable": all_rounds_within_threshold and median_within_threshold,
        }
    )
    return result


def workload_comparability(
    *, perf: int, samples: dict[Cell, list[dict[str, Any]]]
) -> dict[str, Any]:
    if perf != 1:
        return {
            "applicable": False,
            "verdict": "NOT_APPLICABLE_WALL_LANE",
            "complete": True,
            "comparable": None,
            "acceptance_rule": None,
            "architectures": {},
        }
    architectures: dict[str, dict[str, Any]] = {}
    for arch in ("rv", "la"):
        auto_samples = samples[Cell(arch, "auto", perf)]
        force_sync_samples = samples[Cell(arch, "force-sync", perf)]
        metrics = {
            key: workload_metric_comparison(
                key=key,
                auto_samples=auto_samples,
                force_sync_samples=force_sync_samples,
            )
            for key in (
                "block_cache_device_read_submit",
                "block_cache_device_read_blocks",
            )
        }
        complete = all(metric["complete"] for metric in metrics.values())
        comparable = (
            all(bool(metric["comparable"]) for metric in metrics.values())
            if complete
            else None
        )
        architectures[arch] = {
            "complete": complete,
            "comparable": comparable,
            "metrics": metrics,
        }
    complete = all(result["complete"] for result in architectures.values())
    comparable = (
        all(bool(result["comparable"]) for result in architectures.values())
        if complete
        else None
    )
    return {
        "applicable": True,
        "acceptance_rule": (
            "every same-round difference and the median-count difference for both "
            "read-submit and read-block counters must be <=2%"
        ),
        "verdict": (
            "COMPARABLE" if comparable else "NOT_COMPARABLE" if complete else "PENDING"
        ),
        "complete": complete,
        "comparable": comparable,
        "architectures": architectures,
    }


def derive_decision_verdict(
    *,
    perf: int,
    lane_valid: bool | None,
    noise: dict[str, Any],
    workload: dict[str, Any],
) -> str | None:
    if lane_valid is None:
        return None
    if not lane_valid:
        return "INVALID"
    if perf == 0:
        if noise["verdict"] == "ACCEPTED":
            return "ACCEPTED"
        if noise["verdict"] == "RERUN_REQUIRED":
            return "RERUN_REQUIRED"
        return "INCONCLUSIVE"
    if workload["verdict"] == "COMPARABLE":
        return "ACCEPTED"
    return "INCONCLUSIVE"


def lane_statistics(
    *,
    perf: int,
    samples: dict[Cell, list[dict[str, Any]]],
    lane_valid: bool | None,
) -> dict[str, Any]:
    cells = {
        cell.key: cell_time_statistics(cell_samples)
        for cell, cell_samples in samples.items()
    }
    comparisons = {
        arch: architecture_time_comparison(arch=arch, perf=perf, samples=samples)
        for arch in ("rv", "la")
    }
    noise = noise_verdict(perf=perf, cells=cells)
    workload = workload_comparability(perf=perf, samples=samples)
    decision = derive_decision_verdict(
        perf=perf,
        lane_valid=lane_valid,
        noise=noise,
        workload=workload,
    )
    complete = bool(
        all(cell["complete"] for cell in cells.values())
        and all(comparison["complete"] for comparison in comparisons.values())
        and workload["complete"]
    )
    return {
        "schema_version": 1,
        "updated_at": bench.utc_now(),
        "lane": f"p{perf}",
        "formal_wall_time_lane": perf == 0,
        "complete": complete,
        "lane_valid": lane_valid,
        "decision_verdict": decision,
        "cells": cells,
        "architecture_time_comparisons": comparisons,
        "noise": noise,
        "workload_comparability": workload,
    }


def write_lane_statistics(
    *,
    output_dir: Path,
    perf: int,
    samples: dict[Cell, list[dict[str, Any]]],
    lane_valid: bool | None,
) -> dict[str, Any]:
    statistics = lane_statistics(perf=perf, samples=samples, lane_valid=lane_valid)
    bench.write_json(output_dir / "statistics.json", statistics)
    return statistics


def cell_aggregate(
    cell: Cell,
    samples: list[dict[str, Any]],
    *,
    lane_valid: bool | None,
) -> dict[str, Any]:
    result = bench.aggregate(samples, WARMUPS, MEASURED)
    result.update(
        {
            "architecture": cell.arch,
            "block_io_mode": cell.policy,
            "perf_counters": cell.perf,
            "smp": SMP,
            "mem": MEM,
            "lane_valid": lane_valid,
        }
    )
    result["run_valid"] = (
        None
        if lane_valid is None
        else bool(lane_valid and result["all_required_samples_valid"])
    )
    if not result["run_valid"]:
        result["goal_met"] = None
    return result


def write_cell_aggregates(
    *,
    output_dir: Path,
    samples: dict[Cell, list[dict[str, Any]]],
    lane_valid: bool | None,
) -> None:
    for cell, cell_samples in samples.items():
        bench.write_json(
            output_dir / cell.directory / "aggregate.json",
            cell_aggregate(cell, cell_samples, lane_valid=lane_valid),
        )


def sequence_audit(
    expected: list[TrialSpec], executed: list[dict[str, Any]]
) -> dict[str, Any]:
    expected_labels = [spec.label for spec in expected]
    executed_labels = [entry["label"] for entry in executed]
    prefix_valid = executed_labels == expected_labels[: len(executed_labels)]
    nonoverlap = all(
        previous["monotonic_finished_ns"] <= current["monotonic_started_ns"]
        for previous, current in itertools.pairwise(executed)
    )
    return {
        "expected_trials": len(expected_labels),
        "executed_trials": len(executed_labels),
        "expected_order": expected_labels,
        "executed_order": executed_labels,
        "prefix_valid": prefix_valid,
        "complete": prefix_valid and executed_labels == expected_labels,
        "timestamps_nonoverlapping": nonoverlap,
    }


def prepare_host() -> tuple[dict[str, Path], dict[str, str], dict[str, str]]:
    compilers = {arch: bench.compiler_for(arch) for arch in ("rv", "la")}
    compiler_versions = {
        arch: bench.version_line([str(compiler), "--version"])
        for arch, compiler in compilers.items()
    }
    qemu_commands = {
        "rv": bench.require_command("qemu-system-riscv64"),
        "la": bench.require_command("qemu-system-loongarch64"),
    }
    qemu_versions = {
        arch: bench.version_line([command, "--version"])
        for arch, command in qemu_commands.items()
    }
    for command in ("qemu-img", "mkfs.ext4", "truncate"):
        bench.require_command(command)
    return compilers, compiler_versions, qemu_versions


def default_run_id(perf: int) -> str:
    timestamp = datetime.now(timezone.utc).strftime("%Y%m%dt%H%M%Sz")
    return f"io0a-p{perf}-{timestamp}-{os.getpid()}"


def run_lane_locked(
    args: argparse.Namespace,
    *,
    lock_info: dict[str, Any],
    qemu_preflight: dict[str, Any],
) -> int:
    artifacts, disks, output_dir = resolve_inputs(args)
    perf = args.perf_counters
    schedule = frozen_schedule(perf)
    run_id = args.run_id or default_run_id(perf)
    bench.validate_token("run_id", run_id)
    compilers, compiler_versions, qemu_versions = prepare_host()
    metadata_before = selected_input_metadata(
        artifacts=artifacts, disks=disks, compilers=compilers
    )
    git_head_before = bench.current_git_head()
    git_status_before = git_status()
    if git_status_before != "":
        raise bench.BenchmarkError(
            "formal IO0-A lane requires an empty git status before output creation"
        )

    output_dir.mkdir(parents=True)
    for arch in ("rv", "la"):
        (output_dir / "setup" / arch).mkdir(parents=True)
    cells = lane_cells(perf)
    for cell in cells:
        (output_dir / cell.directory).mkdir(parents=True)

    manifest: dict[str, Any] = {
        "schema_version": 1,
        "unit": "IO0-A",
        "run_id": run_id,
        "lane": f"p{perf}",
        "started_at": bench.utc_now(),
        "git_head": git_head_before,
        "git_status_before": git_status_before,
        "fixed_contract": {
            "smp": SMP,
            "mem": MEM,
            "perf_counters": perf,
            "warmups_per_cell": WARMUPS,
            "measured_per_cell": MEASURED,
            "qemu_concurrency": 1,
            "qemu_concurrency_enforcement": (
                "repository directory flock plus foreign-QEMU preflight"
            ),
        },
        "controller_lock": lock_info,
        "foreign_qemu_preflight": qemu_preflight,
        "artifact_provenance": {
            "built_by_controller": False,
            "runtime_feature_identity_verified": False,
            "runtime_feature_identity_verification_method": (
                "exact guest policy/perf boot marker on every required trial"
            ),
        },
        "artifacts": {
            cell.key: str(path)
            for cell, path in sorted(artifacts.items(), key=lambda item: item[0])
        },
        "disks": {arch: str(path) for arch, path in disks.items()},
        "input_metadata_before": metadata_before,
        "timer_compiler_versions": compiler_versions,
        "qemu_versions": qemu_versions,
        "timeout_seconds": args.timeout,
        "image_size": args.image_size,
        "host_load_before": bench.host_load_snapshot(),
        "expected_schedule": [schedule_record(spec) for spec in schedule],
        "executed_trials": [],
    }
    bench.write_json(output_dir / "manifest.json", manifest)

    samples: dict[Cell, list[dict[str, Any]]] = {cell: [] for cell in cells}
    write_cell_aggregates(output_dir=output_dir, samples=samples, lane_valid=None)
    write_lane_statistics(
        output_dir=output_dir,
        perf=perf,
        samples=samples,
        lane_valid=None,
    )
    controller_root: Path | None = None
    timer_binaries: dict[str, Path] = {}
    executed: list[dict[str, Any]] = []
    failure = False
    interrupted = False
    failure_reason: str | None = None

    try:
        controller_root = Path(tempfile.mkdtemp(prefix="whusp-g0b-io0a-controller-"))
        for arch in ("rv", "la"):
            timer_root = controller_root / arch
            timer_root.mkdir()
            timer_binaries[arch] = bench.build_timer(
                compilers[arch], timer_root, output_dir / "setup" / arch
            )

        for spec in schedule:
            artifact = artifacts[spec.cell]
            cell_dir = output_dir / spec.cell.directory
            ledger = schedule_record(spec)
            ledger.update(
                {
                    "label": spec.label,
                    "started_at": bench.utc_now(),
                    "monotonic_started_ns": time.monotonic_ns(),
                    "kernel_elf": str(artifact),
                    "test_disk": str(disks[spec.cell.arch]),
                }
            )
            print(
                f"[{ledger['started_at']}] IO0-A p{perf} "
                f"sequence={spec.sequence}/24 {spec.label} start",
                flush=True,
            )
            try:
                sample = bench.run_trial(
                    trial=spec.trial,
                    run_id=run_id,
                    architecture=bench.ARCHITECTURES[spec.cell.arch],
                    smp=SMP,
                    mem=MEM,
                    block_io_mode=spec.cell.policy,
                    perf_counters=perf,
                    kernel=artifact,
                    disk=disks[spec.cell.arch],
                    timer_binary=timer_binaries[spec.cell.arch],
                    image_size=args.image_size,
                    timeout=args.timeout,
                    evidence_dir=cell_dir,
                )
            except KeyboardInterrupt:
                finish_ledger(ledger)
                ledger["valid"] = False
                ledger["interrupted"] = True
                executed.append(ledger)
                manifest["executed_trials"] = executed
                bench.write_json(output_dir / "manifest.json", manifest)
                raise
            except (bench.BenchmarkError, OSError, subprocess.SubprocessError) as error:
                finish_ledger(ledger)
                ledger["valid"] = False
                ledger["controller_error"] = str(error)
                executed.append(ledger)
                manifest["executed_trials"] = executed
                bench.write_json(output_dir / "manifest.json", manifest)
                raise

            finish_ledger(ledger)
            ledger["valid"] = bool(sample["valid"])
            ledger["sample_started_at"] = sample.get("started_at")
            ledger["sample_finished_at"] = sample.get("finished_at")
            ledger["elapsed_seconds"] = sample.get("guest_result", {}).get(
                "elapsed_seconds"
            )
            ledger["errors"] = sample.get("errors", [])
            executed.append(ledger)
            samples[spec.cell].append(sample)
            write_cell_aggregates(
                output_dir=output_dir, samples=samples, lane_valid=None
            )
            write_lane_statistics(
                output_dir=output_dir,
                perf=perf,
                samples=samples,
                lane_valid=None,
            )
            manifest["executed_trials"] = executed
            manifest["sequence_audit"] = sequence_audit(schedule, executed)
            bench.write_json(output_dir / "manifest.json", manifest)
            print(
                f"[{ledger['finished_at']}] IO0-A p{perf} "
                f"sequence={spec.sequence}/24 {spec.label} "
                f"valid={sample['valid']} elapsed_s={ledger['elapsed_seconds']}",
                flush=True,
            )
            if not sample["valid"]:
                failure = True
                failure_reason = (
                    f"trial {spec.sequence} {spec.label} failed; lane stopped"
                )
                break
    except KeyboardInterrupt:
        interrupted = True
        failure = True
        failure_reason = "controller interrupted"
    except (bench.BenchmarkError, OSError, subprocess.SubprocessError) as error:
        failure = True
        failure_reason = str(error)
        print(f"IO0-A controller error: {error}", file=sys.stderr, flush=True)
    finally:
        if controller_root is None:
            controller_cleanup = True
        else:
            try:
                controller_cleanup = bench.remove_owned_temp(controller_root)
            except (bench.BenchmarkError, OSError) as error:
                controller_cleanup = False
                manifest["controller_cleanup_error"] = str(error)
                failure = True

        manifest["controller_temp_root"] = (
            str(controller_root) if controller_root is not None else None
        )
        manifest["controller_cleanup"] = controller_cleanup
        sequence_result = sequence_audit(schedule, executed)
        manifest["sequence_audit"] = sequence_result
        if (
            not sequence_result["prefix_valid"]
            or not sequence_result["timestamps_nonoverlapping"]
        ):
            failure = True

        try:
            metadata_after = selected_input_metadata(
                artifacts=artifacts, disks=disks, compilers=compilers
            )
        except OSError as error:
            metadata_after = None
            manifest["input_metadata_error"] = str(error)
            failure = True
        manifest["input_metadata_after"] = metadata_after
        manifest["input_metadata_stable"] = metadata_after == metadata_before
        if not manifest["input_metadata_stable"]:
            failure = True

        try:
            git_head_after = bench.current_git_head()
        except bench.BenchmarkError as error:
            git_head_after = None
            manifest["git_head_error"] = str(error)
            failure = True
        manifest["git_head_after"] = git_head_after
        manifest["git_head_stable"] = git_head_after == git_head_before
        if not manifest["git_head_stable"]:
            failure = True
        try:
            git_status_after = git_status()
        except bench.BenchmarkError as error:
            git_status_after = None
            manifest["git_status_error"] = str(error)
            failure = True
        manifest["git_status_after"] = git_status_after
        manifest["git_status_stable"] = git_status_is_stable(
            git_status_before, git_status_after
        )
        if not manifest["git_status_stable"]:
            failure = True
        try:
            manifest["host_load_after"] = bench.host_load_snapshot()
        except (OSError, ValueError) as error:
            manifest["host_load_after"] = None
            manifest["host_load_error"] = str(error)
            failure = True

        complete_cells = all(
            bench.aggregate(samples[cell], WARMUPS, MEASURED)[
                "all_required_samples_valid"
            ]
            for cell in cells
        )
        preliminary_statistics = lane_statistics(
            perf=perf,
            samples=samples,
            lane_valid=None,
        )
        lane_valid = bool(
            not failure
            and not interrupted
            and controller_cleanup
            and sequence_result["complete"]
            and sequence_result["timestamps_nonoverlapping"]
            and manifest["input_metadata_stable"]
            and manifest["git_head_stable"]
            and manifest["git_status_stable"]
            and complete_cells
            and preliminary_statistics["complete"]
        )
        manifest["artifact_provenance"]["runtime_feature_identity_verified"] = (
            complete_cells
        )
        manifest["artifact_provenance"]["verification_basis"] = (
            "all required guest policy/perf markers matched the requested lane"
            if complete_cells
            else "not all required guest policy/perf markers were validated"
        )
        write_cell_aggregates(
            output_dir=output_dir,
            samples=samples,
            lane_valid=lane_valid,
        )
        statistics = write_lane_statistics(
            output_dir=output_dir,
            perf=perf,
            samples=samples,
            lane_valid=lane_valid,
        )
        manifest["statistics"] = {
            "path": str(output_dir / "statistics.json"),
            "complete": statistics["complete"],
            "decision_verdict": statistics["decision_verdict"],
            "noise_verdict": statistics["noise"]["verdict"],
            "workload_comparability_verdict": statistics["workload_comparability"][
                "verdict"
            ],
        }
        manifest["finished_at"] = bench.utc_now()
        manifest["interrupted"] = interrupted
        manifest["failure_reason"] = failure_reason
        manifest["all_cells_complete"] = complete_cells
        manifest["data_valid"] = lane_valid
        manifest["run_valid"] = lane_valid
        manifest["decision_verdict"] = statistics["decision_verdict"]
        bench.write_json(output_dir / "manifest.json", manifest)

    if interrupted:
        print(
            f"IO0-A p{perf} DATA_INVALID decision=INVALID run_id={run_id} "
            f"trials={len(executed)} output={output_dir}",
            flush=True,
        )
        return 130
    if not manifest["run_valid"]:
        print(
            f"IO0-A p{perf} DATA_INVALID decision=INVALID run_id={run_id} "
            f"trials={len(executed)} output={output_dir}",
            flush=True,
        )
        return 1
    print(
        f"IO0-A p{perf} DATA_VALID decision={manifest['decision_verdict']} "
        f"run_id={run_id} trials={len(executed)} output={output_dir}",
        flush=True,
    )
    return 0


def run_lane(args: argparse.Namespace) -> int:
    with repository_controller_lock() as lock_info:
        qemu_preflight = require_no_foreign_qemu()
        return run_lane_locked(
            args,
            lock_info=lock_info,
            qemu_preflight=qemu_preflight,
        )


def self_test() -> int:
    if not git_status_is_stable("", ""):
        raise bench.BenchmarkError("clean git status stability was rejected")
    if git_status_is_stable("", " M tracked") or git_status_is_stable(
        " M tracked", " M tracked"
    ):
        raise bench.BenchmarkError("dirty git status stability was accepted")
    expected_labels = {
        0: [
            "p0-W-rv-auto",
            "p0-W-rv-force-sync",
            "p0-W-la-force-sync",
            "p0-W-la-auto",
            "p0-R1-rv-auto",
            "p0-R1-rv-force-sync",
            "p0-R1-la-auto",
            "p0-R1-la-force-sync",
            "p0-R2-la-force-sync",
            "p0-R2-la-auto",
            "p0-R2-rv-force-sync",
            "p0-R2-rv-auto",
            "p0-R3-rv-force-sync",
            "p0-R3-rv-auto",
            "p0-R3-la-force-sync",
            "p0-R3-la-auto",
            "p0-R4-la-auto",
            "p0-R4-la-force-sync",
            "p0-R4-rv-auto",
            "p0-R4-rv-force-sync",
            "p0-R5-rv-auto",
            "p0-R5-rv-force-sync",
            "p0-R5-la-auto",
            "p0-R5-la-force-sync",
        ],
        1: [
            "p1-W-la-auto",
            "p1-W-la-force-sync",
            "p1-W-rv-force-sync",
            "p1-W-rv-auto",
            "p1-R1-la-force-sync",
            "p1-R1-la-auto",
            "p1-R1-rv-force-sync",
            "p1-R1-rv-auto",
            "p1-R2-rv-auto",
            "p1-R2-rv-force-sync",
            "p1-R2-la-auto",
            "p1-R2-la-force-sync",
            "p1-R3-la-auto",
            "p1-R3-la-force-sync",
            "p1-R3-rv-auto",
            "p1-R3-rv-force-sync",
            "p1-R4-rv-force-sync",
            "p1-R4-rv-auto",
            "p1-R4-la-force-sync",
            "p1-R4-la-auto",
            "p1-R5-la-force-sync",
            "p1-R5-la-auto",
            "p1-R5-rv-force-sync",
            "p1-R5-rv-auto",
        ],
    }
    for perf, expected in expected_labels.items():
        labels = [spec.label for spec in frozen_schedule(perf)]
        if labels != expected:
            raise bench.BenchmarkError(f"p{perf} order does not match the plan")

    if not is_qemu_process_name("/usr/bin/qemu-system-riscv64") or not (
        is_qemu_process_name("qemu-system-loo")
    ):
        raise bench.BenchmarkError("QEMU process-name detection missed a valid name")
    if is_qemu_process_name("/usr/bin/python3"):
        raise bench.BenchmarkError(
            "QEMU process-name detection accepted a non-QEMU name"
        )
    with repository_controller_lock():
        try:
            with repository_controller_lock():
                pass
        except bench.BenchmarkError:
            pass
        else:
            raise bench.BenchmarkError("repository controller lock allowed contention")

    synthetic: list[dict[str, Any]] = [
        {
            "identity": {"kind": "warmup", "sample": "1"},
            "valid": True,
            "guest_result": {"elapsed_ns": "2000000000"},
        }
    ]
    synthetic.extend(
        {
            "identity": {"kind": "measured", "sample": str(sample)},
            "valid": True,
            "guest_result": {"elapsed_ns": str(value)},
        }
        for sample, value in enumerate(
            (
                5_000_000_000,
                4_000_000_000,
                3_000_000_000,
                2_000_000_000,
                1_000_000_000,
            ),
            start=1,
        )
    )
    if (
        bench.aggregate(synthetic, WARMUPS, MEASURED)["median_elapsed_ns"]
        != "3000000000"
    ):
        raise bench.BenchmarkError("base aggregate median contract changed")

    def synthetic_sample(
        *,
        kind: str,
        sample: int,
        elapsed_ns: int,
        selected_deltas: dict[str, int] | None = None,
    ) -> dict[str, Any]:
        guest_result: dict[str, Any] = {"elapsed_ns": str(elapsed_ns)}
        if selected_deltas is not None:
            guest_result["perf_snapshots"] = {"selected_deltas": selected_deltas}
        return {
            "identity": {"kind": kind, "sample": str(sample)},
            "valid": True,
            "guest_result": guest_result,
        }

    p0_elapsed = {
        Cell("rv", "auto", 0): 1_000,
        Cell("rv", "force-sync", 0): 900,
        Cell("la", "auto", 0): 2_000,
        Cell("la", "force-sync", 0): 1_900,
    }
    p0_samples = {
        cell: [
            synthetic_sample(kind="warmup", sample=1, elapsed_ns=value),
            *[
                synthetic_sample(kind="measured", sample=sample, elapsed_ns=value)
                for sample in range(1, MEASURED + 1)
            ],
        ]
        for cell, value in p0_elapsed.items()
    }
    p0_statistics = lane_statistics(perf=0, samples=p0_samples, lane_valid=True)
    rv_comparison = p0_statistics["architecture_time_comparisons"]["rv"]
    la_comparison = p0_statistics["architecture_time_comparisons"]["la"]
    if (
        not p0_statistics["complete"]
        or p0_statistics["noise"]["verdict"] != "ACCEPTED"
        or p0_statistics["decision_verdict"] != "ACCEPTED"
        or rv_comparison["median_uplift_percent"] != "10.000000"
        or rv_comparison["paired_uplift_median_percent"] != "10.000000"
        or la_comparison["median_uplift_percent"] != "5.000000"
    ):
        raise bench.BenchmarkError("wall-time statistics contract changed")
    noisy_sample = p0_samples[Cell("rv", "auto", 0)][1]
    noisy_sample["guest_result"]["elapsed_ns"] = "1200"
    noisy_statistics = lane_statistics(perf=0, samples=p0_samples, lane_valid=True)
    if (
        noisy_statistics["noise"]["verdict"] != "RERUN_REQUIRED"
        or noisy_statistics["decision_verdict"] != "RERUN_REQUIRED"
    ):
        raise bench.BenchmarkError("wall-time noise verdict missed spread above 5%")
    noisy_sample["guest_result"]["elapsed_ns"] = "1000"

    p1_samples: dict[Cell, list[dict[str, Any]]] = {}
    for cell in lane_cells(1):
        auto_submit = 100 if cell.arch == "rv" else 200
        auto_blocks = 1_000 if cell.arch == "rv" else 2_000
        submit = (
            auto_submit if cell.policy == "auto" else auto_submit + auto_submit // 100
        )
        blocks = (
            auto_blocks if cell.policy == "auto" else auto_blocks + auto_blocks // 100
        )
        p1_samples[cell] = [
            synthetic_sample(kind="warmup", sample=1, elapsed_ns=1_000),
            *[
                synthetic_sample(
                    kind="measured",
                    sample=sample,
                    elapsed_ns=1_000,
                    selected_deltas={
                        "block_cache_device_read_submit": submit,
                        "block_cache_device_read_blocks": blocks,
                    },
                )
                for sample in range(1, MEASURED + 1)
            ],
        ]
    p1_statistics = lane_statistics(perf=1, samples=p1_samples, lane_valid=True)
    if (
        p1_statistics["workload_comparability"]["verdict"] != "COMPARABLE"
        or p1_statistics["decision_verdict"] != "ACCEPTED"
    ):
        raise bench.BenchmarkError("p1 workload comparability rejected a 1% difference")
    p1_outlier = p1_samples[Cell("rv", "force-sync", 1)][1]
    p1_outlier["guest_result"]["perf_snapshots"]["selected_deltas"][
        "block_cache_device_read_submit"
    ] = 103
    p1_noncomparable = lane_statistics(perf=1, samples=p1_samples, lane_valid=True)
    if (
        p1_noncomparable["workload_comparability"]["verdict"] != "NOT_COMPARABLE"
        or p1_noncomparable["decision_verdict"] != "INCONCLUSIVE"
    ):
        raise bench.BenchmarkError("p1 workload comparability missed a >2% round")
    invalid_statistics = lane_statistics(perf=1, samples=p1_samples, lane_valid=False)
    if invalid_statistics["decision_verdict"] != "INVALID":
        raise bench.BenchmarkError("invalid data did not produce decision=INVALID")
    print("IO0-A controller self-test PASS")
    return 0


def main() -> int:
    args = parse_args()
    try:
        if args.self_test:
            return self_test()
        return run_lane(args)
    except bench.BenchmarkError as error:
        print(f"IO0-A benchmark failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
