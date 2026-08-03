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

fn defaults_to_noatime(fs_type: &str) -> bool {
    matches!(
        fs_type,
        "ext2" | "ext3" | "ext4" | "vfat" | "tmpfs" | "ramfs" | "overlay"
    )
}

pub(super) fn normalize_mount_stat_flags(fs_type: &str, flags: u64) -> u64 {
    let mut flags = flags | MOUNT_STAT_VALID;
    // CONTEXT: Ordinary contest filesystems deliberately use noatime and
    // nodiratime in every build. Their read backends do not synthesize atime
    // changes, so keeping these bits set makes statfs and /proc mount views
    // describe the actual behavior even after a remount request.
    if defaults_to_noatime(fs_type) {
        flags |= MOUNT_STAT_NOATIME | MOUNT_STAT_NODIRATIME;
    }
    flags
}

pub(super) fn mount_flags_have_nosymfollow(flags: u64) -> bool {
    flags & MOUNT_STAT_NOSYMFOLLOW != 0
}

pub(super) fn mount_flags_from_options(fs_type: &str, options: &str) -> u64 {
    let mut flags = MOUNT_STAT_VALID;
    for option in options.split(',') {
        match option {
            "ro" => flags |= MOUNT_STAT_RDONLY,
            "noatime" => flags |= MOUNT_STAT_NOATIME,
            "nodiratime" => flags |= MOUNT_STAT_NODIRATIME,
            _ => {}
        }
    }
    normalize_mount_stat_flags(fs_type, flags)
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
    let noatime = flags & MOUNT_STAT_NOATIME != 0;
    let nodiratime = flags & MOUNT_STAT_NODIRATIME != 0;
    match (read_only, noatime, nodiratime) {
        (false, false, false) => "rw",
        (false, true, false) => "rw,noatime",
        (false, false, true) => "rw,nodiratime",
        (false, true, true) => "rw,noatime,nodiratime",
        (true, false, false) => "ro",
        (true, true, false) => "ro,noatime",
        (true, false, true) => "ro,nodiratime",
        (true, true, true) => "ro,noatime,nodiratime",
    }
}
