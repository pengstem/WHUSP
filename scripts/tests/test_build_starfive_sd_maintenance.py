from __future__ import annotations

import sys
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT / "scripts"))

import build_starfive_sd_maintenance as maintenance


class BuildStarFiveSdMaintenanceTests(unittest.TestCase):
    def test_list_image_contains_no_recursive_delete(self) -> None:
        script = maintenance.maintenance_entry_script("list")
        self.assertIn('action="list"', script)
        self.assertIn(
            'if [ "$action" = delete ]; then\n            /musl/busybox rm -rf "$entry"',
            script,
        )
        self.assertIn("STARFIVE_SD_MAINTENANCE_FINAL", script)

    def test_delete_is_scoped_to_checked_run_directories(self) -> None:
        script = maintenance.maintenance_entry_script("delete")
        self.assertIn('runs_root="/work/.whusp-buildstorm-runs"', script)
        self.assertIn('"$runs_root"/run-*', script)
        self.assertIn('if [ -L "$entry" ]', script)
        delete_lines = [
            line.strip() for line in script.splitlines() if "rm -rf" in line
        ]
        self.assertEqual(delete_lines, ['/musl/busybox rm -rf "$entry"'])

    def test_unknown_action_is_rejected(self) -> None:
        with self.assertRaises(ValueError):
            maintenance.maintenance_entry_script("everything")


if __name__ == "__main__":
    unittest.main()
