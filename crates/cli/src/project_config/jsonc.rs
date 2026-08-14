//! A minimal JSONC reader: comments and trailing commas, nothing else.
//!
//! WHY NOT A CRATE. `jsonc-parser` and `json_spanned_value` exist and the
//! proposal expected one of them (2026-08-14-project-config.md 5.2). Two things
//! made a ~120-line module the better trade:
//!
//! 1. The scope invariant (proposal 1.2) wants the parser confined to
//!    `crates/cli`, and `tests/project_config_gate.sh` asserts it. A dependency
//!    that no crate declares is a stronger form of "confined" than one declared
//!    in exactly one Cargo.toml, and it cannot be pulled in by indirection.
//! 2. The writeback (5.2) is splice-only and needs BYTE SPANS in the ORIGINAL
//!    text, not in a normalised parse tree. Blanking comments in place gives
//!    that for free: every offset in the stripped text is the same offset in the
//!    file, so a span found in one is a span in the other.
//!
//! WHAT IT IS NOT. It is not a JSON parser. `strip` hands its output to
//! `serde_json`, which owns every question about numbers, escapes, duplicate
//! keys and depth. This module answers exactly two: where does a comment end,
//! and is this comma trailing.
//!
//! BYTE-WISE IS SAFE HERE. Every structural character JSON uses is ASCII, and
//! every byte of a multi-byte UTF-8 sequence is >= 0x80, so no continuation byte
//! can be mistaken for a quote, a slash or a backslash.

