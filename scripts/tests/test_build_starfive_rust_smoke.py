from __future__ import annotations

import sys
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT / "scripts"))

import build_starfive_rust_smoke as starfive_smoke


class StarFiveRustSmokeTests(unittest.TestCase):
    def test_perf_identity_is_explicit_and_defaults_off(self) -> None:
        self.assertEqual(starfive_smoke.identity("hello")["perf"], "0")
        self.assertEqual(
            starfive_smoke.identity("multicrate", perf_counters=1)["perf"], "1"
        )

    def test_perf_identity_reaches_frozen_guest_protocol(self) -> None:
        guest = starfive_smoke.rust_bench.render_guest(
            starfive_smoke.identity(
                "hello", "starfive-hello-cold", perf_counters=1
            ),
            project_storage="tmpfs",
        )

        self.assertIn("PERF='1'", guest)
        self.assertIn("G0_RUST_HELLO_PERF_BEGIN", guest)
        self.assertIn("PERF_PATH=/proc/oskernel/perf", guest)

    def test_entry_clears_perf_snapshots_between_stages(self) -> None:
        entry = starfive_smoke.entry_script()

        self.assertIn("/tmp/g0-rust-hello-perf.before", entry)
        self.assertIn("/tmp/g0-rust-hello-perf.after", entry)

    def test_startup_multiblock_write_probe_check_is_opt_in(self) -> None:
        stable = starfive_smoke.entry_script()
        candidate = starfive_smoke.entry_script(probe_multiblock_write=True)

        self.assertNotIn("STARFIVE_MMC_WRITE_AUTO_CHECK_BEGIN", stable)
        self.assertIn(
            'echo "STARFIVE_MMC_WRITE_AUTO_CHECK_BEGIN expected_blocks=64"', candidate
        )
        self.assertIn("/proc/oskernel/starfive_mmc_max_write_blocks", candidate)
        self.assertNotIn('echo 64 > "$mmc_write_knob"', candidate)
        self.assertIn("STARFIVE_MMC_WRITE_AUTO_CHECK_RESULT ok=true", candidate)


if __name__ == "__main__":
    unittest.main()
