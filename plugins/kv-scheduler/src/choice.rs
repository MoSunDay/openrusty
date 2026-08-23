//! Pure helpers for the kv-scheduler plugin (host-unit-testable).

use alloc::string::String;
use alloc::vec::Vec;

/// Where the task key lives inside the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtractRule {
    /// `query:<param>` -- urlencoded query parameter.
    QueryParam(String),
    /// `path:<n>` -- 1-based path segment.
    PathSegment(usize),
    /// `body:<field>` -- top-level JSON string field of the request body.
    BodyField(String),
}

/// Parse the `extract` plugin setting (`query:<param>`, `path:<n>` or
/// `body:<field>`).
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
    if let Some(field) = cfg.strip_prefix("body:") {
        let field = field.trim();
        if field.is_empty() {
            return None;
        }
        return Some(ExtractRule::BodyField(String::from(field)));
    }
    None
}

/// Extract the first JSON string value whose key is exactly `"<field>"`.
///
/// Scanning is textual, not tree-aware: the first occurrence of the
/// quoted field name that is followed by `:` and a string literal wins,
/// even if it sits inside a nested object. Occurrences whose value is
/// not a string literal (e.g. `"cache_salt":123`) or that fail to decode
/// are skipped; when none succeed the result is `None`.
///
/// The quoted needle prevents substring keys (e.g. `xcache_salt`) from
/// matching, and escapes inside string values keep embedded mentions of
/// the name from matching (JSON requires inner quotes to be escaped).
pub fn json_string_field(body: &[u8], field: &str) -> Option<Vec<u8>> {
    if field.is_empty() {
        return None;
    }
    let mut needle = Vec::with_capacity(field.len() + 2);
    needle.push(b'"');
    needle.extend_from_slice(field.as_bytes());
    needle.push(b'"');

    let mut start = 0usize;
    while let Some(rel) = find_subslice(&body[start..], &needle) {
        let pos = start + rel;
        if let Some(val) = string_value_after(body, pos + needle.len()) {
            return Some(val);
        }
        start = pos + 1;
    }
    None
}

/// First offset of `needle` inside `hay`, or `None`.
fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Decode the string literal that must appear after `from`, allowing
/// arbitrary whitespace and exactly one `:` between the key and the
/// opening quote. Returns the decoded bytes.
fn string_value_after(body: &[u8], from: usize) -> Option<Vec<u8>> {
    let mut i = skip_ws(body, from);
    if i >= body.len() || body[i] != b':' {
        return None;
    }
    i = skip_ws(body, i + 1);
    if i >= body.len() || body[i] != b'"' {
        return None;
    }
    decode_string(body, i + 1)
}

