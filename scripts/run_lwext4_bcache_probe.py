#!/usr/bin/env python3
"""Run the host-side lwext4 metadata-cache concurrency probes offline."""

from __future__ import annotations

import os
import shutil
import subprocess
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "vendor" / "lwext4_rust"


def prepare_manifest(crate: Path) -> None:
    """Remove the unused print adapter from the disposable test copy.

    The kernel builds lwext4 without its optional printf adapter. The adapter's
    historical git dependency is not part of the contest offline vendor set,
    but Cargo still resolves optional packages when generating a standalone
    lock file. Host tests therefore use an exact disposable manifest copy; the
    submission manifest and source remain unchanged.
    """

    manifest = crate / "Cargo.toml"
    text = manifest.read_text()
    old_features = """default = [
    \"print\",
    \"std\",
]
print = [\"printf-compat\"]
"""
    old_dependency = """[dependencies.printf-compat]
git = \"https://github.com/lights0123/printf-compat.git\"
rev = \"5f5c9cc\"
optional = true
default-features = false

"""
    if old_features not in text or old_dependency not in text:
        raise RuntimeError("lwext4 Cargo.toml print dependency layout changed")
    text = text.replace(old_features, 'default = ["std"]\nprint = []\n')
    text = text.replace(old_dependency, "")
    manifest.write_text(text)


def main() -> int:
    with tempfile.TemporaryDirectory(prefix="whusp-lwext4-bcache-") as temp:
        temp_path = Path(temp)
        crate = temp_path / "lwext4_rust"
        shutil.copytree(SOURCE, crate, ignore=shutil.ignore_patterns("target"))
        prepare_manifest(crate)
        lock_dir = temp_path / "lock"
        lock_dir.mkdir()

        env = os.environ.copy()
        env["CARGO_HOME"] = str(ROOT / "vendor")
        rustflags = env.get("RUSTFLAGS", "")
        env["RUSTFLAGS"] = f"{rustflags} -Aunnecessary-transmutes".strip()
        command = [
            "cargo",
            "test",
            "-j",
            "1",
            "-Z",
            "unstable-options",
            "--lockfile-path",
            str(lock_dir / "Cargo.lock"),
            "--manifest-path",
            str(crate / "Cargo.toml"),
            "--lib",
            "--no-default-features",
            "--features",
            "std",
            "--offline",
            "--",
            "--test-threads=1",
        ]
        subprocess.run(command, cwd=ROOT, env=env, check=True)

    print("LWEXT4_BCACHE_PROBE_PASS cases=5")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
