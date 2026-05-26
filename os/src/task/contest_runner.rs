use alloc::{format, string::String};

// CONTEXT: The judge-facing runner emits libc-suffixed marker groups. Keep
// both roots here even when a disk script has a per-libc exception.
const TEST_LIBCS: &[&str] = &["/glibc", "/musl"];
// CONTEXT: Search the manifests that contain the current whitelist, ordered by
// duplicate-resolution priority. The generated script disk uses this same list
// for whitelist export and runtime filter scans.
const LTP_RUNTEST_MANIFESTS: &[&str] = &[
    "syscalls",
    "fs",
    "input",
    "net.ipv6_lib",
    "fs_bind",
    "crypto",
    "pty",
    "hugetlb",
    "watchqueue",
    "containers",
    "smoketest",
    "cve",
    "mm",
];

const INTERACTIVE_SHELL: bool = false;
const SCRIPT_DISK_ENTRY: &str = "/x1/entry.sh";

// CONTEXT: `ALL_TESTS` is the marker universe. Disabled groups still emit
// START/END pairs from the script disk so scorer logs stay aligned.
const ALL_TESTS: &[&str] = &[
    "basic_testcode.sh",
    "busybox_testcode.sh",
    "lua_testcode.sh",
    "libctest_testcode.sh",
    "iozone_testcode.sh",
    "iperf_testcode.sh",
    "libcbench_testcode.sh",
    "lmbench_testcode.sh",
    "netperf_testcode.sh",
    "cyclictest_testcode.sh",
    "ltp_testcode.sh",
];

// CONTEXT: This remains the kernel-owned default group selection. The script
// disk receives it through WHUSP_TEST_SCRIPTS and prints skipped markers for
// groups not listed here.
const TEST_SCRIPTS: &[&str] = &[
    // "basic_testcode.sh",
    // "busybox_testcode.sh",
    // "lua_testcode.sh",
    // "libctest_testcode.sh",
    "ltp_testcode.sh",
    // "iozone_testcode.sh",
    // "iperf_testcode.sh",
    // "libcbench_testcode.sh",
    // "netperf_testcode.sh",
    // "cyclictest_testcode.sh",
    // "lmbench_testcode.sh",
];

/// None runs the current libc's curated whitelist from ltp_whitelist.txt.
/// Some("a")..Some("z") narrows by leading letter, Some("long") runs names
/// outside the ASCII alphabet, Some("case:<name>") runs one exact LTP case,
/// Some("cases:<a>,<b>") runs selected exact LTP cases, Some("prefix:<name>")
/// runs cases whose names start with the prefix, and
/// Some("range:<start>,<end>") runs cases in the lexicographic half-open range
/// [start, end). Empty range bounds are unbounded.
// CONTEXT: Non-None filters are development slices. They narrow LTP case
// execution while leaving outer group markers intact, so always check this
// constant before treating a score log as submission-wide evidence.
const LTP_CASE_FILTER_OPTION: Option<&str> = Some("prefix:fchown");

#[cfg(target_arch = "riscv64")]
const RUNNER_ARCH: &str = "rv";
#[cfg(target_arch = "loongarch64")]
const RUNNER_ARCH: &str = "la";

pub(super) fn build_runner_command() -> String {
    if INTERACTIVE_SHELL || TEST_SCRIPTS.is_empty() {
        return interactive_shell_command();
    }

    let mut command = String::new();
    append_export(&mut command, "WHUSP_ARCH", RUNNER_ARCH);
    append_export(
        &mut command,
        "WHUSP_ALL_TESTS",
        joined_words(ALL_TESTS).as_str(),
    );
    append_export(
        &mut command,
        "WHUSP_TEST_SCRIPTS",
        joined_words(TEST_SCRIPTS).as_str(),
    );
    append_export(
        &mut command,
        "WHUSP_TEST_LIBCS",
        joined_words(TEST_LIBCS).as_str(),
    );
    append_export(
        &mut command,
        "WHUSP_LTP_MANIFESTS",
        joined_words(LTP_RUNTEST_MANIFESTS).as_str(),
    );
    append_export(
        &mut command,
        "WHUSP_LTP_FILTER_OPTION",
        ltp_filter_option_value(),
    );
    append_export(
        &mut command,
        "WHUSP_LTP_WHITELIST_LEN",
        format!("{}", super::ltp_whitelist::ltp_case_whitelist_len()).as_str(),
    );
    command.push_str("; if [ -f ");
    command.push_str(SCRIPT_DISK_ENTRY);
    command.push_str(" ]; then /musl/busybox sh ");
    command.push_str(SCRIPT_DISK_ENTRY);
    command.push_str("; else echo 'contest script disk entry missing: ");
    command.push_str(SCRIPT_DISK_ENTRY);
    command.push_str("'; fi; cd /musl && ./busybox sync; ./busybox reboot -f");
    command
}

fn interactive_shell_command() -> String {
    "/musl/busybox mkdir -p /tmp/bin && /musl/busybox --install -s /tmp/bin; export PATH=/tmp/bin:/musl:/glibc:$PATH && cd /musl && exec /musl/busybox sh".into()
}

fn ltp_filter_option_value() -> &'static str {
    match LTP_CASE_FILTER_OPTION {
        Some(option) => option,
        None => "None",
    }
}

fn append_export(command: &mut String, key: &str, value: &str) {
    if !command.is_empty() {
        command.push_str("; ");
    }
    command.push_str("export ");
    command.push_str(key);
    command.push('=');
    command.push_str(shell_quote(value).as_str());
}

fn joined_words(words: &[&str]) -> String {
    let mut output = String::new();
    for word in words {
        if !output.is_empty() {
            output.push(' ');
        }
        output.push_str(word);
    }
    output
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
