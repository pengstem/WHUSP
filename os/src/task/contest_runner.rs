use alloc::string::String;

const INTERACTIVE_SHELL: bool = false;
// Script-disk handoff point. The kernel assembles only the environment and the
// final shutdown command; marker emission and per-suite shell logic live in
// this mounted entry script.
const SCRIPT_DISK_ENTRY: &str = "/x1/entry.sh";
// Exact perf marker strings are parsed by host log tooling. Keep them outside
// per-suite OS COMP group regions so perf output cannot split a judged group.
#[cfg(feature = "perf-counters")]
const PERF_COUNTER_DUMP_COMMAND: &str = "; echo '#### KERNEL PERF START ####'; /musl/busybox cat /proc/oskernel/perf; echo '#### KERNEL PERF END ####'";
#[cfg(not(feature = "perf-counters"))]
const PERF_COUNTER_DUMP_COMMAND: &str = "";

#[cfg(target_arch = "riscv64")]
const RUNNER_ARCH: &str = "rv";
#[cfg(target_arch = "loongarch64")]
const RUNNER_ARCH: &str = "la";

pub(super) fn build_runner_command() -> String {
    if INTERACTIVE_SHELL {
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
