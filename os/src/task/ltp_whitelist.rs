pub(super) const LTP_CASE_WHITELIST_GLIBC: &[&str] = &[
    "faccessat01",
    "faccessat02",
    "faccessat201",
    "faccessat202",
    "open_tree01",
    "open_tree02,",
    "move_mount01",
    "move_mount02,",
    "fsopen01",
    "fsopen02,",
    "fsconfig01",
    "fsconfig02,fsconfig03,",
    "fsmount01",
    "fsmount02",
];

pub(super) const LTP_CASE_WHITELIST_MUSL: &[&str] = &[
    "faccessat01",
    "faccessat02",
    "faccessat201",
    "faccessat202",
    "open_tree01",
    "open_tree02,",
    "move_mount01",
    "move_mount02,",
    "fsopen01",
    "fsopen02,",
    "fsconfig01",
    "fsconfig02,fsconfig03,",
    "fsmount01",
    "fsmount02",
];

pub(super) fn ltp_case_whitelist(libc_root: &str) -> &'static [&'static str] {
    match libc_root {
        "/glibc" => LTP_CASE_WHITELIST_GLIBC,
        "/musl" => LTP_CASE_WHITELIST_MUSL,
        _ => &[],
    }
}
