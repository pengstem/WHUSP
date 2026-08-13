from __future__ import annotations

import sys
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT / "scripts"))

import analyze_starfive_rust_smoke as analyzer
import build_starfive_rust_smoke as starfive_smoke
import run_rust_hello_bench as rust_bench


def synthetic_stage(run_id: str, workload: str, *, jh7110: bool = False) -> str:
    identity = starfive_smoke.identity(workload, run_id, perf_counters=1)
    log = rust_bench.synthetic_log(identity, rust_bench.ARCHITECTURES["rv"], 4, "4G")
    lines = log.splitlines()
    enriched: list[str] = []
    point = ""
    sync_keys = {
        "block_io_sync_read_submits",
        "block_io_sync_write_submits",
    }
    required_ordinals = {
        key: ordinal
        for ordinal, key in enumerate(rust_bench.PERF_REQUIRED_KEYS, start=1)
    }
    for line in lines:
        if "G0_RUST_HELLO_PERF_BEGIN" in line:
            point = line.rsplit("point=", 1)[1]
        if jh7110 and point == "after":
            key = line.split(" ", 1)[0]
            if key in sync_keys:
                line = f"{key} {required_ordinals[key]}"
        enriched.append(line)
        if line == "perf_counters_enabled 1":
            for key in analyzer.DIAGNOSTIC_KEYS:
                value = 0
                if jh7110:
                    if key == "jh7110_mmc_single_read_commands":
                        value = 100 if point == "before" else 105
                    elif key == "jh7110_mmc_read_blocks":
                        value = 100 if point == "before" else 105
                    elif key == "jh7110_mmc_read_us":
                        value = 1000 if point == "before" else 1050
                    elif key == "jh7110_mmc_read_max_blocks_per_transfer":
                        value = 1
                enriched.append(f"{key} {value}")
    return "\n".join(enriched) + "\n"


class StarFiveRustSmokeAnalysisTests(unittest.TestCase):
    def test_accepts_three_complete_perf_stages(self) -> None:
        log = "".join(
            synthetic_stage(run_id, workload)
            for run_id, workload in analyzer.WORKLOADS
        )
        log += "FINAL: starfive rust smoke finished (status=0)\n"

        result = analyzer.analyze_text(log)

        self.assertTrue(result["valid"])
        self.assertEqual(set(result["workloads"]), {item[0] for item in analyzer.WORKLOADS})

    def test_rejects_missing_final_marker(self) -> None:
        log = "".join(
            synthetic_stage(run_id, workload)
            for run_id, workload in analyzer.WORKLOADS
        )

        with self.assertRaises(analyzer.AnalysisError):
            analyzer.analyze_text(log)

    def test_accepts_physical_jh7110_accounting_without_virtio_counters(self) -> None:
        log = "".join(
            synthetic_stage(run_id, workload, jh7110=True)
            for run_id, workload in analyzer.WORKLOADS
        )
        log += "FINAL: starfive rust smoke finished (status=0)\n"

        result = analyzer.analyze_text(log)

        self.assertTrue(result["valid"])
        for workload in result["workloads"].values():
            self.assertEqual(workload["storage_driver"], "jh7110-mmc")


if __name__ == "__main__":
    unittest.main()
