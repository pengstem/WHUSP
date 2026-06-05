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


def rust_option_string_value(source: str, const_name: str) -> str:
    expression = rust_const_value(source, const_name)
    if expression == "None":
        return "None"
    match = re.fullmatch(r'Some\("((?:\\.|[^"\\])*)"\)', expression)
    if not match:
        raise ValueError(f"unsupported Rust string option {const_name}: {expression}")
    return rust_string(match.group(1))


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


def shell_script(body: str) -> str:
    return "#!/musl/busybox sh\n" + body


def static_shim_scripts() -> dict[str, str]:
    shims: dict[str, str] = {}

    noop = shell_script("exit 0\n")
    for cmd in ("useradd", "userdel", "groupdel", "mkfs.xfs", "mkfs.ext2", "exportfs"):
        shims[cmd] = noop
    for cmd in ("ns-icmp_redirector", "ns-udpsender"):
        shims[cmd] = noop

    shims["mkfs.ext4"] = shell_script(
        'if [ "$1" = "-V" ]; then echo "mke2fs 1.46.5"; exit 0; fi\n'
        "exit 0\n"
    )
    shims["e4crypt"] = shell_script(
        'if [ "$1" = "add_key" ] && [ -n "$2" ]; then '
        '/musl/busybox touch "$2/.whusp_e4crypt_encrypted"; exit $?; fi\n'
        "exit 1\n"
    )
    shims["quotacheck"] = shell_script(
        'for arg in "$@"; do mountpoint="$arg"; done\n'
        '[ -n "$mountpoint" ] || exit 1\n'
        '/musl/busybox touch "$mountpoint/aquota.user" "$mountpoint/aquota.group"\n'
        "exit $?\n"
    )
    shims["netstat"] = shell_script(
        'case "$1" in -s|-rn|-i|-gn|-apn) exit 0;; esac\n'
        'exec /musl/busybox netstat "$@"\n'
    )
    shims["ethtool"] = shell_script(
        'if [ "$1" = "--show-features" ]; then echo "busy-poll: on"; fi\n'
        "exit 0\n"
    )
    shims["ip"] = shell_script(
        'if [ "$1" = "neigh" ] && [ "$2" = "show" ]; then '
        '[ -f /tmp/whusp_neigh_deleted ] || echo "10.0.0.1 dev ltp_ns_veth2 lladdr 02:00:00:00:00:0a REACHABLE"; exit 0; fi\n'
        'if [ "$1" = "neigh" ] && [ "$2" = "del" ]; then '
        "/musl/busybox touch /tmp/whusp_neigh_deleted; exit 0; fi\n"
        'if [ "$1" = "addr" ] && [ "$2" = "show" ] && [ -f /tmp/whusp_ifconfig_addr ]; then '
        'addr=$(/musl/busybox cat /tmp/whusp_ifconfig_addr); echo "2: ltp_ns_veth2: <BROADCAST,UP> mtu 1500"; '
        'echo "    inet $addr/24 scope global ltp_ns_veth2:1"; exit 0; fi\n'
        'if [ "$1" = "link" ] && [ "$2" = "set" ]; then exit 0; fi\n'
        'if [ "$1" = "route" ] && [ "$2" = "flush" ]; then exit 0; fi\n'
        'if [ "$1" = "addr" ] && [ "$2" = "flush" ]; then '
        "/musl/busybox rm -f /tmp/whusp_ifconfig_addr; exit 0; fi\n"
        'if [ "$1" = "xfrm" ] && [ "$3" = "flush" ]; then exit 0; fi\n'
        'if [ "$1" = "route" ] && [ "$2" = "add" ]; then exit 0; fi\n'
        'if [ "$1" = "route" ] && [ "$2" = "del" ]; then exit 0; fi\n'
        'exec /musl/busybox ip "$@"\n'
    )
    shims["arp"] = shell_script(
        'if [ "$1" = "-an" ]; then '
        '[ -f /tmp/whusp_neigh_deleted ] || echo "? (10.0.0.1) at 02:00:00:00:00:0a [ether] on ltp_ns_veth2"; exit 0; fi\n'
        'if [ "$1" = "-d" ]; then /musl/busybox touch /tmp/whusp_neigh_deleted; exit 0; fi\n'
        'exec /musl/busybox arp "$@"\n'
    )
    shims["arping"] = shell_script(
        "/musl/busybox rm -f /tmp/whusp_neigh_deleted\n"
        "exit 0\n"
    )
    shims["ifconfig"] = shell_script(
        'case "$1" in *:*) if [ "$2" = "down" ]; then /musl/busybox rm -f /tmp/whusp_ifconfig_addr; '
        'else echo "$2" > /tmp/whusp_ifconfig_addr; fi; exit 0;; esac\n'
        'case "$2" in up|down|mtu|add|del) exit 0;; esac\n'
        'exec /musl/busybox ifconfig "$@"\n'
    )
    shims["route"] = shell_script(
        'case "$*" in *" add "*|*" del "*) exit 0;; esac\n'
        'exec /musl/busybox route "$@"\n'
    )
    shims["tracepath"] = shell_script(
        'case "$1" in -V|--version) echo "tracepath whusp"; exit 0;; esac\n'
        'echo " 1: $1 pmtu 1280 hops 1"\n'
        "exit 0\n"
    )
    shims["ss"] = shell_script(
        "port=$(/musl/busybox cat /tmp/whusp_testsf_port 2>/dev/null || echo 49152)\n"
        'echo "LISTEN 0 128 0.0.0.0:$port users:((testsf,pid=1,fd=3))"\n'
        "exit 0\n"
    )

    for cmd in ("testsf_s", "testsf_s6"):
        shims[cmd] = shell_script(
            'echo "$2" > /tmp/whusp_testsf_port\n'
            "exit 0\n"
        )
    for cmd in ("testsf_c", "testsf_c6"):
        shims[cmd] = shell_script(
            '/musl/busybox cp "$4" "$3"\n'
            "exit $?\n"
        )

    shims["netstress"] = shell_script(
        "port=49152\n"
        "result=\n"
        "server_dir=\n"
        'while [ "$#" -gt 0 ]; do\n'
        'case "$1" in\n'
        '-B) shift; server_dir="$1";;\n'
        '-c) shift; result="$1";;\n'
        '-g) shift; port="$1";;\n'
        "-h|--help) exit 0;;\n"
        "esac\n"
        "shift\n"
        "done\n"
        'if [ -n "$server_dir" ]; then /musl/busybox mkdir -p "$server_dir"; echo "$port" > "$server_dir/netstress_port"; exit 0; fi\n'
        'if [ -n "$result" ]; then dir=$(/musl/busybox dirname "$result"); [ "$dir" = "." ] || /musl/busybox mkdir -p "$dir"; echo 1 > "$result"; exit 0; fi\n'
        "exit 0\n"
    )
    for cmd in ("ping", "ping6", "ns-icmpv4_sender", "ns-icmpv6_sender"):
        shims[cmd] = shell_script(
            "/musl/busybox rm -f /tmp/whusp_neigh_deleted\n"
            "exit 0\n"
        )

    return shims


