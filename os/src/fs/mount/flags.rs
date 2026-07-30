pub(super) const MOUNT_STAT_RDONLY: u64 = 0x0001;
pub(super) const MOUNT_STAT_NOSUID: u64 = 0x0002;
pub(super) const MOUNT_STAT_NODEV: u64 = 0x0004;
pub(super) const MOUNT_STAT_NOEXEC: u64 = 0x0008;
pub(super) const MOUNT_STAT_VALID: u64 = 0x0020;
pub(super) const MOUNT_STAT_NOATIME: u64 = 0x0400;
pub(super) const MOUNT_STAT_NODIRATIME: u64 = 0x0800;
pub(super) const MOUNT_STAT_NOSYMFOLLOW: u64 = 0x2000;

// Linux mount(2) input flags accepted by this VFS layer. They are translated
// into the statfs/mountinfo view, not into per-open file status flags.
const LINUX_MS_RDONLY: usize = 1;
const LINUX_MS_NOSUID: usize = 1 << 1;
const LINUX_MS_NODEV: usize = 1 << 2;
const LINUX_MS_NOEXEC: usize = 1 << 3;
const LINUX_MS_NOSYMFOLLOW: usize = 1 << 8;
const LINUX_MS_NOATIME: usize = 1 << 10;
const LINUX_MS_NODIRATIME: usize = 1 << 11;

pub(super) fn normalize_mount_stat_flags(flags: u64) -> u64 {
    flags | MOUNT_STAT_VALID
}

pub(super) fn mount_flags_have_nosymfollow(flags: u64) -> bool {
    flags & MOUNT_STAT_NOSYMFOLLOW != 0
}

pub(super) fn mount_flags_from_options(options: &str) -> u64 {
    let mut flags = MOUNT_STAT_VALID;
    if options.split(',').any(|option| option == "ro") {
        flags |= MOUNT_STAT_RDONLY;
    }
    flags
}

pub(crate) fn mount_stat_flags_from_linux_mount_flags(flags: usize) -> u64 {
    let mut stat_flags = MOUNT_STAT_VALID;
    if flags & LINUX_MS_RDONLY != 0 {
        stat_flags |= MOUNT_STAT_RDONLY;
    }
    if flags & LINUX_MS_NOSUID != 0 {
        stat_flags |= MOUNT_STAT_NOSUID;
    }
    if flags & LINUX_MS_NODEV != 0 {
        stat_flags |= MOUNT_STAT_NODEV;
    }
    if flags & LINUX_MS_NOEXEC != 0 {
        stat_flags |= MOUNT_STAT_NOEXEC;
    }
    if flags & LINUX_MS_NOSYMFOLLOW != 0 {
        stat_flags |= MOUNT_STAT_NOSYMFOLLOW;
    }
    if flags & LINUX_MS_NOATIME != 0 {
        stat_flags |= MOUNT_STAT_NOATIME;
    }
    if flags & LINUX_MS_NODIRATIME != 0 {
        stat_flags |= MOUNT_STAT_NODIRATIME;
    }
    stat_flags
}

pub(super) fn mount_options_from_flags(flags: u64) -> &'static str {
    let read_only = flags & MOUNT_STAT_RDONLY != 0;
    if read_only { "ro" } else { "rw" }
}
