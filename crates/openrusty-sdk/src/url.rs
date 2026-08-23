//! Tiny pure URL helpers (application/x-www-form-urlencoded style).
//!
//! Percent-decoding covers `+` -> space and `%XX` hex escapes; malformed
//! escapes pass through unchanged. All functions are pure and
//! host-unit-testable.

use alloc::string::String;
use alloc::vec::Vec;

/// Value of the first `name` parameter in a urlencoded query string,
/// percent-decoded. Parameters without `=` have an empty value.
pub fn query_param(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (key, val) = match pair.split_once('=') {
            Some(kv) => kv,
            None => (pair, ""),
        };
        (key == name).then(|| decode(val))
    })
}

/// The `n`-th (1-based) non-empty path segment, percent-decoded.
/// Leading slashes and duplicate slashes are ignored.
pub fn path_segment(path: &str, n: usize) -> Option<String> {
    if n == 0 {
        return None;
    }
    path.split('/')
        .filter(|s| !s.is_empty())
        .nth(n - 1)
        .map(decode)
}

/// Decode `+` as space and `%XX` hex escapes; anything else passes through.
/// Output is UTF-8 (lossy on invalid byte sequences).
pub fn decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => match (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                (Some(hi), Some(lo)) => {
                    out.push((hi << 4) | lo);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_parameter() {
        assert_eq!(
            query_param("a=1&task=hello&b=2", "task"),
            Some(String::from("hello"))
        );
        assert_eq!(
            query_param("task=first&task=second", "task"),
            Some(String::from("first"))
        );
    }

    #[test]
    fn missing_parameter_is_none() {
        assert_eq!(query_param("a=1&b=2", "task"), None);
        assert_eq!(query_param("", "task"), None);
        // Prefixes of other names must not match.
        assert_eq!(query_param("task_id=9", "task"), None);
    }

    #[test]
    fn decodes_plus_and_percent() {
        assert_eq!(query_param("task=a+b", "task"), Some(String::from("a b")));
        assert_eq!(
            query_param("task=%41%42%20c", "task"),
            Some(String::from("AB c"))
        );
        assert_eq!(
            query_param("task=%2Frun%2F", "task"),
            Some(String::from("/run/"))
        );
    }

    #[test]
    fn malformed_escapes_pass_through() {
        assert_eq!(query_param("task=%zz", "task"), Some(String::from("%zz")));
        assert_eq!(query_param("task=%4", "task"), Some(String::from("%4")));
        assert_eq!(query_param("task=100%", "task"), Some(String::from("100%")));
    }

    #[test]
    fn empty_and_valueless_parameters() {
        assert_eq!(query_param("task=", "task"), Some(String::from("")));
        assert_eq!(query_param("task", "task"), Some(String::from("")));
    }

    #[test]
    fn picks_path_segments_one_based() {
        assert_eq!(path_segment("/v1/tasks/42", 1), Some(String::from("v1")));
        assert_eq!(path_segment("/v1/tasks/42", 2), Some(String::from("tasks")));
        assert_eq!(path_segment("/v1/tasks/42", 3), Some(String::from("42")));
        assert_eq!(path_segment("/v1/tasks/42", 4), None);
        assert_eq!(path_segment("/v1/tasks/42", 0), None);
    }

    #[test]
    fn ignores_empty_segments_and_decodes() {
        assert_eq!(path_segment("//a//b%20c/", 2), Some(String::from("b c")));
        assert_eq!(path_segment("", 1), None);
        assert_eq!(path_segment("/", 1), None);
    }
}
