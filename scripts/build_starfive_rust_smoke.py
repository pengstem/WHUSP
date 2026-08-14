#!/usr/bin/env python3
"""Build a VisionFive 2 FIT that runs the frozen hello and multicrate probes."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import tempfile
from pathlib import Path

import run_rust_hello_bench as rust_bench


REPO_ROOT = Path(__file__).resolve().parents[1]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--kernel", type=Path, default=REPO_ROOT / "kernel-rv")
    parser.add_argument(
        "--disk-output",
        type=Path,
        default=Path("/tmp/whusp-starfive-rust-smoke.img"),
    )
    parser.add_argument(
        "--fit-output",
        type=Path,
        default=Path("/tmp/whusp-starfive-tftp/whusp-rust-smoke.itb"),
    )
    parser.add_argument("--image-size", default="64M")
    parser.add_argument("--perf-counters", type=int, choices=(0, 1), default=0)
    parser.add_argument(
        "--probe-multiblock-write",
        action="store_true",
        help="require the JH7110 startup probe to have enabled CMD25",
    )
    parser.add_argument(
        "--reboot-after",
        action="store_true",
        help="request reboot after the final marker (for watchdog/reacquire validation)",
    )
    return parser.parse_args()


def identity(
    workload: str, run_id: str | None = None, *, perf_counters: int = 0
) -> dict[str, str]:
    return {
        "run_id": run_id or f"starfive-{workload}",
        "arch": "rv",
        "kind": "measured",
        "sample": "1",
        "smp": "4",
        "mem": "4G",
        "block_io": "force-sync",
        "perf": str(perf_counters),
        "workload": workload,
    }


def entry_script(
    *, reboot_after: bool = False, probe_multiblock_write: bool = False
) -> str:
    script = """#!/musl/busybox sh
# StarFive-only staged Rust smoke test. Projects and build artifacts live on
# the kernel tmpfs; the physical SD supplies only the existing toolchain/cache.

export PATH="/tmp/bin:/glibc:/musl:/root/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
export TERM="vt220"

install_rootfs_link() {
    target="$1"
    link="$2"
    if [ ! -e "$link" ] && [ ! -L "$link" ] && [ -e "$target" ]; then
        /musl/busybox ln -s "$target" "$link" || return $?
    fi
}

/musl/busybox mkdir -p /bin /lib /lib64 /tmp/bin
/musl/busybox --install -s /tmp/bin
install_rootfs_link /musl/busybox /bin/sh || exit 127
install_rootfs_link /musl/lib/libc.so /lib/ld-musl-riscv64-sf.so.1 || exit 127
install_rootfs_link /musl/lib/libc.so /lib/ld-musl-riscv64.so.1 || exit 127
install_rootfs_link /glibc/lib/ld-linux-riscv64-lp64d.so.1 /lib/ld-linux-riscv64-lp64d.so.1 || exit 127

overall_status=0
echo "STARFIVE_RUST_SMOKE_STORAGE project=tmpfs fixture=x1 sd_bulk_writes=disabled"
"""
    if probe_multiblock_write:
        script += """echo "STARFIVE_MMC_WRITE_AUTO_CHECK_BEGIN expected_blocks=64"
mmc_write_knob=/proc/oskernel/starfive_mmc_max_write_blocks
if [ ! -f "$mmc_write_knob" ] || \
   [ "$(/musl/busybox cat "$mmc_write_knob")" != 64 ]; then
    echo "STARFIVE_MMC_WRITE_AUTO_CHECK_RESULT ok=false"
    echo "FINAL: starfive rust smoke finished (status=1)"
    /musl/busybox sync
    /musl/busybox reboot -f
    exit 1
fi
echo "STARFIVE_MMC_WRITE_AUTO_CHECK_RESULT ok=true active_blocks=64"
"""
    script += """echo "FINAL: starting rust-hello-cold"
/musl/busybox ash /x1/g0-rust-hello-cold.sh
hello_cold_status=$?
echo "FINAL: finished rust-hello-cold (status=$hello_cold_status)"
if [ "$hello_cold_status" -ne 0 ]; then
    overall_status=1
fi

clear_rust_smoke_tmp() {
    /musl/busybox rm -rf \
        /tmp/minibuild \
        /tmp/rust-build-timer.result \
        /tmp/minibuild.stdout \
        /tmp/minibuild.stderr \
        /tmp/g0-rust-hello-perf.before \
        /tmp/g0-rust-hello-perf.after \
        /tmp/g0-rustc-active \
        /tmp/g0-rustc-active.samples \
        /tmp/g0-rustc-wrapper.sh
}

clear_rust_smoke_tmp

hello_warm_status=1
if [ "$hello_cold_status" -eq 0 ]; then
    echo "FINAL: starting rust-hello-warm"
    /musl/busybox ash /x1/g0-rust-hello-warm.sh
    hello_warm_status=$?
    echo "FINAL: finished rust-hello-warm (status=$hello_warm_status)"
    if [ "$hello_warm_status" -ne 0 ]; then
        overall_status=1
    fi
else
    echo "FINAL: rust-hello-warm skipped: rust-hello-cold failed"
fi

clear_rust_smoke_tmp

if [ "$hello_cold_status" -eq 0 ] && [ "$hello_warm_status" -eq 0 ]; then
    echo "FINAL: starting rust-multicrate"
    /musl/busybox ash /x1/g0-rust-multicrate.sh
    multicrate_status=$?
    echo "FINAL: finished rust-multicrate (status=$multicrate_status)"
    if [ "$multicrate_status" -ne 0 ]; then
        overall_status=1
    fi
