const LTP_CASE_WHITELIST_TEXT: &str = include_str!("ltp_whitelist.txt");

pub(super) fn ltp_case_whitelist_len() -> usize {
    LTP_CASE_WHITELIST_TEXT
        .lines()
        .filter(|line| {
            let line = line.trim();
            !line.is_empty() && !line.starts_with('#')
        })
        .count()
}
