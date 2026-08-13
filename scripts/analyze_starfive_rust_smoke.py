#!/usr/bin/env python3
"""Validate and summarize a PERF-enabled StarFive Rust smoke serial log."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any

import build_starfive_rust_smoke as starfive_smoke
import run_rust_hello_bench as rust_bench


WORKLOADS = (
    ("starfive-hello-cold", "hello"),
    ("starfive-hello-warm", "hello"),
    ("starfive-multicrate", "multicrate"),
)
DIAGNOSTIC_KEYS = (
    "rv_user_trap_entries",
    "rv_user_timer_interrupts",
    "rv_sbi_set_timer_calls",
    "rv_user_fp_save_calls",
    "rv_user_fp_restore_calls",
    "rv_user_fp_lazy_init_traps",
    "jh7110_mmc_single_read_commands",
    "jh7110_mmc_multi_read_commands",
    "jh7110_mmc_single_write_commands",
    "jh7110_mmc_multi_write_commands",
    "jh7110_mmc_read_blocks",
    "jh7110_mmc_write_blocks",
    "jh7110_mmc_read_us",
    "jh7110_mmc_write_us",
    "jh7110_mmc_read_retries",
    "jh7110_mmc_write_retries",
    "jh7110_mmc_read_failures",
    "jh7110_mmc_write_failures",
    "jh7110_mmc_read_max_blocks_per_transfer",
    "jh7110_mmc_write_max_blocks_per_transfer",
)


class AnalysisError(RuntimeError):
    pass


def _matches(
    lines: list[str], pattern: Any, run_id: str
) -> list[tuple[int, Any]]:
    found: list[tuple[int, Any]] = []
    for index, line in enumerate(lines):
        match = pattern.fullmatch(line)
        if match is not None and match.group("run_id") == run_id:
            found.append((index, match))
    return found


def _one(
    matches: list[tuple[int, Any]], label: str, errors: list[str]
) -> tuple[int, Any] | None:
    if len(matches) != 1:
        errors.append(f"{label}: expected one marker, found {len(matches)}")
        return None
    return matches[0]


def analyze_text(log: str) -> dict[str, Any]:
    lines = rust_bench.normalized_lines(log)
    errors: list[str] = []
    workloads: dict[str, Any] = {}
    if "\ufffd" in log:
        errors.append("serial log contains a UTF-8 replacement character")
    if rust_bench.PANIC_RE.search(rust_bench.ANSI_RE.sub("", log)):
        errors.append("kernel panic/assertion text found")
    if rust_bench.FAIL_RE.search("\n".join(lines)):
        errors.append("guest emitted a G0_RUST_HELLO_FAIL marker")
    if lines.count("FINAL: starfive rust smoke finished (status=0)") != 1:
        errors.append("missing unique successful StarFive Rust smoke final marker")

    for run_id, workload in WORKLOADS:
        identity = starfive_smoke.identity(
            workload, run_id, perf_counters=1
        )
        start = _one(_matches(lines, rust_bench.START_RE, run_id), f"{run_id} START", errors)
        result = _one(
            _matches(lines, rust_bench.RESULT_RE, run_id), f"{run_id} RESULT", errors
        )
        passed = _one(_matches(lines, rust_bench.PASS_RE, run_id), f"{run_id} PASS", errors)
        boundaries: dict[tuple[str, str], tuple[int, Any] | None] = {}
        for point in ("before", "after"):
            for kind, pattern in (
                ("begin", rust_bench.PERF_BEGIN_RE),
                ("end", rust_bench.PERF_END_RE),
            ):
                matches = [
                    item
                    for item in _matches(lines, pattern, run_id)
                    if item[1].group("point") == point
                ]
                boundaries[(point, kind)] = _one(
                    matches, f"{run_id} perf {point} {kind}", errors
                )

        markers = (
            start,
            boundaries[("before", "begin")],
            boundaries[("before", "end")],
            boundaries[("after", "begin")],
            boundaries[("after", "end")],
            result,
            passed,
        )
        if any(marker is None for marker in markers):
            continue
        marker_values = [marker for marker in markers if marker is not None]
        indices = [marker[0] for marker in marker_values]
        if indices != sorted(indices) or len(set(indices)) != len(indices):
            errors.append(f"{run_id}: workload/perf markers are out of order")
            continue

        for label, marker in (
            ("START", start),
            ("RESULT", result),
            ("PASS", passed),
            ("PERF before BEGIN", boundaries[("before", "begin")]),
            ("PERF before END", boundaries[("before", "end")]),
            ("PERF after BEGIN", boundaries[("after", "begin")]),
            ("PERF after END", boundaries[("after", "end")]),
        ):
            assert marker is not None
            errors.extend(
                rust_bench.marker_identity_errors(marker[1].groupdict(), identity, f"{run_id} {label}")
            )

        before_begin = boundaries[("before", "begin")]
        before_end = boundaries[("before", "end")]
        after_begin = boundaries[("after", "begin")]
        after_end = boundaries[("after", "end")]
        assert before_begin is not None and before_end is not None
        assert after_begin is not None and after_end is not None
        before, before_errors = rust_bench.parse_perf_snapshot(
            lines[before_begin[0] + 1 : before_end[0]], label=f"{run_id} before"
        )
        after, after_errors = rust_bench.parse_perf_snapshot(
            lines[after_begin[0] + 1 : after_end[0]], label=f"{run_id} after"
        )
        errors.extend(before_errors)
        errors.extend(after_errors)
        selected_deltas, pair_errors = rust_bench.validate_perf_snapshot_pair(
            before, after, block_io_mode="force-sync"
        )
        jh7110_active = any(
            after["values"].get(key, 0) > 0
            for key in (
                "jh7110_mmc_single_read_commands",
                "jh7110_mmc_multi_read_commands",
                "jh7110_mmc_single_write_commands",
                "jh7110_mmc_multi_write_commands",
            )
        )
        if jh7110_active:
            # `block_io_*` routes only VirtIOBlock requests. The physical
            # VisionFive 2 root disk uses Jh7110MmcBlock and has its own
            # command/block/error accounting below the shared block cache.
            pair_errors = [
                error
                for error in pair_errors
                if error
                != "force-sync mode requires block_io_sync_read_submits delta > 0"
                and not error.startswith("read submit accounting mismatch:")
                and not error.startswith("write submit accounting mismatch:")
            ]
        elif run_id == "starfive-hello-warm":
            pair_errors = [
                error
                for error in pair_errors
                if error
                != "force-sync mode requires block_io_sync_read_submits delta > 0"
            ]
        errors.extend(f"{run_id}: {error}" for error in pair_errors)

        missing = [
            key
            for key in DIAGNOSTIC_KEYS
            if key not in before["values"] or key not in after["values"]
        ]
        if missing:
            errors.append(f"{run_id}: missing diagnostic keys: {missing!r}")
            continue
        deltas = {
            key: after["values"][key] - before["values"][key]
            for key in DIAGNOSTIC_KEYS
        }
        backwards = {key: value for key, value in deltas.items() if value < 0}
        if backwards:
            errors.append(f"{run_id}: counters moved backwards: {backwards!r}")
        if jh7110_active:
            nonblocking_activity = {
                key: selected_deltas[key]
                for key in (
                    "block_io_nb_read_submits",
                    "block_io_nb_write_submits",
                    "block_io_nb_read_waits",
                    "block_io_nb_write_waits",
                    "block_io_nb_read_completions",
                    "block_io_nb_write_completions",
                )
                if selected_deltas.get(key, 0) != 0
            }
            if nonblocking_activity:
                errors.append(
                    f"{run_id}: JH7110 force-sync path used nonblocking IO: "
                    f"{nonblocking_activity!r}"
                )
            if deltas["jh7110_mmc_read_failures"] != 0:
                errors.append(f"{run_id}: JH7110 read transfer failed")
            if deltas["jh7110_mmc_write_failures"] != 0:
                errors.append(f"{run_id}: JH7110 write transfer failed")
            if (
                selected_deltas.get("block_cache_device_read_submit", 0) > 0
                and deltas["jh7110_mmc_single_read_commands"]
                + deltas["jh7110_mmc_multi_read_commands"]
                == 0
            ):
                errors.append(
                    f"{run_id}: block cache reads did not reach the JH7110 MMC driver"
                )
        assert result is not None
        workloads[run_id] = {
            "workload": workload,
            "elapsed_seconds": str(int(result[1].group("elapsed_ns")) / 1_000_000_000),
            "storage_driver": "jh7110-mmc" if jh7110_active else "virtio-blk",
            "diagnostic_deltas": deltas,
            "diagnostic_after": {
                key: after["values"][key] for key in DIAGNOSTIC_KEYS
            },
        }

    if errors:
        raise AnalysisError("; ".join(errors))
    return {"valid": True, "workloads": workloads}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("log", type=Path)
    parser.add_argument("--json-output", type=Path)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    result = analyze_text(args.log.read_text(encoding="utf-8", errors="replace"))
    rendered = json.dumps(result, indent=2, sort_keys=True) + "\n"
    if args.json_output is not None:
        args.json_output.write_text(rendered, encoding="utf-8")
    print(rendered, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
