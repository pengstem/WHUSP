from __future__ import annotations

import sys
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT / "scripts"))

import finals_guard


def cagent_log(group: str = "cagent") -> str:
    records = "\n".join(
        f"testcase cagent {name} pass 100"
        for name in finals_guard.CAGENT_TESTS
    )
    return (
        f"#### OS COMP TEST GROUP START {group} ####\n"
        f"{records}\n"
        f"#### OS COMP TEST GROUP END {group} ####\n"
    )


def buildstorm_log(group: str, arch: str, cores: int = 8) -> str:
    return f"""#### OS COMP TEST GROUP START {group} ####
BUILDSTORM_TOOLCHAIN ok
BUILDSTORM_MINIBUILD ok
BUILDSTORM_COMPILE mode=multi ok=true elapsed_s=500.0 cores={cores} bytes=600000 arch={arch}
#### OS COMP TEST GROUP END {group} ####
"""


class CAgentParserTests(unittest.TestCase):
    def test_accepts_published_group_name(self) -> None:
        result = finals_guard.parse_cagent(cagent_log("cagent"))
        self.assertTrue(result["valid"])
        self.assertEqual(result["score"], 199.1)

    def test_accepts_public_image_group_name(self) -> None:
        result = finals_guard.parse_cagent(cagent_log("cagent-glibc"))
        self.assertTrue(result["valid"])
        self.assertEqual(len(result["cases"]), 10)

    def test_rejects_duplicate_case_records(self) -> None:
        text = cagent_log() + "testcase cagent factorial pass 101\n"
        result = finals_guard.parse_cagent(text)
        self.assertFalse(result["valid"])
        self.assertTrue(any("duplicate" in issue for issue in result["issues"]))


class BuildStormParserTests(unittest.TestCase):
    def test_accepts_published_group_name(self) -> None:
        result = finals_guard.parse_buildstorm(
            buildstorm_log("buildstorm", "riscv64"), "rv", None
        )
        self.assertTrue(result["valid"])
        self.assertEqual(result["reference_script_score"], 180.0)

    def test_accepts_public_image_group_name(self) -> None:
        result = finals_guard.parse_buildstorm(
            buildstorm_log("buildstorm-glibc", "loongarch64"), "la", None
        )
        self.assertTrue(result["valid"])
        self.assertEqual(result["bytes"], 600_000)

    def test_rejects_non_final_core_count(self) -> None:
        result = finals_guard.parse_buildstorm(
            buildstorm_log("buildstorm", "loongarch64", cores=12), "la", None
        )
        self.assertFalse(result["valid"])
        self.assertTrue(any("cores=12" in issue for issue in result["issues"]))

    def test_accepts_explicit_physical_board_core_count(self) -> None:
        result = finals_guard.parse_buildstorm(
            buildstorm_log("buildstorm-glibc", "riscv64", cores=4),
            "rv",
            None,
            expected_cores=4,
        )
        self.assertTrue(result["valid"])
        self.assertEqual(result["expected_cores"], 4)

    def test_diagnoses_efault(self) -> None:
        text = "Bad address (os error 14)\n"
        diagnoses = finals_guard.diagnose_log(
            text,
            finals_guard.parse_cagent(text),
            finals_guard.parse_buildstorm(text, None, None),
        )
        self.assertTrue(any("EFAULT" in diagnosis for diagnosis in diagnoses))


if __name__ == "__main__":
    unittest.main()
