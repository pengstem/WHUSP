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
    "fanout01",
    "fcntl14",
    "fcntl14_64",
    "filecapstest.sh",
    "find_portbundle",
    "force_erase.sh",
    "fork14",
    "fork_exec_loop",
    "fork_freeze.sh",
    "fou*.sh",
    "frag",
    "freeze*",
    "fs_fill",
    "fs_racer.sh",
    "fs_racer_dir_test.sh",
    "fs_racer_file_list.sh",
    "fsstress",
    "ftrace*",
    "getrusage04",
    "hackbench",
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

const WEIGHTED_LTP_CASES: &[&str] = &[
    "fcntl01",
    "fcntl01_64",
    "fcntl02",
    "fcntl02_64",
    "fcntl03",
    "fcntl03_64",
    "fcntl04",
    "fcntl04_64",
    "fcntl05",
    "fcntl05_64",
    "fcntl07",
    "fcntl07_64",
    "fcntl08",
    "fcntl08_64",
    "fcntl09",
    "fcntl09_64",
    "fcntl10",
    "fcntl10_64",
    "fcntl11",
    "fcntl11_64",
    "fcntl12",
    "fcntl12_64",
    "fcntl13",
    "fcntl13_64",
    "fcntl15",
    "fcntl15_64",
    "fcntl16",
    "fcntl16_64",
    "fcntl17",
    "fcntl17_64",
    "fcntl18",
    "fcntl18_64",
    "fcntl19",
    "fcntl19_64",
    "fcntl20",
    "fcntl20_64",
    "fcntl21",
    "fcntl21_64",
    "fcntl22",
    "fcntl22_64",
    "fcntl23",
    "fcntl23_64",
    "fcntl24",
    "fcntl24_64",
    "fcntl25",
    "fcntl25_64",
    "fcntl26",
    "fcntl26_64",
    "fcntl27",
    "fcntl27_64",
    "fcntl29",
    "fcntl29_64",
    "fcntl30",
    "fcntl30_64",
    "fcntl31",
    "fcntl31_64",
    "fcntl32",
    "fcntl32_64",
    "fcntl33",
    "fcntl33_64",
    "fcntl35",
    "fcntl35_64",
    "fcntl37",
    "fcntl37_64",
    "fcntl38",
    "fcntl38_64",
    "fcntl39",
    "fcntl39_64",
    "mmap001",
    "mmap01",
    "mmap02",
    "mmap03",
    "mmap04",
    "mmap05",
    "mmap06",
    "mmap08",
    "mmap09",
    "mmap10",
    "mmap12",
    "mmap13",
    "mmap14",
    "mmap15",
    "mmap16",
    "mmap17",
    "mmap18",
    "mmap19",
    "mmap20",
    "pipe01",
    "pipe02",
    "pipe03",
    "pipe04",
    "pipe05",
    "pipe06",
    "pipe07",
    "pipe08",
    "pipe09",
    "pipe10",
    "pipe11",
    "pipe12",
    "pipe13",
    "pipe14",
    "pipe15",
    "open01",
    "open02",
    "open03",
    "open04",
    "open06",
    "open07",
    "open08",
    "open09",
    "open10",
    "open11",
    "open12",
    "open13",
    "open14",
];

pub(super) fn build_runner_command() -> String {
    if WEIGHTED_LTP_CASES.first().is_some() {
        let mut command = String::new();
        let mut first = true;
        for libc_root in TEST_LIBCS {
            append_separator(&mut command, &mut first);
            append_weighted_ltp_runner(&mut command, libc_root);
        }
        command.push_str("; cd /musl && ./busybox reboot -f");
        return command;
    }

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

fn append_weighted_ltp_runner(command: &mut String, libc_root: &str) {
    command.push_str("cd ");
    command.push_str(libc_root);
    command.push_str(" && { ./busybox echo \"#### OS COMP TEST GROUP START ltp-");
    command.push_str(libc_label(libc_root));
    command.push_str(" ####\"; for case_name in ");
    for (index, case_name) in WEIGHTED_LTP_CASES.iter().enumerate() {
        if index > 0 {
            command.push(' ');
        }
        command.push_str(case_name);
    }
    command.push_str("; do if [ ! -x \"./ltp/testcases/bin/$case_name\" ]; then ./busybox echo \"SKIP LTP CASE $case_name\"; continue; fi; ./busybox echo \"RUN LTP CASE $case_name\"; ./ltp/testcases/bin/$case_name; ret=$?; ./busybox echo \"FAIL LTP CASE $case_name : $ret\"; done; ./busybox echo \"#### OS COMP TEST GROUP END ltp-");
    command.push_str(libc_label(libc_root));
    command.push_str(" ####\"; }");
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
    command.push_str(" && { ./busybox echo \"#### OS COMP TEST GROUP START ltp-");
    command.push_str(libc_label(libc_root));
    command.push_str(" ####\"; for file in ltp/testcases/bin/*; do [ -f \"$file\" ] || continue; case_name=${file##*/}; case \"$case_name\" in ");
    append_ltp_blacklist_patterns(command);
    command.push_str(") echo \"SKIP LTP CASE $case_name\"; continue ;; esac; echo \"RUN LTP CASE $case_name\"; \"$file\"; ret=$?; echo \"FAIL LTP CASE $case_name : $ret\"; done; ./busybox echo \"#### OS COMP TEST GROUP END ltp-");
    command.push_str(libc_label(libc_root));
    command.push_str(" ####\"; }");
}

fn append_ltp_blacklist_patterns(command: &mut String) {
    for (index, pattern) in LTP_BLACKLIST_PATTERNS.iter().enumerate() {
        if index > 0 {
            command.push('|');
        }
        command.push_str(pattern);
    }
}

fn libc_label(libc_root: &str) -> &str {
    match libc_root {
        "/musl" => "musl",
        "/glibc" => "glibc",
        _ => "unknown",
    }
}