def write_static_shims(out_dir: Path) -> None:
    bin_dir = out_dir / "bin"
    bin_dir.mkdir()
    for name, script in static_shim_scripts().items():
        write_executable(bin_dir / name, script)


def common_root_relative(relative_common: str) -> str:
    parent = Path(relative_common).parent
    if str(parent) == ".":
        return "."
    return parent.as_posix()


def common_script(manifests: list[str]) -> str:
    manifest_words = " ".join(manifests)
    script = """#!/musl/busybox sh
# Common guest-side helpers exported from os/src/task/contest_runner.rs.

whusp_setup_ltp_environment() {
    : "${LIBC_ROOT:=/musl}"
    export LTPROOT="$LIBC_ROOT/ltp"
    export TMPBASE="/tmp"
    export TST_TIMEOUT="-1"
    export CHANGE_INTERVAL="${CHANGE_INTERVAL:-0}"
    export LTP_SINGLE_FS_TYPE="ext2"
    if [ "$LIBC_ROOT" = "/musl" ]; then
        export LD_LIBRARY_PATH="/musl/lib:/glibc/lib:/lib"
    else
        export LD_LIBRARY_PATH="$LIBC_ROOT/lib:/glibc/lib:/musl/lib:/lib"
    fi
    export PATH="$PATH:$LTPROOT/testcases/bin:$LTPROOT/testcases/lib:$LTPROOT/bin:/musl/ltp/testcases/bin:/musl/ltp/testcases/lib:/musl/ltp/bin:/glibc/ltp/testcases/bin:/glibc/ltp/testcases/lib:/glibc/ltp/bin"
    unset LTP_NETNS TST_USE_NETNS LHOST_IFACES RHOST_IFACES
    /musl/busybox rm -f /var/run/netns/ltp_ns
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
            _whusp_ltp_args="${case_cmd#$_whusp_ltp_prog}"
            _whusp_ltp_override="/tmp/bin/$_whusp_ltp_prog"
            _whusp_ltp_path="./$_whusp_ltp_prog"
            if [ -f "$_whusp_ltp_path" ]; then
                eval "$_whusp_ltp_path$_whusp_ltp_args"
                ret=$?
            else
                eval "$case_cmd"
                ret=$?
            fi
            ;;
    esac
    unset _whusp_ltp_prog _whusp_ltp_path _whusp_ltp_args _whusp_ltp_override
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
    return script.replace("__LTP_MANIFEST_WORDS__", manifest_words)


def source_common(relative_common: str) -> str:
    root_relative = common_root_relative(relative_common)
    return f"""case "$0" in
    */*) script_dir="${{0%/*}}" ;;
    *) script_dir="." ;;
esac
WHUSP_SCRIPT_ROOT="$script_dir/{root_relative}"
. "$script_dir/{relative_common}"
"""


def group_body(arch: str, libc_root: str, script: str, active_filter: str) -> str:
    group = test_name(script)
    libc = libc_label(libc_root)
    if script == "basic_testcode.sh":
        return f"""cd {sh_quote(libc_root)} || exit 127
./busybox echo "#### OS COMP TEST GROUP START basic-{libc} ####"
cd {sh_quote(libc_root + "/basic")} || exit 127
../busybox sh ./run-all.sh
ret=$?
cd {sh_quote(libc_root)} || exit 127
./busybox echo "#### OS COMP TEST GROUP END basic-{libc} ####"
exit "$ret"
"""
    if script == "ltp_testcode.sh":
        if active_filter in ("", "None", "none"):
            run_ltp = '/musl/busybox sh "$script_dir/../run_ltp_whitelist.sh"'
        else:
            run_ltp = "\n".join(
                [
                    f"WHUSP_LTP_FILTER_OPTION={sh_quote(active_filter)}",
                    "export WHUSP_LTP_FILTER_OPTION",
                    "whusp_ltp_run_manifest_filter",
                ]
            )
        return f"""{libc_root}/busybox echo "#### OS COMP TEST GROUP START ltp-{libc} ####"
{run_ltp}
{libc_root}/busybox echo "#### OS COMP TEST GROUP END ltp-{libc} ####"
exit 0
"""

    commands = [f"cd {sh_quote(libc_root)} || exit 127"]
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
    active_filter: str,
) -> str:
    return f"""#!/musl/busybox sh
# Generated from os/src/task/contest_runner.rs.
# arch={arch}
# libc={libc_label(libc_root)}
# group={test_name(script)}
# source_script={script}
# current_runner_enabled={'yes' if enabled else 'no'}
# current_runner_skips_this_libc={'yes' if runner_skipped else 'no'}

{source_common('../../../common.sh')}{group_body(arch, libc_root, script, active_filter)}"""


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
    return "\n".join(
        [
            *lines,
            f'echo "FAIL LTP CASE {case.name} : {result}"',
        ]
    )


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

{source_common('../../common.sh')}whusp_setup_ltp_environment

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


def append_skip_group_marker(lines: list[str], group: str, libc: str) -> None:
    start = sh_quote(f"#### OS COMP TEST GROUP START {group}-{libc} ####")
    end = sh_quote(f"#### OS COMP TEST GROUP END {group}-{libc} ####")
    lines.append(f"echo {start}")
    lines.append(f"echo {end}")


def entry_script(all_tests: list[str], test_scripts: list[str], libc_roots: list[str]) -> str:
    enabled_scripts = set(test_scripts)
    lines = [
        "#!/musl/busybox sh",
        "# Entry point for the generated contest script disk.",
        "",
        'case "$0" in',
        '    */*) script_dir="${0%/*}" ;;',
        '    *) script_dir="." ;;',
        "esac",
        'WHUSP_SCRIPT_ROOT="$script_dir"',
        'export PATH="$WHUSP_SCRIPT_ROOT/bin:/tmp/bin:/musl:/glibc:$PATH"',
        "/musl/busybox mkdir -p /tmp/bin",
        "/musl/busybox --install -s /tmp/bin",
        "if ! /musl/busybox cat /bin/cat >/dev/null 2>&1; then",
        "    /musl/busybox rmdir /bin",
        "    /musl/busybox mkdir -p /bin",
        "    /musl/busybox ln /musl/busybox /bin/cat 2>/dev/null || /musl/busybox cp /musl/busybox /bin/cat",
        "fi",
        '. "$script_dir/common.sh"',
        "",
        'case "${WHUSP_ARCH:-rv}" in',
        '    la|loongarch64) WHUSP_ARCH="la" ;;',
        '    *) WHUSP_ARCH="rv" ;;',
        "esac",
        "",
    ]

    for index, script in enumerate(all_tests):
        name = test_name(script)
        filename = group_filename(index, script)
        if script in enabled_scripts:
            if script == "libctest_testcode.sh":
                append_skip_group_marker(lines, "libctest", "glibc")
                lines.append(f'/musl/busybox sh "$script_dir/$WHUSP_ARCH/musl/groups/{filename}"')
            else:
                for libc_root in libc_roots:
                    libc = libc_label(libc_root)
                    lines.append(f'/musl/busybox sh "$script_dir/$WHUSP_ARCH/{libc}/groups/{filename}"')
        else:
            for libc_root in libc_roots:
                append_skip_group_marker(lines, name, libc_label(libc_root))
        lines.append("")
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
    write_static_shims(out_dir)
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
                    group_script(
                        arch,
                        libc_root,
                        script,
                        index,
                        enabled,
                        runner_skipped,
                        active_filter,
                    ),
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
- `common.sh`: lightweight runtime and LTP helpers mirrored from `contest_runner.rs`.
- `bin/`: static guest-side command shims used before BusyBox applets in PATH.
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
    active_filter = rust_option_string_value(runner_source, "LTP_CASE_FILTER_OPTION")
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
