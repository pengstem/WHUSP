use crate::fs::{OpenFlags, open_file};
use alloc::{string::String, vec, vec::Vec};
use core::fmt::Write;

const BUSYBOX_PATH: &str = "/musl/busybox";
const BUSYBOX_APPLET: &str = "sh";
const BUSYBOX_COMMAND_FLAG: &str = "-c";
const TEST_LIBCS: &[&str] = &["/musl", "/glibc"];
const TEST: bool = false;
const ALL_TESTS: &[&str] = &[
    "basic_testcode.sh",
    "busybox_testcode.sh",
    "lua_testcode.sh",
    "libctest_testcode.sh",
    "iozone_testcode.sh",
    "unixbench_testcode.sh",
    "iperf_testcode.sh",
    "libcbench_testcode.sh",
    "lmbench_testcode.sh",
    "netperf_testcode.sh",
    "cyclictest_testcode.sh",
    "ltp_testcode.sh",
];
// CONTEXT: temporary - only libctest enabled for pthread bringup; restore all before merge.
const TEST_SCRIPTS: &[&str] = &[
    // perfect
    "basic_testcode.sh",
    //runable
    "busybox_testcode.sh",
    //perfect
    "lua_testcode.sh",
    //runalbe
    "libctest_testcode.sh",
    //runalbe
    "iozone_testcode.sh",
    //runable
    "unixbench_testcode.sh",
    //runalbe
    "iperf_testcode.sh",
    // "libcbench_testcode.sh",
    // "lmbench_testcode.sh",
    //runalbe
    "netperf_testcode.sh",
    //runalbe
    "cyclictest_testcode.sh",
    // "ltp_testcode.sh",
];

pub(super) struct KernelInitProc {
    pub(super) path: String,
    pub(super) data: Vec<u8>,
    pub(super) argv: Vec<String>,
    pub(super) envp: Vec<String>,
}

fn build_runner_command() -> String {
    let mut command =
        String::from("/musl/busybox mkdir -p /bin && /musl/busybox --install -s /bin");

    for test in ALL_TESTS {
        if !TEST_SCRIPTS.contains(test) {
            let testname = test.strip_suffix("_testcode.sh").unwrap_or(test);
            let _ = write!(
                command,
                "; echo '#### OS COMP TEST GROUP START {testname}-musl ####' "
            );
            let _ = write!(
                command,
                "; echo '#### OS COMP TEST GROUP END {testname}-musl ####' "
            );
            let _ = write!(
                command,
                "; echo '#### OS COMP TEST GROUP START {testname}-glibc ####' "
            );
            let _ = write!(
                command,
                "; echo '#### OS COMP TEST GROUP END {testname}-glibc ####' "
            );
        }
    }
    if TEST_SCRIPTS.is_empty() || TEST {
        let _ = write!(command, ";(sh)");
    } else {
        for script in TEST_SCRIPTS {
            for libc_root in TEST_LIBCS {
                let _ = write!(command, "; (cd {libc_root} && ./busybox sh ./{script})");
            }
        }
        let _ = write!(command, "; (cd /musl && ./busybox reboot -f)");
    }
    command
}

pub(super) fn load() -> Option<KernelInitProc> {
    let inode = open_file(BUSYBOX_PATH, OpenFlags::RDONLY).ok()?;
    Some(KernelInitProc {
        path: BUSYBOX_PATH.into(),
        data: inode.read_all(),
        argv: vec![
            BUSYBOX_PATH.into(),
            BUSYBOX_APPLET.into(),
            BUSYBOX_COMMAND_FLAG.into(),
            build_runner_command(),
        ],
        envp: vec![
            "PATH=/:/bin:/sbin:/usr/bin:/usr/local/bin".into(),
            "LD_LIBRARY_PATH=/glibc/lib:/musl/lib:/lib".into(),
        ],
    })
}
