mod ext4;
mod inode;
mod mount;
mod path;
mod pipe;
mod stdio;

use crate::mm::UserBuffer;

pub trait File: Send + Sync {
    fn readable(&self) -> bool;
    fn writable(&self) -> bool;
    fn read(&self, buf: UserBuffer) -> usize;
    fn write(&self, buf: UserBuffer) -> usize;
    fn working_dir(&self) -> Option<WorkingDir> {
        None
    }
}

pub fn init() {
    mount::init_mounts();
}

pub fn list_apps() {
    mount::mount_status_log();
    println!("/**** APPS ****");
    for app in mount::list_root_apps() {
        println!("{}", app);
    }
    println!("**************/")
}

pub(crate) use inode::open_file_at;
pub use inode::{OpenFlags, open_file};
pub(crate) use inode::{lookup_dir_at, mkdir_at, unlink_file_at};
pub(crate) use path::{WorkingDir, normalize_path};
pub use pipe::make_pipe;
pub use stdio::{Stdin, Stdout};
