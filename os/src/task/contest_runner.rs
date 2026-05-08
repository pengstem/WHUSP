use alloc::{format, string::String};

const TEST_LIBCS: &[&str] = &["/glibc", "/musl"];

const INTERACTIVE_SHELL: bool = false;

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

const TEST_SCRIPTS: &[&str] = &[
    // perfect
    // "basic_testcode.sh",
    // runnable
    // "busybox_testcode.sh",
    // perfect
    // "lua_testcode.sh",
    // runnable
    // "libctest_testcode.sh",
    // runnable
    // "iozone_testcode.sh",
    // runnable
    // "iperf_testcode.sh",
    // "libcbench_testcode.sh",
    // "lmbench_testcode.sh",
    // runnable
    // "netperf_testcode.sh",
    // runnable
    // "cyclictest_testcode.sh",
    "ltp_testcode.sh",
];

const LTP_BLACKLIST_PATTERNS: &[&str] = &[
    "*-lib.sh",
    "*_helper",
    "*_lib.sh",
    "*lib.sh",
    "ask_password.sh",
    "assign_password.sh",
    "bbr*.sh",
    "bind06",
    "bind_noport01.sh",
    "binfmt_misc*",
    "block_dev",
    "broken_ip-*",
    "busy_poll*",
    "cacheflush01",
    "can_*",
    "cap_bounds_*",
    "cap_bset_inh_bounds",
    "cfs_bandwidth01",
    "cgroup_*",
    "change_password.sh",
    "check_*",
    "clock_settime03",
    "cpuacct*",
    "cpufreq_boost",
    "cpuctl_*",
    "cpuhotplug*",
    "cpuset*",
    "create_datafile",
    "create_file",
    "cve-2017-17052",
    "data",
    "datafiles",
    "dccp*",
    "delete_module02",
    "dhcp*",
    "dirtyc0w*",
    "dns*",
    "doio",
    "ebizzy",
    "eject*",
    "event_generator",
    "epoll_pwait01",
    "fanotify13",
    "fanout01",
    "fallocate04",
    "fallocate05",
    "fallocate06",
    "fs_di",
    "fs_inod",
    "fs_perms",
    "ftest03",
    "ftp-download-stress02-rmt.sh",
    "fcntl14",
    "fcntl14_64",
    "filecapstest.sh",
    "find_portbundle",
    "force_erase.sh",
    "fork09",
    "fork13",
    "fork14",
    "fork_exec_loop",
    "fork_freeze.sh",
    "fou*.sh",
    "frag",
    "freeze*",
    "fs_fill",
    "fsconfig01",
    "fsconfig02",
    "fsconfig03",
    "fsmount01",
    "fsmount02",
    "fsopen01",
    "fsopen02",
    "fspick01",
    "fspick02",
    "fs_racer.sh",
    "fs_racer_dir_create.sh",
    "fs_racer_dir_test.sh",
    "fs_racer_file_concat.sh",
    "fs_racer_file_create.sh",
    "fs_racer_file_link.sh",
    "fs_racer_file_list.sh",
    "fs_racer_file_rename.sh",
    "fs_racer_file_rm.sh",
    "fs_racer_file_symlink.sh",
    "fsstress",
    "fsx-linux",
    "fsx.sh",
    "fsync04",
    "ftp-download-stress.sh",
    "ftp-download-stress01-rmt.sh",
    "ftp-upload-stress.sh",
    "ftp-upload-stress01-rmt.sh",
    "ftp-upload-stress02-rmt.sh",
    "ftp01.sh",
    "ftrace*",
    "ftruncate04",
    "ftruncate04_64",
    "futex_waitv*",
    "futex_wake04",
    "futimesat01",
    "fw_load",
    "fgetxattr01",
    "fsetxattr01",
    "fsetxattr02",
    "gettimeofday02",
    "getrusage04",
    "hackbench",
    "inode02",
    "ipsec*",
    "iptables*",
    "kcmp03",
    "keyctl05",
    "lftest",
    "ltpClient",
    "ltpServer",
    "ltpSockets.sh",
    "ltp_acpi",
    "mallocstress",
    "mcast*",
    "mmap-corruption01",
    "mmap1",
    "mmap2",
    "mmap3",
    "mmapstress03",
    "mmapstress05",
    "mmapstress10",
    "memcg_test_2",
    "memcg_test_4",
    "mmstress",
    "mmstress_dummy",
    "mremap01",
    "mremap02",
    "mremap03",
    "mremap04",
    "mremap05",
    "mpls*",
    "open_by_handle_at*",
    "open_tree*",
    "openfile",
    "pids_task2",
    "pivot_root*",
    "pthserv",
    "ptrace*",
    "remap_file_pages*",
    "route*",
    "run_sched_cliserv.sh",
    "sctp*",
    "shm_test",
    "shmat1",
    "tcp_cc*",
    "test_*",
    "timed_forkbomb",
    "tracepath01.sh",
    "traceroute01.sh",
    "tst_*",
    "udp4-*",
    "udp6-*",
    "udp_ipsec*",
    "uevent*",
    "umip_basic_test",
    "unshare01.sh",
    "userns*",
    "verify_caps_exec",
    "vfork*",
    "vhangup*",
    "virt_lib.sh",
    "vlan*.sh",
    "vma*.sh",
    "vma02",
    "vma03",
    "vma04",
    "vma05_vdso",
    "vsock01",
    "vxlan*.sh",
    "wc01.sh",
    "which01.sh",
    "wireguard*",
    "write_freezing.sh",
    "writev03",
    "zram*",
    // Host, privileged, namespace, cgroup, or device-environment families.
    // Interactive MMC password helpers.
    // LTP helper/library files that are not standalone test cases.
    // Network test helpers and topology-dependent suites.
    // Stress, freeze, or known hang/error cases seen in reference runners.
];