/// Decode the JSON string literal starting just after its opening quote
/// (`start`), handling the standard escapes. `\uXXXX` decodes BMP code
/// points only; surrogate code points (D800-DFFF) are rejected since
/// re-encoding a lone surrogate is invalid UTF-8.
fn decode_string(body: &[u8], start: usize) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = start;
    while i < body.len() {
        match body[i] {
            b'"' => return Some(out),
            b'\\' => {
                i += 1;
                if i >= body.len() {
                    return None;
                }
                match body[i] {
                    b'"' => out.push(b'"'),
                    b'\\' => out.push(b'\\'),
                    b'/' => out.push(b'/'),
                    b'b' => out.push(0x08),
                    b'f' => out.push(0x0C),
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b't' => out.push(b'\t'),
                    b'u' => {
                        if i + 4 >= body.len() {
                            return None;
                        }
                        let cp = parse_hex4(&body[i + 1..i + 5])?;
                        if (0xD800..=0xDFFF).contains(&cp) {
                            return None;
                        }
                        let mut buf = [0u8; 4];
                        let s = char::from_u32(cp)?.encode_utf8(&mut buf);
                        out.extend_from_slice(s.as_bytes());
                        i += 4;
                    }
                    _ => return None,
                }
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    None // unterminated string
}

/// Skip ASCII whitespace used by JSON (space, tab, LF, CR).
fn skip_ws(body: &[u8], mut i: usize) -> usize {
    while i < body.len() && matches!(body[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

/// Parse exactly four ASCII hex digits.
fn parse_hex4(hex: &[u8]) -> Option<u32> {
    if hex.len() != 4 {
        return None;
    }
    let mut v: u32 = 0;
    for &b in hex {
        let d = match b {
            b'0'..=b'9' => u32::from(b - b'0'),
            b'a'..=b'f' => u32::from(b - b'a' + 10),
            b'A'..=b'F' => u32::from(b - b'A' + 10),
            _ => return None,
        };
        v = (v << 4) | d;
    }
    Some(v)
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
        assert_eq!(
            parse_extract("body:cache_salt"),
            Some(ExtractRule::BodyField(String::from("cache_salt")))
        );
        assert_eq!(
            parse_extract(" body: cache_salt "),
            Some(ExtractRule::BodyField(String::from("cache_salt")))
        );
        assert_eq!(parse_extract("header:x-task"), None);
        assert_eq!(parse_extract("query:"), None);
        assert_eq!(parse_extract("path:0"), None);
        assert_eq!(parse_extract("path:abc"), None);
        assert_eq!(parse_extract("body:"), None);
        assert_eq!(parse_extract("body:  "), None);
        assert_eq!(parse_extract(""), None);
    }

    #[test]
    fn extracts_body_string_field() {
        let body = br#"{"model":"llama","cache_salt":"agent:s1"}"#;
        assert_eq!(
            json_string_field(body, "cache_salt"),
            Some(b"agent:s1".to_vec())
        );
    }

    #[test]
    fn tolerates_whitespace_around_colon() {
        let body = br#"{ "cache_salt"  :  "agent:s2" }"#;
        assert_eq!(
            json_string_field(body, "cache_salt"),
            Some(b"agent:s2".to_vec())
        );
    }

    #[test]
    fn decodes_escapes_in_value() {
        let body = br#"{"cache_salt":"a\"b\\c\/\n\t\u00e9"}"#;
        // \" \\ \/ \n \t and é (U+00E9) -> UTF-8 0xC3 0xA9
        assert_eq!(
            json_string_field(body, "cache_salt"),
            Some(b"a\"b\\c/\n\t\xC3\xA9".to_vec())
        );
    }

    #[test]
    fn missing_field_is_none() {
        assert_eq!(json_string_field(br#"{"other":"x"}"#, "cache_salt"), None);
        assert_eq!(json_string_field(br#"{}"#, "cache_salt"), None);
        assert_eq!(json_string_field(br#""#, "cache_salt"), None);
    }

    #[test]
    fn non_string_value_is_skipped() {
        let body = br#"{"cache_salt":123,"cache_salt":"real"}"#;
        assert_eq!(
            json_string_field(body, "cache_salt"),
            Some(b"real".to_vec())
        );
        assert_eq!(json_string_field(br#"{"cache_salt":123}"#, "cache_salt"), None);
        assert_eq!(json_string_field(br#"{"cache_salt":null}"#, "cache_salt"), None);
        assert_eq!(json_string_field(br#"{"cache_salt":{"x":"y"}}"#, "cache_salt"), None);
    }

    #[test]
    fn substring_field_names_do_not_match() {
        let body = br#"{"xcache_salt":"no","cache_salt_extra":"no"}"#;
        assert_eq!(json_string_field(body, "cache_salt"), None);
        // The value of another key mentioning the quoted name cannot
        // match either (JSON escapes inner quotes).
        let body = br#"{"msg":"\"cache_salt\": fake","cache_salt":"yes"}"#;
        assert_eq!(
            json_string_field(body, "cache_salt"),
            Some(b"yes".to_vec())
        );
    }

    #[test]
    fn first_occurrence_wins_even_when_nested() {
        // No nesting awareness by design: the inner object's field is
        // seen first and returned.
        let body = br#"{"a":{"cache_salt":"inner"},"cache_salt":"outer"}"#;
        assert_eq!(
            json_string_field(body, "cache_salt"),
            Some(b"inner".to_vec())
        );
    }

    #[test]
    fn rejects_surrogate_escapes_and_malformed_strings() {
        assert_eq!(
            json_string_field(br#"{"cache_salt":"\ud800"}"#, "cache_salt"),
            None
        );
        assert_eq!(
            json_string_field(br#"{"cache_salt":"unterminated"#, "cache_salt"),
            None
        );
        assert_eq!(
            json_string_field(br#"{"cache_salt":"bad\q"}"#, "cache_salt"),
            None
        );
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
