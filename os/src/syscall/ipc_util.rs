pub(crate) fn now_sec() -> i64 {
    (crate::timer::wall_time_nanos() / 1_000_000_000) as i64
}

pub(crate) fn pid_to_i32(pid: usize) -> i32 {
    pid.try_into().unwrap_or(i32::MAX)
}