/// Replace comments and trailing commas with spaces, preserving every byte
/// offset and every newline.
///
/// Newlines are preserved inside block comments so a `serde_json` error still
/// reports the line the creator sees in their editor.
pub fn strip(text: &str) -> String {
    let src = text.as_bytes();
    let mut out = src.to_vec();
    let mut i = 0usize;
    let mut in_string = false;

    while i < src.len() {
        let b = src[i];
        if in_string {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match b {
            b'"' => {
                in_string = true;
                i += 1;
            }
            b'/' if i + 1 < src.len() && src[i + 1] == b'/' => {
                while i < src.len() && src[i] != b'\n' {
                    out[i] = b' ';
                    i += 1;
                }
            }
            b'/' if i + 1 < src.len() && src[i + 1] == b'*' => {
                out[i] = b' ';
                out[i + 1] = b' ';
                i += 2;
                while i < src.len() {
                    if src[i] == b'*' && i + 1 < src.len() && src[i + 1] == b'/' {
                        out[i] = b' ';
                        out[i + 1] = b' ';
                        i += 2;
                        break;
                    }
                    if src[i] != b'\n' {
                        out[i] = b' ';
                    }
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }

    blank_trailing_commas(&mut out);
    // Every byte we rewrote is an ASCII space over an ASCII byte or over a
    // comment body, and a comment body is never resumed mid-code-point because
    // the scan only enters one at an ASCII `/`.
    String::from_utf8(out).expect("strip only writes ASCII spaces over whole bytes")
}

fn blank_trailing_commas(buf: &mut [u8]) {
    let mut in_string = false;
    let mut i = 0usize;
    while i < buf.len() {
        let b = buf[i];
        if in_string {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if b == b'"' {
            in_string = true;
            i += 1;
            continue;
        }
        if b == b',' {
            let mut j = i + 1;
            while j < buf.len() && (buf[j] as char).is_ascii_whitespace() {
                j += 1;
            }
            if j < buf.len() && (buf[j] == b'}' || buf[j] == b']') {
                buf[i] = b' ';
            }
        }
        i += 1;
    }
}

/// The byte span of a top-level member's VALUE, if the member exists.
///
/// `text` must be the stripped form, so the returned span is also valid in the
/// original file. Returns `None` when the key is absent, when the document is
/// not an object, or when the value is not a scalar or bracketed value that
/// this scanner can bound.
pub fn top_level_value_span(text: &str, key: &str) -> Option<(usize, usize)> {
    let src = text.as_bytes();
    let mut i = skip_ws(src, 0);
    if i >= src.len() || src[i] != b'{' {
        return None;
    }
    i += 1;

    loop {
        i = skip_ws(src, i);
        if i >= src.len() || src[i] == b'}' {
            return None;
        }
        if src[i] != b'"' {
            return None;
        }
        let (name, after_key) = read_string(src, i)?;
        i = skip_ws(src, after_key);
        if i >= src.len() || src[i] != b':' {
            return None;
        }
        i = skip_ws(src, i + 1);
        let start = i;
        let end = value_end(src, i)?;
        if name == key {
            return Some((start, end));
        }
        i = skip_ws(src, end);
        if i < src.len() && src[i] == b',' {
            i += 1;
            continue;
        }
        return None;
    }
}

fn skip_ws(src: &[u8], mut i: usize) -> usize {
    while i < src.len() && (src[i] as char).is_ascii_whitespace() {
        i += 1;
    }
    i
}

fn read_string(src: &[u8], start: usize) -> Option<(String, usize)> {
    debug_assert_eq!(src[start], b'"');
    let mut i = start + 1;
    let mut raw = Vec::new();
    while i < src.len() {
        match src[i] {
            b'\\' => {
                raw.push(src[i]);
                raw.push(*src.get(i + 1)?);
                i += 2;
            }
            b'"' => {
                let s = serde_json::from_slice::<String>(
                    format!("\"{}\"", String::from_utf8_lossy(&raw)).as_bytes(),
                )
                .ok()?;
                return Some((s, i + 1));
            }
            b => {
                raw.push(b);
                i += 1;
            }
        }
    }
    None
}

/// The offset one past the end of the value starting at `start`.
fn value_end(src: &[u8], start: usize) -> Option<usize> {
    let mut i = start;
    match *src.get(i)? {
        b'"' => Some(read_string(src, i)?.1),
        b'{' | b'[' => {
            let mut depth = 0isize;
            let mut in_string = false;
            while i < src.len() {
                let b = src[i];
                if in_string {
                    if b == b'\\' {
                        i += 2;
                        continue;
                    }
                    if b == b'"' {
                        in_string = false;
                    }
                    i += 1;
                    continue;
                }
                match b {
                    b'"' => in_string = true,
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(i + 1);
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            None
        }
        _ => {
            while i < src.len() {
                let b = src[i];
                if (b as char).is_ascii_whitespace() || b == b',' || b == b'}' || b == b']' {
                    break;
                }
                i += 1;
            }
            if i == start {
                None
            } else {
                Some(i)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Comments vanish, and every byte offset survives them. The offset claim is
    /// what the splice writeback rests on, so it is asserted directly rather
    /// than inferred from the parse succeeding.
    #[test]
    fn comments_become_spaces_of_the_same_length() {
        let src = "{\n  // hello\n  \"a\": 1 /* x */\n}\n";
        let out = strip(src);
        assert_eq!(out.len(), src.len());
        assert_eq!(out.as_bytes().iter().filter(|b| **b == b'\n').count(), 4);
        let v: serde_json::Value = serde_json::from_str(&out).expect("strips to valid JSON");
        assert_eq!(v["a"], 1);
        assert_eq!(&out[src.find("\"a\"").unwrap()..][..3], "\"a\"");
    }

    /// A `//` inside a string is DATA, not a comment. This is the case a
    /// naive line-based stripper gets wrong, and every `control` value in a
    /// real file is a URL containing exactly that sequence.
    #[test]
    fn a_url_in_a_string_is_not_a_comment() {
        let out = strip("{\"control\": \"https://control.zeroship.ai\"}");
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid");
        assert_eq!(v["control"], "https://control.zeroship.ai");
    }

    /// An escaped quote must not end the string scan; the `/*` after it would
    /// otherwise start a comment that eats the rest of the document.
    #[test]
    fn escaped_quote_does_not_end_the_string() {
        let out = strip(r#"{"a": "x\"/*y", "b": 2}"#);
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid");
        assert_eq!(v["a"], "x\"/*y");
        assert_eq!(v["b"], 2);
    }

    #[test]
    fn trailing_commas_are_blanked_in_objects_and_arrays() {
        let out = strip("{\"a\": [1, 2, ], \"b\": {\"c\": 1, },}");
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid");
        assert_eq!(v["a"], serde_json::json!([1, 2]));
        assert_eq!(v["b"]["c"], 1);
    }

    /// A comma inside a string is not a trailing comma even when a `}` follows.
    #[test]
    fn a_comma_in_a_string_is_left_alone() {
        let out = strip(r#"{"a": "x,}"}"#);
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid");
        assert_eq!(v["a"], "x,}");
    }

    /// Multi-byte content survives byte-wise scanning unchanged.
    #[test]
    fn non_ascii_string_content_is_preserved() {
        let src = "{\"name\": \"caf\u{e9}-\u{1f600}\" // note\n}";
        let out = strip(src);
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid");
        assert_eq!(v["name"], "caf\u{e9}-\u{1f600}");
    }

    #[test]
    fn top_level_span_finds_the_value_and_only_the_value() {
        let src = "{\n  \"name\": \"x\",\n  \"app\": \"app_1\",\n  \"control\": \"u\"\n}";
        let stripped = strip(src);
        let (s, e) = top_level_value_span(&stripped, "app").expect("app span");
        assert_eq!(&src[s..e], "\"app_1\"");
    }

    /// A nested `"app"` must not be mistaken for the top-level one - that is
    /// the splice writing an environment's id into the root, which is exactly
    /// the cross-targeting the environments rule exists to prevent.
    #[test]
    fn a_nested_app_key_is_not_the_top_level_one() {
        let src = "{\n  \"environments\": { \"staging\": { \"app\": \"app_stg\" } },\n  \"app\": \"app_root\"\n}";
        let stripped = strip(src);
        let (s, e) = top_level_value_span(&stripped, "app").expect("app span");
        assert_eq!(&src[s..e], "\"app_root\"");
    }

    #[test]
    fn an_absent_key_has_no_span() {
        let stripped = strip("{\"name\": \"x\"}");
        assert_eq!(top_level_value_span(&stripped, "app"), None);
    }
}
