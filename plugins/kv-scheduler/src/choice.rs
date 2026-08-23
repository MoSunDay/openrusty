//! Pure helpers for the kv-scheduler plugin (host-unit-testable).

use alloc::string::String;

/// Where the task key lives inside the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtractRule {
    /// `query:<param>` -- urlencoded query parameter.
    QueryParam(String),
    /// `path:<n>` -- 1-based path segment.
    PathSegment(usize),
}

/// Parse the `extract` plugin setting (`query:<param>` or `path:<n>`).
pub fn parse_extract(cfg: &str) -> Option<ExtractRule> {
    let cfg = cfg.trim();
    if let Some(param) = cfg.strip_prefix("query:") {
        let param = param.trim();
        if param.is_empty() {
            return None;
        }
        return Some(ExtractRule::QueryParam(String::from(param)));
    }
    if let Some(n) = cfg.strip_prefix("path:") {
        let n = parse_ascii_u64(n.trim().as_bytes())? as usize;
        if n == 0 {
            return None;
        }
        return Some(ExtractRule::PathSegment(n));
    }
    None
}

/// KV key holding the affinity record (peer index) of a task.
pub fn aff_key(task: &str) -> String {
    let mut out = String::with_capacity(task.len() + 4);
    out.push_str("aff:");
    out.push_str(task);
    out
}

/// KV key holding the last-scheduled timestamp (ms) of peer `idx`.
pub fn sched_key(idx: usize) -> String {
    let mut out = String::from("sched:");
    out.push_str(&ascii_u64(idx as u64));
    out
}

/// Parse ASCII decimal bytes; reject anything else (including empty).
pub fn parse_ascii_u64(bytes: &[u8]) -> Option<u64> {
    if bytes.is_empty() {
        return None;
    }
    let mut v: u64 = 0;
    for &b in bytes {
        if !b.is_ascii_digit() {
            return None;
        }
        v = v.checked_mul(10)?.checked_add(u64::from(b - b'0'))?;
    }
    Some(v)
}

/// Encode `v` as ASCII decimal without pulling in fmt machinery.
pub fn ascii_u64(mut v: u64) -> String {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    // SAFETY: the slice contains ASCII digits only.
    unsafe { String::from_utf8_unchecked(buf[i..].to_vec()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_extract_rules() {
        assert_eq!(
            parse_extract("query:task_id"),
            Some(ExtractRule::QueryParam(String::from("task_id")))
        );
        assert_eq!(parse_extract("path:2"), Some(ExtractRule::PathSegment(2)));
        assert_eq!(parse_extract(" query: t "), Some(ExtractRule::QueryParam(String::from("t"))));
        assert_eq!(parse_extract("header:x-task"), None);
        assert_eq!(parse_extract("query:"), None);
        assert_eq!(parse_extract("path:0"), None);
        assert_eq!(parse_extract("path:abc"), None);
        assert_eq!(parse_extract(""), None);
    }

    #[test]
    fn keys_are_prefixed() {
        assert_eq!(aff_key("t1"), "aff:t1");
        assert_eq!(sched_key(3), "sched:3");
    }

    #[test]
    fn parses_ascii_numbers() {
        assert_eq!(parse_ascii_u64(b"0"), Some(0));
        assert_eq!(parse_ascii_u64(b"42"), Some(42));
        assert_eq!(parse_ascii_u64(b"18446744073709551615"), Some(u64::MAX));
        assert_eq!(parse_ascii_u64(b""), None);
        assert_eq!(parse_ascii_u64(b"12a"), None);
        assert_eq!(parse_ascii_u64(b"-1"), None);
        assert_eq!(parse_ascii_u64(b"18446744073709551616"), None); // overflow
    }

    #[test]
    fn encodes_ascii_numbers() {
        assert_eq!(ascii_u64(0), "0");
        assert_eq!(ascii_u64(7), "7");
        assert_eq!(ascii_u64(123456789), "123456789");
        assert_eq!(ascii_u64(u64::MAX), "18446744073709551615");
    }

    #[test]
    fn number_roundtrip() {
        for v in [0u64, 1, 9, 10, 255, 1_000_000, u64::MAX] {
            assert_eq!(parse_ascii_u64(ascii_u64(v).as_bytes()), Some(v));
        }
    }
}
