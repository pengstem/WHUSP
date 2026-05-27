#!/usr/bin/env python3
"""Export contest runner group commands and LTP whitelist cases as shell scripts."""

from __future__ import annotations

import argparse
import ast
import fcntl
import hashlib
import re
import shlex
import shutil
from dataclasses import dataclass
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
RUNNER_PATH = REPO_ROOT / "os" / "src" / "task" / "contest_runner.rs"
WHITELIST_PATH = REPO_ROOT / "os" / "src" / "task" / "ltp_whitelist.txt"
DEFAULT_RUNTEST_DIR = (
    REPO_ROOT / "testsuits" / "ltp-full-20240524" / "runtest"
).resolve()
DEFAULT_OUT_DIR = REPO_ROOT / "contest-case-commands"
MARKER_FILE = ".generated-by-export-contest-case-scripts"
ARCHES = ("rv", "la")
LA_MUSL_COMPAT_PRELOAD = "/opt/oscomp-support/lib/liboscomp-musl-compat.so"
DEBUG_VERSION = "la-sigill-block-probe-20260527"


@dataclass(frozen=True)
class LtpCase:
    order: int
    name: str
    manifest: str
    manifest_line: int
    command: str


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Generate guest-side shell scripts for every contest group and "
            "every case in os/src/task/ltp_whitelist.txt."
        )
    )
    parser.add_argument(
        "--out-dir",
        type=Path,
        default=DEFAULT_OUT_DIR,
        help="directory to write; defaults to contest-case-commands",
    )
    parser.add_argument(
        "--runtest-dir",
        type=Path,
        default=DEFAULT_RUNTEST_DIR,
        help="LTP runtest manifest directory from the testsuite checkout",
    )
    parser.add_argument(
        "--force",
        action="store_true",
        help="replace an existing generated output directory",
    )
    return parser.parse_args()


