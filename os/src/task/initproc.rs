use crate::fs::{OpenFlags, open_file};
use alloc::{string::String, vec, vec::Vec};

const BUSYBOX_PATH: &str = "/musl/busybox";
const BUSYBOX_APPLET: &str = "sh";

pub(super) struct KernelInitProc {
    pub(super) path: String,
    pub(super) data: Vec<u8>,
    pub(super) argv: Vec<String>,
    pub(super) envp: Vec<String>,
}

pub(super) fn load() -> Option<KernelInitProc> {
    let inode = open_file(BUSYBOX_PATH, OpenFlags::RDONLY)?;
    Some(KernelInitProc {
        path: BUSYBOX_PATH.into(),
        data: inode.read_all(),
        argv: vec![BUSYBOX_PATH.into(), BUSYBOX_APPLET.into()],
        envp: Vec::new(),
    })
}
