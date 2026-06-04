use alloc::string::String;

// CONTEXT: The script-disk exporter reads these plan constants and bakes the
// run/skip sequence into `/x1/entry.sh`. The kernel runtime only selects the
// architecture and invokes that generated entry point.
#[allow(dead_code)]
const TEST_LIBCS: &[&str] = &["/glibc", "/musl"];
// CONTEXT: Search the manifests that contain the current whitelist, ordered by
// duplicate-resolution priority. The generated script disk uses this same list
// for whitelist export and runtime filter scans.
#[allow(dead_code)]
const LTP_RUNTEST_MANIFESTS: &[&str] = &[
    "syscalls",
    "syscalls-ipc",
    "fs",
    "input",
    "net.features",
    "net.ipv6_lib",
    "net.tcp_cmds",
    "net_stress.broken_ip",
    "net_stress.interface",
    "net_stress.route",
    "fs_bind",
    "crypto",
    "pty",
    "hugetlb",
    "watchqueue",
    "containers",
    "smoketest",
    "cve",
    "mm",
    "fs_readonly",
];

const INTERACTIVE_SHELL: bool = false;
// Script-disk handoff point. The kernel assembles only the environment and the
// final shutdown command; marker emission and per-suite shell logic live in
// this mounted entry script.
const SCRIPT_DISK_ENTRY: &str = "/x1/entry.sh";
#[cfg(feature = "perf-counters")]
const PERF_COUNTER_DUMP_COMMAND: &str = "; echo '#### KERNEL PERF START ####'; /musl/busybox cat /proc/oskernel/perf; echo '#### KERNEL PERF END ####'";
#[cfg(not(feature = "perf-counters"))]
const PERF_COUNTER_DUMP_COMMAND: &str = "";

// CONTEXT: `ALL_TESTS` is the marker universe baked into the generated script
// disk. Disabled groups still emit START/END pairs so scorer logs stay aligned.
#[allow(dead_code)]
const ALL_TESTS: &[&str] = &[
    "basic_testcode.sh",
    "busybox_testcode.sh",
    "lua_testcode.sh",
    "libctest_testcode.sh",
    "ltp_testcode.sh",
    "iozone_testcode.sh",
    "iperf_testcode.sh",
    "libcbench_testcode.sh",
    "lmbench_testcode.sh",
    "cyclictest_testcode.sh",
    "netperf_testcode.sh",
];

// CONTEXT: This remains the source of truth for script-disk generation. The
// generated entry script hardcodes the resulting run/skip sequence instead of
// rechecking the list before every group at runtime.
const TEST_SCRIPTS: &[&str] = &[
    "basic_testcode.sh",
    "busybox_testcode.sh",
    "lua_testcode.sh",
    "libctest_testcode.sh",
    // "ltp_testcode.sh",
    "iozone_testcode.sh",
    "iperf_testcode.sh",
    "libcbench_testcode.sh",
    "netperf_testcode.sh",
    "cyclictest_testcode.sh",
    "lmbench_testcode.sh",
];

/// None runs the current libc's curated whitelist from ltp_whitelist.txt.
/// Some("a")..Some("z") narrows by leading letter, Some("long") runs names
/// outside the ASCII alphabet, Some("case:<name>") runs one exact LTP case,
/// Some("cases:<a>,<b>") runs selected exact LTP cases, Some("prefix:<name>")
/// runs cases whose names start with the prefix, and
/// Some("range:<start>,<end>") runs cases in the lexicographic half-open range
/// [start, end). Empty range bounds are unbounded.
// CONTEXT: Non-None filters are development slices. They narrow LTP case
// execution in the generated script disk while leaving outer group markers
// intact, so always check this constant before treating a score log as
// submission-wide evidence.
#[allow(dead_code)]
const LTP_CASE_FILTER_OPTION: Option<&str> = None;

#[cfg(target_arch = "riscv64")]
const RUNNER_ARCH: &str = "rv";
#[cfg(target_arch = "loongarch64")]
const RUNNER_ARCH: &str = "la";

pub(super) fn build_runner_command() -> String {
    if INTERACTIVE_SHELL || TEST_SCRIPTS.is_empty() {
        return interactive_shell_command();
    }

    let mut command = String::new();
    // Keep the runtime ABI narrow: test selection, libc order, LTP manifests,
    // and filters are consumed by the host-side script exporter.
    append_export(&mut command, "WHUSP_ARCH", RUNNER_ARCH);
    // Keep a missing script disk visible in the serial log and still run the
    // final sync/reboot path; otherwise a host-side x1 wiring problem looks
    // like an in-kernel test hang.
    command.push_str("; if [ -f ");
    command.push_str(SCRIPT_DISK_ENTRY);
    command.push_str(" ]; then /musl/busybox sh ");
    command.push_str(SCRIPT_DISK_ENTRY);
    command.push_str("; else echo 'contest script disk entry missing: ");
    command.push_str(SCRIPT_DISK_ENTRY);
    command.push_str("'; fi");
    command.push_str(PERF_COUNTER_DUMP_COMMAND);
    command.push_str("; cd /musl && ./busybox sync; ./busybox reboot -f");
    command
}

fn interactive_shell_command() -> String {
    "/musl/busybox mkdir -p /tmp/bin && /musl/busybox --install -s /tmp/bin; export PATH=/tmp/bin:/musl:/glibc:$PATH && cd /musl && exec /musl/busybox sh".into()
}

fn append_export(command: &mut String, key: &str, value: &str) {
    if !command.is_empty() {
        command.push_str("; ");
    }
    // Keep quoting centralized so future runtime exports cannot accidentally
    // introduce shell syntax through the kernel-built command line.
    command.push_str("export ");
    command.push_str(key);
    command.push('=');
    command.push_str(shell_quote(value).as_str());
}

fn shell_quote(value: &str) -> String {
    let mut quoted = String::from("'");
    for byte in value.bytes() {
        if byte == b'\'' {
            quoted.push_str("'\"'\"'");
        } else {
            quoted.push(byte as char);
        }
    }
    quoted.push('\'');
    quoted
}
