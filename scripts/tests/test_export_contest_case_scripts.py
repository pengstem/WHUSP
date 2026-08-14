import importlib.util
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
EXPORTER_PATH = REPO_ROOT / "scripts" / "export_contest_case_scripts.py"
SPEC = importlib.util.spec_from_file_location("export_contest_case_scripts", EXPORTER_PATH)
assert SPEC is not None and SPEC.loader is not None
EXPORTER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(EXPORTER)


class StarFiveSafeBuildStormExporterTests(unittest.TestCase):
    def test_safe_mode_uses_x1_wrapper_without_changing_default(self) -> None:
        default_entry = EXPORTER.entry_script(False, True, False)
        safe_entry = EXPORTER.entry_script(False, True, False, True)

        self.assertIn('buildstorm_runner="/glibc/buildstorm_testcode.sh"', default_entry)
        self.assertNotIn("STARFIVE_BUILDSTORM_SAFE", default_entry)
        self.assertIn(
            'buildstorm_runner="/x1/starfive-safe-buildstorm.sh"', safe_entry
        )

    def test_safe_runner_is_fail_closed_and_preserves_failed_workspace(self) -> None:
        runner = EXPORTER.starfive_safe_buildstorm_script()

        self.assertIn('runs_root="/work/.whusp-buildstorm-runs"', runner)
        self.assertIn('run_base="$runs_root/run-${uptime_seconds}-$$"', runner)
        self.assertIn('while [ -e "$run_root" ] || [ -L "$run_root" ]; do', runner)
        self.assertIn('run_root="${run_base}-${run_suffix}"', runner)
        self.assertIn('run_root_collision_limit', runner)
        self.assertIn('/musl/busybox mkdir "$run_root"', runner)
        self.assertLess(
            runner.index('while [ -e "$run_root" ]'),
            runner.index('/musl/busybox mkdir "$run_root"'),
        )
        self.assertIn('minimum_free_kib=6291456', runner)
        self.assertIn('maximum_tmp_cache_kib=65536', runner)
        self.assertIn('cp -al "$source_entry" "$workspace/$entry_name"', runner)
        self.assertIn('tmp|docs) continue', runner)
        self.assertNotIn('target|tmp|docs) continue', runner)
        self.assertIn('"$workspace/target/debug/tg-xtask"', runner)
        self.assertIn('missing_cached_tg_xtask', runner)
        self.assertIn('target/.rustc_info.json', runner)
        self.assertIn('target/debug/.cargo-lock', runner)
        self.assertIn('cp -a "$source_workspace/tmp" "$workspace/tmp"', runner)
        self.assertIn('missing_axbuild_tmp_cache', runner)
        self.assertIn('tmp_cache_too_large_', runner)
        self.assertIn('patch_private_tmp_cache', runner)
        self.assertIn('unpatched_tmp_cache_path', runner)
        self.assertIn('STARFIVE_BUILDSTORM_SAFE_TMP_CACHE ok', runner)
        self.assertNotIn('cloned_tmp_must_be_absent', runner)
        self.assertNotIn('rm -rf "$source_workspace"', runner)
        self.assertIn("no_auto_cleanup=1", runner)
        self.assertIn('official_log="$run_root/buildstorm.official.out"', runner)
        self.assertIn('STARFIVE_BUILDSTORM_PERF_BEGIN point=$point', runner)
        self.assertIn('dump_perf_snapshot before', runner)
        self.assertIn('dump_perf_snapshot after', runner)
        self.assertIn("STARFIVE_BUILDSTORM_MMC_WRITE_AUTO active_blocks=$active", runner)
        self.assertIn("report_starfive_multiblock_write", runner)
        self.assertNotIn('echo 64 > "$knob"', runner)
        self.assertLess(
            runner.index("report_starfive_multiblock_write\n", runner.index("SAFE_PRECHECK ok")),
            runner.index("STARFIVE_BUILDSTORM_SAFE_CLONE begin"),
        )
        self.assertIn("STARFIVE_BUILDSTORM_SAFE_VALIDATION fail", runner)
        self.assertIn("'^BUILDSTORM_COMPILE mode=multi ok=true '", runner)
        self.assertIn("'^BUILDSTORM_TOOLCHAIN ok$'", runner)
        self.assertIn("'^BUILDSTORM_MINIBUILD ok$'", runner)

    def test_safe_mode_reboots_after_preserving_the_workspace(self) -> None:
        default_entry = EXPORTER.entry_script(False, True, False)
        safe_entry = EXPORTER.entry_script(False, True, False, True)

        self.assertIn("/musl/busybox reboot -f", default_entry)
        self.assertIn("/musl/busybox reboot -f", safe_entry)
        self.assertNotIn("StarFive safe BuildStorm parked in guest shell", safe_entry)
        self.assertIn("/musl/busybox sync", safe_entry)

    def test_safe_runner_is_emitted_executable(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            output = Path(temp_dir) / "runner"
            EXPORTER.write_outputs(output, False, False, True, False, True)

            safe_runner = output / "starfive-safe-buildstorm.sh"
            self.assertTrue(safe_runner.is_file())
            self.assertNotEqual(safe_runner.stat().st_mode & 0o111, 0)
            self.assertIn(
                "/x1/starfive-safe-buildstorm.sh",
                (output / "entry.sh").read_text(encoding="utf-8"),
            )


if __name__ == "__main__":
    unittest.main()