else
    echo "FINAL: rust-multicrate skipped: rust-hello stage failed"
fi

echo "FINAL: starfive rust smoke finished (status=$overall_status)"
/musl/busybox sync
echo "FINAL: StarFive Rust smoke parked in guest shell"
exec /musl/busybox sh
exit "$overall_status"
"""
    if reboot_after:
        parked = """echo "FINAL: StarFive Rust smoke parked in guest shell"
exec /musl/busybox sh
exit "$overall_status"
"""
        reboot = """echo "FINAL: StarFive watchdog reboot requested"
/musl/busybox reboot -f
exit "$overall_status"
"""
        if script.count(parked) != 1:
            raise RuntimeError("Rust smoke reboot trailer contract changed")
        script = script.replace(parked, reboot)
    return script


def write_staging(
    staging: Path,
    timer_binary: Path,
    *,
    perf_counters: int,
    reboot_after: bool = False,
    probe_multiblock_write: bool = False,
) -> None:
    hello_cold = rust_bench.render_guest(
        identity(
            "hello", "starfive-hello-cold", perf_counters=perf_counters
        ),
        project_storage="tmpfs",
    )
    hello_warm = rust_bench.render_guest(
        identity(
            "hello", "starfive-hello-warm", perf_counters=perf_counters
        ),
        project_storage="tmpfs",
    )
    multicrate = rust_bench.render_guest(
        identity("multicrate", perf_counters=perf_counters),
        project_storage="tmpfs",
    )
    fixture_source = "/root/minibuild"
    fixture_target = "/x1/multicrate-fixture"
    if multicrate.count(fixture_source) != 2:
        raise RuntimeError("multicrate guest fixture path contract changed")
    multicrate = multicrate.replace(fixture_source, fixture_target)

    for name, contents in (
        (
            "entry.sh",
            entry_script(
                reboot_after=reboot_after,
                probe_multiblock_write=probe_multiblock_write,
            ),
        ),
        ("g0-rust-hello-cold.sh", hello_cold),
        ("g0-rust-hello-warm.sh", hello_warm),
        ("g0-rust-multicrate.sh", multicrate),
    ):
        path = staging / name
        path.write_text(contents, encoding="utf-8", newline="\n")
        path.chmod(0o755)

    installed_timer = staging / "rust_build_timer"
    shutil.copy2(timer_binary, installed_timer)
    installed_timer.chmod(0o755)
    rust_bench.build_multicrate_fixture(staging / "multicrate-fixture")


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def main() -> int:
    args = parse_args()
    kernel = args.kernel.resolve()
    if not kernel.is_file():
        raise SystemExit(f"kernel does not exist: {kernel}")
    if not rust_bench.IMAGE_SIZE_RE.fullmatch(args.image_size):
        raise SystemExit("--image-size must be a positive size ending in M or G")

    args.disk_output.parent.mkdir(parents=True, exist_ok=True)
    args.fit_output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="whusp-starfive-rust-smoke-") as temp:
        temp_root = Path(temp)
        setup_dir = temp_root / "setup"
        setup_dir.mkdir()
        timer = rust_bench.build_timer(
            rust_bench.compiler_for("rv"), temp_root, setup_dir
        )
        staging = temp_root / "staging"
        staging.mkdir()
        write_staging(
            staging,
            timer,
            perf_counters=args.perf_counters,
            reboot_after=args.reboot_after,
            probe_multiblock_write=args.probe_multiblock_write,
        )

        temp_image = temp_root / "rust-smoke.img"
        subprocess.run(["truncate", "-s", args.image_size, temp_image], check=True)
        subprocess.run(
            [
                "mkfs.ext4",
                "-q",
                "-F",
                "-N",
                "8192",
                "-O",
                "^orphan_file,^metadata_csum_seed,^metadata_csum,^64bit,^has_journal",
                "-d",
                staging,
                temp_image,
            ],
            check=True,
        )
        shutil.copy2(temp_image, args.disk_output)

    environment = os.environ.copy()
    environment.update(
        {
            "STARFIVE_KERNEL_ELF": str(kernel),
            "STARFIVE_RUNNER_DISK": str(args.disk_output),
            "STARFIVE_FIT_OUTPUT": str(args.fit_output),
        }
    )
    subprocess.run(
        [str(REPO_ROOT / "scripts" / "build_starfive_fit.sh")],
        cwd=REPO_ROOT,
        env=environment,
        check=True,
    )
    manifest = {
        "kernel": str(kernel),
        "runner_disk": str(args.disk_output),
        "fit": str(args.fit_output),
        "fit_sha256": sha256(args.fit_output),
        "workloads": ["hello-cold", "hello-warm", "multicrate"],
        "smp": 4,
        "memory": "4G",
        "project_storage": "tmpfs",
        "perf_counters": args.perf_counters,
        "reboot_after": args.reboot_after,
        "probe_multiblock_write": args.probe_multiblock_write,
        "multicrate_leaf_crates": rust_bench.MULTICRATE_LEAF_CRATES,
    }
    manifest_path = args.fit_output.with_suffix(args.fit_output.suffix + ".json")
    manifest_path.write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    print(f"built StarFive Rust smoke FIT: {args.fit_output}")
    print(f"manifest: {manifest_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
