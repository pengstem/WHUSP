use crate::fs::{OpenFlags, open_file};
use alloc::{string::String, vec, vec::Vec};

const BUSYBOX_PATH: &str = "/musl/busybox";
const BUSYBOX_APPLET: &str = "sh";
#[allow(unused)]
const BUSYBOX_COMMAND_FLAG: &str = "-c";
#[allow(unused)]
const BASIC_MUSL_RUNNER: &str = "cd /musl && ./busybox sh ./basic_testcode.sh";

pub(super) struct KernelInitProc {
    pub(super) path: String,
    pub(super) data: Vec<u8>,
    pub(super) argv: Vec<String>,
    pub(super) envp: Vec<String>,
}

pub(super) fn load() -> Option<KernelInitProc> {
    let inode = open_file(BUSYBOX_PATH, OpenFlags::RDONLY).ok()?;
    Some(KernelInitProc {
        path: BUSYBOX_PATH.into(),
        data: inode.read_all(),
        argv: vec![
            BUSYBOX_PATH.into(),
            BUSYBOX_APPLET.into(),
            // BUSYBOX_COMMAND_FLAG.into(),
            // BASIC_MUSL_RUNNER.into(),
        ],
        envp: Vec::new(),
    })
}