const LTP_MUSL_BLACKLIST_PATTERNS: &[&str] = &[
    // CONTEXT: RISC-V musl implements epoll_create(size) by calling
    // epoll_create1(0), so the kernel cannot distinguish invalid size values
    // from the valid epoll_create1(0) path checked by epoll_create1_01.
    "epoll_create02",
];

// None runs all non-blacklisted cases. Some("a")..Some("z") narrows by
// leading letter, Some("long") runs names outside the ASCII alphabet,
// Some("case:<name>") runs one exact LTP case, Some("cases:<a>,<b>") runs
// selected exact LTP cases, and Some("prefix:<name>") runs cases whose names
// start with the prefix.
const LTP_CASE_FILTER_OPTION: Option<&str> = Some("f");

enum LtpCaseFilter {
    All,
    Letter(u8),
    Long,
    Exact(&'static str),
    ExactSet(&'static str),
    Prefix(&'static str),
    Invalid,
}

pub(super) fn build_runner_command() -> String {
    if INTERACTIVE_SHELL || TEST_SCRIPTS.is_empty() {
        return "/musl/busybox sh".into();
    }
    let mut command = String::new();
    let mut first = true;
    for test in ALL_TESTS {
        if !TEST_SCRIPTS.contains(test) {
            let testname = test.strip_suffix("_testcode.sh").unwrap_or(test);
            append_skipped_group_markers(&mut command, &mut first, testname);
        }
    }

    for script in TEST_SCRIPTS {
        for libc_root in TEST_LIBCS {
            append_separator(&mut command, &mut first);
            append_script_command(&mut command, libc_root, script);
        }
    }
    command.push_str("; cd /musl && ./busybox reboot -f");
    command
}

fn append_separator(command: &mut String, first: &mut bool) {
    if *first {
        *first = false;
    } else {
        command.push_str("; ");
    }
}

fn append_skipped_group_markers(command: &mut String, first: &mut bool, testname: &str) {
    for libc_root in TEST_LIBCS {
        let libc = libc_label(libc_root);
        append_separator(command, first);
        command.push_str(&format!(
            "echo '#### OS COMP TEST GROUP START {testname}-{libc} ####'"
        ));
        append_separator(command, first);
        command.push_str(&format!(
            "echo '#### OS COMP TEST GROUP END {testname}-{libc} ####'"
        ));
    }
}

fn append_script_command(command: &mut String, libc_root: &str, script: &str) {
    if script == "ltp_testcode.sh" {
        append_ltp_runner(command, libc_root);
    } else {
        append_normal_script(command, libc_root, script);
    }
}

fn append_normal_script(command: &mut String, libc_root: &str, script: &str) {
    command.push_str("cd ");
    command.push_str(libc_root);
    command.push_str(" && ");
    if script == "lmbench_testcode.sh" {
        command.push_str("./busybox rm -f /tmp/hello; ");
    }
    command.push_str("./busybox sh ./");
    command.push_str(script);
    if script == "lmbench_testcode.sh" {
        command.push_str("; ./busybox rm -f /tmp/hello");
    }
}

fn append_ltp_runner(command: &mut String, libc_root: &str) {
    command.push_str("cd ");
    command.push_str(libc_root);
    // CONTEXT: The fs_bind-focused runner is bounded by the outer QEMU
    // timeout. LA BusyBox ash currently loses the caller-local timeout value in
    // LTP's shell helper, so disable LTP's per-case shell timer for this pass.
    command.push_str(" && { export LTPROOT=\"");
    command.push_str(libc_root);
    command.push_str(
        "/ltp\"; export TMPBASE=\"/tmp\"; export TST_TIMEOUT=\"-1\"; export PATH=\"$PATH:$LTPROOT/testcases/bin:$LTPROOT/bin\"; ./busybox echo \"#### OS COMP TEST GROUP START ltp-",
    );
    command.push_str(libc_label(libc_root));
    command.push_str(" ####\"; cd \"$LTPROOT/testcases/bin\"; for file in *; do [ -f \"$file\" ] || continue; case_name=${file##*/}; ");
    append_ltp_case_filter(command);
    command.push_str("case \"$case_name\" in ");
    append_ltp_blacklist_patterns(command, libc_root);
    // CONTEXT: The autotest parser consumes the historical
    // "FAIL LTP CASE ... : <ret>" record as a per-case result line. A zero
    // return still means the case passed, so keep the text stable here.
    command.push_str(") echo \"SKIP LTP CASE $case_name\"; continue ;; esac; echo \"RUN LTP CASE $case_name\"; \"./$case_name\"; ret=$?; echo \"FAIL LTP CASE $case_name : $ret\"; done; \"");
    command.push_str(libc_root);
    command.push_str("/busybox\" echo \"#### OS COMP TEST GROUP END ltp-");
    command.push_str(libc_label(libc_root));
    command.push_str(" ####\"; }");
}

fn append_ltp_case_filter(command: &mut String) {
    match ltp_case_filter() {
        LtpCaseFilter::All => {}
        LtpCaseFilter::Letter(letter) => {
            command.push_str("case \"$case_name\" in [");
            command.push(letter as char);
            command.push((letter as char).to_ascii_uppercase());
            command.push_str("]*) ;; *) continue ;; esac; ");
        }
        LtpCaseFilter::Long => {
            command.push_str("case \"$case_name\" in [A-Za-z]*) continue ;; esac; ");
        }
        LtpCaseFilter::Exact(case_name) => {
            command.push_str("case \"$case_name\" in ");
            command.push_str(case_name);
            command.push_str(") ;; *) continue ;; esac; ");
        }
        LtpCaseFilter::ExactSet(case_names) => {
            command.push_str("case \"$case_name\" in ");
            append_ltp_case_set_pattern(command, case_names);
            command.push_str(") ;; *) continue ;; esac; ");
        }
        LtpCaseFilter::Prefix(prefix) => {
            command.push_str("case \"$case_name\" in ");
            command.push_str(prefix);
            command.push_str("*) ;; *) continue ;; esac; ");
        }
        LtpCaseFilter::Invalid => {
            command.push_str("./busybox echo \"INVALID LTP_CASE_FILTER_OPTION\"; break; ");
        }
    }
}

fn ltp_case_filter() -> LtpCaseFilter {
    match LTP_CASE_FILTER_OPTION {
        None => LtpCaseFilter::All,
        Some(option) if option.eq_ignore_ascii_case("long") => LtpCaseFilter::Long,
        Some(option) if option.starts_with("case:") => {
            let case_name = &option["case:".len()..];
            if is_ltp_case_name(case_name) {
                LtpCaseFilter::Exact(case_name)
            } else {
                LtpCaseFilter::Invalid
            }
        }
        Some(option) if option.starts_with("cases:") => {
            let case_names = &option["cases:".len()..];
            if case_names
                .split(',')
                .all(|case_name| is_ltp_case_name(case_name))
            {
                LtpCaseFilter::ExactSet(case_names)
            } else {
                LtpCaseFilter::Invalid
            }
        }
        Some(option) if option.starts_with("prefix:") => {
            let prefix = &option["prefix:".len()..];
            if is_ltp_case_name(prefix) {
                LtpCaseFilter::Prefix(prefix)
            } else {
                LtpCaseFilter::Invalid
            }
        }
        Some(option) => {
            let bytes = option.as_bytes();
            if bytes.len() == 1 && bytes[0].is_ascii_alphabetic() {
                LtpCaseFilter::Letter(bytes[0].to_ascii_lowercase())
            } else {
                LtpCaseFilter::Invalid
            }
        }
    }
}

fn is_ltp_case_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn append_ltp_case_set_pattern(command: &mut String, case_names: &str) {
    let mut first = true;
    for case_name in case_names.split(',') {
        if !first {
            command.push('|');
        }
        first = false;
        command.push_str(case_name);
    }
}

fn append_ltp_blacklist_patterns(command: &mut String, libc_root: &str) {
    let mut first = true;
    for pattern in LTP_BLACKLIST_PATTERNS {
        if !first {
            command.push('|');
        }
        first = false;
        command.push_str(pattern);
    }
    if libc_root == "/musl" {
        for pattern in LTP_MUSL_BLACKLIST_PATTERNS {
            if !first {
                command.push('|');
            }
            first = false;
            command.push_str(pattern);
        }
    }
}

fn libc_label(libc_root: &str) -> &str {
    match libc_root {
        "/musl" => "musl",
        "/glibc" => "glibc",
        _ => "unknown",
    }
}
