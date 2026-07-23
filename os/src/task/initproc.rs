use crate::fs::{File, OpenFlags, VfsNodeId, open_file};
use alloc::{string::String, vec, vec::Vec};

const BUSYBOX_PATH: &str = "/musl/busybox";
const BUSYBOX_APPLET: &str = "sh";
const SCRIPT_DISK_ENTRY: &str = "/x1/entry.sh";

pub(super) struct KernelInitProc {
    pub(super) path: String,
    pub(super) executable_node: Option<VfsNodeId>,
    pub(super) data: Vec<u8>,
    pub(super) argv: Vec<String>,
    pub(super) envp: Vec<String>,
}

pub(super) fn load() -> Option<KernelInitProc> {
    // CONTEXT: The kernel owns only the PID 1 ELF bootstrap and the stable
    // script-disk handoff path. Test selection, environment, diagnostics, and
    // shutdown policy live in the generated `/x1/entry.sh`.
    let inode = open_file(BUSYBOX_PATH, OpenFlags::RDONLY).ok()?;
    Some(KernelInitProc {
        path: BUSYBOX_PATH.into(),
        executable_node: inode.vfs_node_id(),
        data: inode.read_all(),
        argv: vec![
            BUSYBOX_PATH.into(),
            BUSYBOX_APPLET.into(),
            SCRIPT_DISK_ENTRY.into(),
        ],
        envp: Vec::new(),
    })
}