def read_text(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def rust_string(value: str) -> str:
    return ast.literal_eval(f'"{value}"')


def rust_string_array(source: str, const_name: str) -> list[str]:
    match = re.search(
        rf"const\s+{re.escape(const_name)}\s*:\s*&\[\s*&str\s*\]\s*=\s*&\[(.*?)\];",
        source,
        re.DOTALL,
    )
    if not match:
        raise ValueError(f"could not find Rust string array {const_name}")
    block = re.sub(r"//.*", "", match.group(1))
    return [rust_string(item) for item in re.findall(r'"((?:\\.|[^"\\])*)"', block)]


def text_list(source: str, source_name: str) -> list[str]:
    items: list[str] = []
    for line_no, raw in enumerate(source.splitlines(), 1):
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        if any(ch.isspace() for ch in line):
            raise ValueError(f"{source_name}:{line_no}: whitespace is not allowed in case names")
        items.append(line)
    return items


def rust_const_value(source: str, const_name: str) -> str:
    match = re.search(
        rf"const\s+{re.escape(const_name)}\s*:[^=]+=\s*(.*?);",
        source,
        re.DOTALL,
    )
    if not match:
        raise ValueError(f"could not find Rust const {const_name}")
    return " ".join(match.group(1).split())


def iter_manifest_entries(path: Path):
    for line_no, raw in enumerate(path.read_text(encoding="utf-8", errors="replace").splitlines(), 1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split(None, 1)
        if len(parts) < 2:
            continue
        yield line_no, parts[0], parts[1]


def resolve_ltp_cases(
    manifests: list[str],
    whitelist: list[str],
    runtest_dir: Path,
) -> list[LtpCase]:
    duplicate_whitelist = sorted({name for name in whitelist if whitelist.count(name) > 1})
    if duplicate_whitelist:
        raise ValueError("duplicate whitelist entries: " + ", ".join(duplicate_whitelist))

    order = {name: index for index, name in enumerate(whitelist)}
    selected: list[tuple[int, int, str, str, str, int]] = []
    seq = 0
    for manifest in manifests:
        path = runtest_dir / manifest
        if not path.exists():
            raise FileNotFoundError(f"missing runtest manifest: {path}")
        for line_no, case_name, case_cmd in iter_manifest_entries(path):
            if case_name not in order:
                continue
            seq += 1
            selected.append((order[case_name], seq, case_name, case_cmd, manifest, line_no))

    selected.sort()
    cases: list[LtpCase] = []
    seen: set[str] = set()
    for case_order, _seq, case_name, case_cmd, manifest, line_no in selected:
        if case_name in seen:
            continue
        seen.add(case_name)
        cases.append(
            LtpCase(
                order=case_order,
                name=case_name,
                manifest=manifest,
                manifest_line=line_no,
                command=case_cmd,
            )
        )

    missing = [name for name in whitelist if name not in seen]
    if missing:
        raise ValueError("missing runtest commands for: " + ", ".join(missing))
    return cases


def sh_quote(value: str) -> str:
    return "'" + value.replace("'", "'\"'\"'") + "'"


def libc_label(libc_root: str) -> str:
    return libc_root.strip("/")


def test_name(script: str) -> str:
    return script.removesuffix("_testcode.sh")


def group_filename(index: int, script: str) -> str:
    return f"{index:03d}-{test_name(script)}.sh"


def ltp_case_filename(case: LtpCase) -> str:
    return f"{case.order:04d}-{case.name}.sh"


def write_executable(path: Path, text: str) -> None:
    path.write_text(text, encoding="utf-8", newline="\n")
    path.chmod(0o755)


def prepare_out_dir(path: Path, force: bool) -> None:
    if path.exists():
        marker = path / MARKER_FILE
        if not force:
            raise FileExistsError(f"{path} already exists; pass --force to replace it")
        if not marker.exists():
            raise FileExistsError(f"{path} does not look generated; refusing to replace it")
        shutil.rmtree(path)
    path.mkdir(parents=True)
    (path / MARKER_FILE).write_text(
        "Generated by scripts/export_contest_case_scripts.py.\n",
        encoding="utf-8",
    )


def output_lock_path(path: Path) -> Path:
    digest = hashlib.sha256(str(path.resolve()).encode()).hexdigest()[:16]
    return Path("/tmp") / f"whusp-export-contest-case-scripts-{digest}.lock"


def acquire_output_lock(path: Path):
    lock_file = output_lock_path(path).open("w", encoding="utf-8")
    fcntl.flock(lock_file, fcntl.LOCK_EX)
    return lock_file


def common_script(manifests: list[str]) -> str:
    manifest_words = " ".join(manifests)
    script = """#!/musl/busybox sh
# Common guest-side helpers exported from os/src/task/contest_runner.rs.

whusp_setup_runtime_environment() {
    /musl/busybox mkdir -p /tmp/bin
    /musl/busybox --install -s /tmp/bin
    for cmd in useradd userdel groupdel mkfs.xfs mkfs.ext2 exportfs; do
        /musl/busybox rm -f /tmp/bin/$cmd
        /musl/busybox printf '#!/musl/busybox sh\\nexit 0\\n' > /tmp/bin/$cmd
        /musl/busybox chmod +x /tmp/bin/$cmd
    done
    /musl/busybox rm -f /tmp/bin/mkfs.ext4
    /musl/busybox printf '#!/musl/busybox sh\\nif [ "$1" = "-V" ]; then echo "mke2fs 1.46.5"; exit 0; fi\\nexit 0\\n' > /tmp/bin/mkfs.ext4
    /musl/busybox chmod +x /tmp/bin/mkfs.ext4
    /musl/busybox rm -f /tmp/bin/e4crypt
    /musl/busybox printf '#!/musl/busybox sh\\nif [ "$1" = "add_key" ] && [ -n "$2" ]; then /musl/busybox touch "$2/.whusp_e4crypt_encrypted"; exit $?; fi\\nexit 1\\n' > /tmp/bin/e4crypt
    /musl/busybox chmod +x /tmp/bin/e4crypt
    export PATH=/tmp/bin:/musl:/glibc:$PATH
}

whusp_debug_run() {
    _whusp_debug_label="$1"
    shift
    echo "#### WHUSP DEBUG CMD START $_whusp_debug_label ####"
    "$@"
    _whusp_debug_ret=$?
    echo "WHUSP_DEBUG_CMD_RET $_whusp_debug_label $_whusp_debug_ret"
    echo "#### WHUSP DEBUG CMD END $_whusp_debug_label ####"
    unset _whusp_debug_label _whusp_debug_ret
    return 0
}

whusp_debug_ltp_probe() {
    _whusp_debug_libc="$1"
    _whusp_debug_case="$2"
    _whusp_debug_bin="$_whusp_debug_libc/ltp/testcases/bin/$_whusp_debug_case"
    echo "#### WHUSP DEBUG LTP PROBE START $_whusp_debug_libc $_whusp_debug_case ####"
    if [ -f "$_whusp_debug_bin" ]; then
        (
            whusp_setup_runtime_environment
            LIBC_ROOT="$_whusp_debug_libc"
            whusp_setup_ltp_environment
            "./$_whusp_debug_case"
        )
        _whusp_debug_ret=$?
    else
        echo "WHUSP_DEBUG_LTP_PROBE_MISSING $_whusp_debug_bin"
        _whusp_debug_ret=127
    fi
    echo "WHUSP_DEBUG_LTP_PROBE_RET $_whusp_debug_libc $_whusp_debug_case $_whusp_debug_ret"
    echo "#### WHUSP DEBUG LTP PROBE END $_whusp_debug_libc $_whusp_debug_case ####"
    unset _whusp_debug_libc _whusp_debug_case _whusp_debug_bin _whusp_debug_ret
    return 0
}

whusp_debug_prelude() {
    whusp_setup_runtime_environment
    echo "#### WHUSP DEBUG START __WHUSP_DEBUG_VERSION__ ####"
    echo "WHUSP_DEBUG_VERSION=__WHUSP_DEBUG_VERSION__"
    echo "WHUSP_DEBUG_ARCH=${WHUSP_ARCH:-unset}"
    echo "WHUSP_DEBUG_TEST_SCRIPTS=${WHUSP_TEST_SCRIPTS:-unset}"
    echo "WHUSP_DEBUG_TEST_LIBCS=${WHUSP_TEST_LIBCS:-unset}"
    echo "WHUSP_DEBUG_LTP_FILTER=${WHUSP_LTP_FILTER_OPTION:-unset}"
    echo "WHUSP_DEBUG_LTP_WHITELIST_LEN=${WHUSP_LTP_WHITELIST_LEN:-unset}"
    echo "WHUSP_DEBUG_SCRIPT_DIR=${script_dir:-unset}"
    whusp_debug_run ls_x1 /musl/busybox ls -la /x1
    whusp_debug_run cksum_entry /musl/busybox cksum /x1/entry.sh
    whusp_debug_run cksum_common /musl/busybox cksum /x1/common.sh
    whusp_debug_run ls_ltp_glibc_pathconf01 /musl/busybox ls -l /glibc/ltp/testcases/bin/pathconf01
    whusp_debug_run ls_ltp_glibc_writev01 /musl/busybox ls -l /glibc/ltp/testcases/bin/writev01
    whusp_debug_run ls_ltp_musl_pathconf01 /musl/busybox ls -l /musl/ltp/testcases/bin/pathconf01
    whusp_debug_run proc_cpuinfo /musl/busybox cat /proc/cpuinfo
    whusp_debug_run proc_mounts /musl/busybox cat /proc/mounts
    whusp_debug_run proc_block_cache_before /musl/busybox cat /proc/block_cache
    case "${WHUSP_ARCH:-rv}" in
        la)
            whusp_debug_ltp_probe /glibc pathconf01
            whusp_debug_ltp_probe /musl pathconf01
            whusp_debug_run proc_block_cache_after_probe /musl/busybox cat /proc/block_cache
            ;;
        *)
            echo "WHUSP_DEBUG_LTP_PROBE_SKIPPED arch=${WHUSP_ARCH:-rv}"
            ;;
    esac
    echo "#### WHUSP DEBUG END __WHUSP_DEBUG_VERSION__ ####"
}

whusp_setup_ltp_environment() {
    : "${LIBC_ROOT:=/musl}"
    export LTPROOT="$LIBC_ROOT/ltp"
    export TMPBASE="/tmp"
    export TST_TIMEOUT="-1"
    export LTP_SINGLE_FS_TYPE="ext2"
    if [ "$LIBC_ROOT" = "/musl" ]; then
        export LD_LIBRARY_PATH="/musl/lib:/glibc/lib:/lib"
    else
        export LD_LIBRARY_PATH="$LIBC_ROOT/lib:/glibc/lib:/musl/lib:/lib"
    fi
    export PATH="$PATH:$LTPROOT/testcases/bin:$LTPROOT/testcases/lib:$LTPROOT/bin:/musl/ltp/testcases/bin:/musl/ltp/testcases/lib:/musl/ltp/bin:/glibc/ltp/testcases/bin:/glibc/ltp/testcases/lib:/glibc/ltp/bin"
    cd "$LTPROOT/testcases/bin" || exit 127
}

whusp_ltp_fs_bind_preflight() {
    case "$case_name" in
        fs_bind_*)
            unset TST_LIB_LOADED TST_SECURITY_LOADED
            for _whusp_ltp_helper in fs_bind_lib.sh tst_test.sh tst_ansi_color.sh tst_security.sh; do
                for _whusp_ltp_dir in "$LTPROOT/testcases/bin" "$LTPROOT/testcases/lib"; do
                    [ -f "$_whusp_ltp_dir/$_whusp_ltp_helper" ] &&
                        /musl/busybox cat "$_whusp_ltp_dir/$_whusp_ltp_helper" >/dev/null 2>&1
                done
            done
            ;;
    esac
}

whusp_ltp_eval_case() {
    _whusp_ltp_prog="${case_cmd%%[	 ]*}"
    case "$_whusp_ltp_prog" in
        ""|*/*)
            eval "$case_cmd"
            ret=$?
            ;;
        *)
            _whusp_ltp_path="./$_whusp_ltp_prog"
            if [ -f "$_whusp_ltp_path" ]; then
                _whusp_ltp_args="${case_cmd#$_whusp_ltp_prog}"
                eval "$_whusp_ltp_path$_whusp_ltp_args"
                ret=$?
            else
                eval "$case_cmd"
                ret=$?
            fi
            ;;
    esac
    unset _whusp_ltp_prog _whusp_ltp_path _whusp_ltp_args
}

whusp_ltp_run_current_case() {
    echo "RUN LTP CASE $case_name"
    whusp_ltp_fs_bind_preflight
    case "$case_name" in
        statx10)
            _old_ltp_single_fs_type="$LTP_SINGLE_FS_TYPE"
            export LTP_SINGLE_FS_TYPE="ext4"
            whusp_ltp_eval_case
            _whusp_ltp_ret="$ret"
            export LTP_SINGLE_FS_TYPE="$_old_ltp_single_fs_type"
            ret="$_whusp_ltp_ret"
            unset _old_ltp_single_fs_type _whusp_ltp_ret
            ;;
        *)
            whusp_ltp_eval_case
            ;;
    esac
    echo "FAIL LTP CASE $case_name : $ret"
}

whusp_ltp_run_case() {
    whusp_setup_runtime_environment
    whusp_setup_ltp_environment
    whusp_ltp_run_current_case
    exit "$ret"
}

whusp_ltp_filter_accepts() {
    filter="${WHUSP_LTP_FILTER_OPTION:-None}"
    case "$filter" in
        ""|None|none)
            return 0
            ;;
        long|LONG)
            case "$case_name" in
                [A-Za-z]*) return 1 ;;
                *) return 0 ;;
            esac
            ;;
        case:*)
            [ "$case_name" = "${filter#case:}" ]
            return $?
            ;;
        cases:*)
            _old_ifs="$IFS"
            IFS=,
            for _selected_case in ${filter#cases:}; do
                if [ "$case_name" = "$_selected_case" ]; then
                    IFS="$_old_ifs"
                    unset _old_ifs _selected_case
                    return 0
                fi
            done
            IFS="$_old_ifs"
            unset _old_ifs _selected_case
            return 1
            ;;
        prefix:*)
            _prefix="${filter#prefix:}"
            case "$case_name" in
                "$_prefix"*) unset _prefix; return 0 ;;
                *) unset _prefix; return 1 ;;
            esac
            ;;
        range:*)
            _range="${filter#range:}"
            _start="${_range%%,*}"
            _end="${_range#*,}"
            if [ "$_range" = "$_end" ]; then
                unset _range _start _end
                return 1
            fi
            if [ -n "$_start" ] && [ "$case_name" \\< "$_start" ]; then
                unset _range _start _end
                return 1
            fi
            if [ -n "$_end" ]; then
                [ "$case_name" \\< "$_end" ] || { unset _range _start _end; return 1; }
            fi
            unset _range _start _end
            return 0
            ;;
        ?)
            _case_first="$(/musl/busybox printf '%.1s' "$case_name" | /musl/busybox tr 'A-Z' 'a-z')"
            _filter_letter="$(/musl/busybox printf '%.1s' "$filter" | /musl/busybox tr 'A-Z' 'a-z')"
            [ "$_case_first" = "$_filter_letter" ]
            _ret=$?
            unset _case_first _filter_letter
            return "$_ret"
            ;;
        *)
            /musl/busybox echo "INVALID LTP_CASE_FILTER_OPTION"
            return 2
            ;;
    esac
}

whusp_ltp_run_manifest_filter() {
    whusp_setup_ltp_environment
    status=0
    for manifest_name in __LTP_MANIFEST_WORDS__; do
        manifest="$LTPROOT/runtest/$manifest_name"
        [ -f "$manifest" ] || continue
        while read case_name case_cmd; do
            [ -n "$case_name" ] || continue
            case "$case_name" in \\#*) continue ;; esac
            [ -n "$case_cmd" ] || continue
            whusp_ltp_filter_accepts
            _selected=$?
            if [ "$_selected" -eq 2 ]; then
                unset _selected
                return 2
            fi
            [ "$_selected" -eq 0 ] || { unset _selected; continue; }
            unset _selected
            whusp_ltp_run_current_case
            [ "$ret" -eq 0 ] || status="$ret"
        done < "$manifest"
    done
    return "$status"
}
"""
    return script.replace("__LTP_MANIFEST_WORDS__", manifest_words).replace(
        "__WHUSP_DEBUG_VERSION__", DEBUG_VERSION
    )


def source_common(relative_common: str) -> str:
    return f"""case "$0" in
    */*) script_dir="${{0%/*}}" ;;
    *) script_dir="." ;;
esac
. "$script_dir/{relative_common}"
"""


def group_body(arch: str, libc_root: str, script: str) -> str:
    group = test_name(script)
    libc = libc_label(libc_root)
    if script == "basic_testcode.sh":
        return f"""whusp_setup_runtime_environment
cd {sh_quote(libc_root)} || exit 127
./busybox echo "#### OS COMP TEST GROUP START basic-{libc} ####"
cd {sh_quote(libc_root + "/basic")} || exit 127
../busybox sh ./run-all.sh
ret=$?
cd {sh_quote(libc_root)} || exit 127
./busybox echo "#### OS COMP TEST GROUP END basic-{libc} ####"
exit "$ret"
"""
    if script == "ltp_testcode.sh":
        return f"""{libc_root}/busybox echo "#### OS COMP TEST GROUP START ltp-{libc} ####"
case "${{WHUSP_LTP_FILTER_OPTION:-None}}" in
    ""|None|none)
        /musl/busybox sh "$script_dir/../run_ltp_whitelist.sh"
        ;;
    *)
        whusp_setup_runtime_environment
        whusp_ltp_run_manifest_filter
        ;;
esac
{libc_root}/busybox echo "#### OS COMP TEST GROUP END ltp-{libc} ####"
exit 0
"""

    commands = ["whusp_setup_runtime_environment", f"cd {sh_quote(libc_root)} || exit 127"]
    if arch == "la" and libc_root == "/musl" and script == "iperf_testcode.sh":
        commands.append("./busybox sed 's/ -i 0 / /g' ./iperf_testcode.sh | ./busybox sh")
    else:
        if script == "lmbench_testcode.sh":
            commands.append("export ENOUGH=10000 TIMING_O=0 LOOP_O=0")
            commands.append("./busybox rm -f /tmp/hello")
        prefix = ""
        if arch == "la" and libc_root == "/musl" and script == "cyclictest_testcode.sh":
            prefix = f"LD_PRELOAD={LA_MUSL_COMPAT_PRELOAD} "
        commands.append(f"{prefix}./busybox sh ./{script}")
        if script == "lmbench_testcode.sh":
            commands.append('ret=$?')
            commands.append("./busybox rm -f /tmp/hello")
            commands.append('exit "$ret"')
            return "\n".join(commands) + "\n"
    commands.append('exit "$?"')
    return "\n".join(commands) + "\n"


def group_script(
    arch: str,
    libc_root: str,
    script: str,
    index: int,
    enabled: bool,
    runner_skipped: bool,
) -> str:
    return f"""#!/musl/busybox sh
# Generated from os/src/task/contest_runner.rs.
# arch={arch}
# libc={libc_label(libc_root)}
# group={test_name(script)}
# source_script={script}
# current_runner_enabled={'yes' if enabled else 'no'}
# current_runner_skips_this_libc={'yes' if runner_skipped else 'no'}

{source_common('../../../common.sh')}{group_body(arch, libc_root, script)}"""


def ltp_case_script(arch: str, libc_root: str, case: LtpCase) -> str:
    return f"""#!/musl/busybox sh
# Generated from current WHUSP LTP whitelist and runtest manifests.
# arch={arch}
# libc={libc_label(libc_root)}
# whitelist_index={case.order}
# manifest={case.manifest}:{case.manifest_line}
# runtest_command={case.command}

LIBC_ROOT={sh_quote(libc_root)}
case_name={sh_quote(case.name)}
case_cmd={sh_quote(case.command)}

{source_common('../../../common.sh')}whusp_ltp_run_case
"""


def ltp_case_argv(case: LtpCase) -> list[str]:
    try:
        argv = shlex.split(case.command)
    except ValueError as err:
        raise ValueError(f"{case.name}: invalid shell command {case.command!r}: {err}") from err
    if not argv:
        raise ValueError(f"{case.name}: empty LTP command")
    shell_tokens = {";", "|", "&", "&&", "||", "<", ">", ">>", "2>", "<<"}
    if any(token in shell_tokens for token in argv):
        raise ValueError(f"{case.name}: complex shell command is not supported: {case.command!r}")
    if any(ch in case.command for ch in "$`\\\n"):
        raise ValueError(f"{case.name}: shell expansion is not supported: {case.command!r}")
    if "=" in argv[0]:
        raise ValueError(f"{case.name}: environment-prefixed command is not supported: {case.command!r}")
    return argv


def ltp_static_command_args(case: LtpCase) -> list[str]:
    argv = ltp_case_argv(case)
    prog = argv[0]
    if "/" not in prog:
        prog = f"./{prog}"
    return [prog, *argv[1:]]


def ltp_static_case_block(case: LtpCase) -> str:
    command_args = ltp_static_command_args(case)
    command = " ".join(sh_quote(arg) for arg in command_args)
    lines = [
        f'echo "RUN LTP CASE {case.name}"',
    ]
    if Path(command_args[0]).name.startswith("fs_bind"):
        lines.append("fs_bind_preflight")
    if case.name == "statx10":
        lines.extend(
            [
                '_old_ltp_single_fs_type="$LTP_SINGLE_FS_TYPE"',
                'export LTP_SINGLE_FS_TYPE="ext4"',
                command,
                "ret=$?",
                'export LTP_SINGLE_FS_TYPE="$_old_ltp_single_fs_type"',
                "unset _old_ltp_single_fs_type",
            ]
        )
        result = "$ret"
    else:
        lines.append(command)
        result = "$?"
    lines.extend(
        [
            f'echo "FAIL LTP CASE {case.name} : {result}"',
        ]
    )
    return "\n".join(lines)


def ltp_whitelist_script(arch: str, libc_root: str, cases: list[LtpCase]) -> str:
    case_lines: list[str] = []
    if not cases:
        case_lines.append("# No LTP whitelist cases are currently selected.")
    for case in cases:
        case_lines.append(ltp_static_case_block(case))
    case_body = "\n\n".join(case_lines)
    return f"""#!/musl/busybox sh
# Generated unified LTP whitelist runner.
# arch={arch}
# libc={libc_label(libc_root)}
# whitelist_cases={len(cases)}

LIBC_ROOT={sh_quote(libc_root)}

{source_common('../../common.sh')}whusp_setup_runtime_environment
whusp_setup_ltp_environment

fs_bind_preflight() {{
    unset TST_LIB_LOADED TST_SECURITY_LOADED
    for _whusp_ltp_helper in fs_bind_lib.sh tst_test.sh tst_ansi_color.sh tst_security.sh; do
        for _whusp_ltp_dir in "$LTPROOT/testcases/bin" "$LTPROOT/testcases/lib"; do
            [ -f "$_whusp_ltp_dir/$_whusp_ltp_helper" ] &&
                /musl/busybox cat "$_whusp_ltp_dir/$_whusp_ltp_helper" >/dev/null 2>&1
        done
    done
}}

{case_body}
exit 0
"""


def run_all_script(kind: str) -> str:
    subdir = "groups" if kind == "groups" else "ltp-cases"
    return f"""#!/musl/busybox sh
# Run every exported {kind} script in lexical order.

case "$0" in
    */*) script_dir="${{0%/*}}" ;;
    *) script_dir="." ;;
esac

status=0
for script in "$script_dir"/{subdir}/*.sh; do
    /musl/busybox sh "$script"
    ret=$?
    [ "$ret" -eq 0 ] || status=$ret
done
exit "$status"
"""


def entry_script(all_tests: list[str], test_scripts: list[str], libc_roots: list[str]) -> str:
    default_test_scripts = " ".join(test_scripts)
    default_test_libcs = " ".join(libc_roots)
    lines = [
        "#!/musl/busybox sh",
        "# Entry point for the generated contest script disk.",
        "",
        'case "$0" in',
        '    */*) script_dir="${0%/*}" ;;',
        '    *) script_dir="." ;;',
        "esac",
        '. "$script_dir/common.sh"',
        "",
        f'WHUSP_TEST_SCRIPTS="${{WHUSP_TEST_SCRIPTS:-{default_test_scripts}}}"',
        f'WHUSP_TEST_LIBCS="${{WHUSP_TEST_LIBCS:-{default_test_libcs}}}"',
        'case "${WHUSP_ARCH:-rv}" in',
        '    la|loongarch64) WHUSP_ARCH="la" ;;',
        '    *) WHUSP_ARCH="rv" ;;',
        "esac",
        "",
        "whusp_debug_prelude",
        "",
        "whusp_script_enabled() {",
        '    case " $WHUSP_TEST_SCRIPTS " in',
        '        *" $1 "*) return 0 ;;',
        "        *) return 1 ;;",
        "    esac",
        "}",
        "",
        "whusp_libc_label() {",
        '    case "$1" in',
        '        /musl) echo "musl" ;;',
        '        /glibc) echo "glibc" ;;',
        '        *) echo "unknown" ;;',
        "    esac",
        "}",
        "",
        "whusp_skip_group() {",
        '    group="$1"',
        '    libc="$(whusp_libc_label "$2")"',
        '    echo "#### OS COMP TEST GROUP START $group-$libc ####"',
        '    echo "#### OS COMP TEST GROUP END $group-$libc ####"',
        "}",
        "",
        "whusp_run_group() {",
        '    libc="$(whusp_libc_label "$2")"',
        '    /musl/busybox sh "$script_dir/$WHUSP_ARCH/$libc/groups/$1"',
        "}",
        "",
    ]

    for index, script in enumerate(all_tests):
        name = test_name(script)
        filename = group_filename(index, script)
        lines.extend(
            [
                f"if whusp_script_enabled {script}; then",
            ]
        )
        if script == "libctest_testcode.sh":
            lines.extend(
                [
                    "    whusp_skip_group libctest /glibc",
                    f"    whusp_run_group {filename} /musl",
                ]
            )
        else:
            lines.extend(
                [
                    "    for libc_root in $WHUSP_TEST_LIBCS; do",
                    f"        whusp_run_group {filename} \"$libc_root\"",
                    "    done",
                ]
            )
        lines.extend(
            [
                "else",
                "    for libc_root in $WHUSP_TEST_LIBCS; do",
                f"        whusp_skip_group {name} \"$libc_root\"",
                "    done",
                "fi",
                "",
            ]
        )
    lines.append("exit 0")
    return "\n".join(lines) + "\n"


def _write_outputs_unlocked(
    out_dir: Path,
    all_tests: list[str],
    test_scripts: list[str],
    libc_roots: list[str],
    ltp_cases: list[LtpCase],
    manifests: list[str],
    active_filter: str,
    interactive_shell: str,
    runtest_dir: Path,
    force: bool,
) -> None:
    prepare_out_dir(out_dir, force)
    write_executable(out_dir / "common.sh", common_script(manifests))
    write_executable(out_dir / "entry.sh", entry_script(all_tests, test_scripts, libc_roots))
    manifest_lines = [
        "kind\tarch\tlibc\tindex\tgroup_or_case\tsource\tline\tcommand\tcurrent_runner_enabled\tcurrent_runner_skips_this_libc"
    ]
    enabled_scripts = set(test_scripts)

    for arch in ARCHES:
        for libc_root in libc_roots:
            libc = libc_label(libc_root)
            root = out_dir / arch / libc
            group_dir = root / "groups"
            ltp_dir = root / "ltp-cases"
            group_dir.mkdir(parents=True)
            ltp_dir.mkdir()
            write_executable(root / "run_all_groups.sh", run_all_script("groups"))
            write_executable(root / "run_all_ltp_cases.sh", run_all_script("ltp-cases"))
            whitelist_path = root / "run_ltp_whitelist.sh"
            write_executable(whitelist_path, ltp_whitelist_script(arch, libc_root, ltp_cases))
            manifest_lines.append(
                "\t".join(
                    [
                        "ltp_whitelist",
                        arch,
                        libc,
                        "",
                        "ltp",
                        "ltp_whitelist.txt",
                        "",
                        str(whitelist_path.relative_to(out_dir)),
                        "whitelist",
                        "no",
                    ]
                )
            )

            for index, script in enumerate(all_tests):
                enabled = script in enabled_scripts
                runner_skipped = enabled and script == "libctest_testcode.sh" and libc_root == "/glibc"
                path = group_dir / group_filename(index, script)
                write_executable(
                    path,
                    group_script(arch, libc_root, script, index, enabled, runner_skipped),
                )
                manifest_lines.append(
                    "\t".join(
                        [
                            "group",
                            arch,
                            libc,
                            str(index),
                            test_name(script),
                            script,
                            "",
                            str(path.relative_to(out_dir)),
                            "yes" if enabled else "no",
                            "yes" if runner_skipped else "no",
                        ]
                    )
                )

            for case in ltp_cases:
                path = ltp_dir / ltp_case_filename(case)
                write_executable(path, ltp_case_script(arch, libc_root, case))
                manifest_lines.append(
                    "\t".join(
                        [
                            "ltp_case",
                            arch,
                            libc,
                            str(case.order),
                            case.name,
                            case.manifest,
                            str(case.manifest_line),
                            case.command,
                            "whitelist",
                            "no",
                        ]
                    )
                )

    (out_dir / "manifest.tsv").write_text("\n".join(manifest_lines) + "\n", encoding="utf-8")
    (out_dir / "ltp_manifest_order.txt").write_text("\n".join(manifests) + "\n", encoding="utf-8")
    try:
        runtest_display = str(runtest_dir.resolve().relative_to(REPO_ROOT))
    except ValueError:
        runtest_display = str(runtest_dir)

    readme = f"""# Contest Case Commands

Generated from:

- `os/src/task/contest_runner.rs`
- `os/src/task/ltp_whitelist.txt`
- `{runtest_display}`

This directory exports the guest-side commands for every contest test group in
`ALL_TESTS`, plus every current LTP whitelist case. It is a script view of the
runner command construction, not a new source of truth.

Current runner metadata:

- `INTERACTIVE_SHELL = {interactive_shell}`
- `TEST_LIBCS = {', '.join(libc_label(root) for root in libc_roots)}`
- `TEST_SCRIPTS = {', '.join(test_scripts) if test_scripts else '(empty)'}`
- `LTP_CASE_FILTER_OPTION = {active_filter}`
- LTP whitelist cases exported: {len(ltp_cases)}

Layout:

- `entry.sh`: script-disk entry point used by the kernel init command.
- `common.sh`: runtime and LTP helpers mirrored from `contest_runner.rs`.
- `rv/<libc>/groups/*.sh`: all RISC-V group commands for that libc root.
- `la/<libc>/groups/*.sh`: all LoongArch group commands for that libc root.
- `rv/<libc>/run_ltp_whitelist.sh`: unified RISC-V LTP whitelist runner.
- `la/<libc>/run_ltp_whitelist.sh`: unified LoongArch LTP whitelist runner.
- `rv/<libc>/ltp-cases/*.sh`: RISC-V LTP whitelist case commands.
- `la/<libc>/ltp-cases/*.sh`: LoongArch LTP whitelist case commands.
- `manifest.tsv`: one row per generated script or LTP command.

Run examples inside the guest filesystem:

```sh
/musl/busybox sh ./rv/musl/groups/000-basic.sh
/musl/busybox sh ./la/musl/groups/005-iperf.sh
/musl/busybox sh ./rv/glibc/run_ltp_whitelist.sh
/musl/busybox sh ./rv/glibc/ltp-cases/0012-execve05.sh
```

Regenerate after changing the runner or whitelist:

```sh
python3 scripts/export_contest_case_scripts.py --force
```
"""
    (out_dir / "README.md").write_text(readme, encoding="utf-8")


def write_outputs(
    out_dir: Path,
    all_tests: list[str],
    test_scripts: list[str],
    libc_roots: list[str],
    ltp_cases: list[LtpCase],
    manifests: list[str],
    active_filter: str,
    interactive_shell: str,
    runtest_dir: Path,
    force: bool,
) -> None:
    with acquire_output_lock(out_dir):
        _write_outputs_unlocked(
            out_dir,
            all_tests,
            test_scripts,
            libc_roots,
            ltp_cases,
            manifests,
            active_filter,
            interactive_shell,
            runtest_dir,
            force,
        )


def main() -> int:
    args = parse_args()
    runner_source = read_text(RUNNER_PATH)
    whitelist_source = read_text(WHITELIST_PATH)
    all_tests = rust_string_array(runner_source, "ALL_TESTS")
    test_scripts = rust_string_array(runner_source, "TEST_SCRIPTS")
    libc_roots = rust_string_array(runner_source, "TEST_LIBCS")
    manifests = rust_string_array(runner_source, "LTP_RUNTEST_MANIFESTS")
    whitelist = text_list(whitelist_source, str(WHITELIST_PATH.relative_to(REPO_ROOT)))
    active_filter = rust_const_value(runner_source, "LTP_CASE_FILTER_OPTION")
    interactive_shell = rust_const_value(runner_source, "INTERACTIVE_SHELL")
    ltp_cases = resolve_ltp_cases(manifests, whitelist, args.runtest_dir)
    write_outputs(
        args.out_dir,
        all_tests,
        test_scripts,
        libc_roots,
        ltp_cases,
        manifests,
        active_filter,
        interactive_shell,
        args.runtest_dir,
        args.force,
    )
    group_count = len(ARCHES) * len(libc_roots) * len(all_tests)
    ltp_count = len(ARCHES) * len(libc_roots) * len(ltp_cases)
    ltp_whitelist_count = len(ARCHES) * len(libc_roots)
    print(
        f"wrote {group_count} group scripts, {ltp_whitelist_count} unified LTP "
        f"whitelist scripts, and {ltp_count} LTP case scripts to {args.out_dir}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
