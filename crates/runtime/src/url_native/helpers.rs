//! Shared helpers for URL + URLSearchParams.
//!
//! Holds the application/x-www-form-urlencoded serializer/parser per
//! WHATWG URL §5 (https://url.spec.whatwg.org/#urlencoded-parsing,
//! https://url.spec.whatwg.org/#urlencoded-serializing) and the lone
//! surrogate scrubber required by USVString conversion (§3.2.21).

use crate::state::OpError;

/// A WebIDL `USVString` — Unicode scalar value sequence.
///
/// Per https://webidl.spec.whatwg.org/#es-USVString:
///   1. Let s be ? ToString(V).
///   2. Return the result of replacing any unmatched surrogate code
///      unit in s with U+FFFD.
///
/// Internally a UTF-8 `String` (lone surrogates are replaced before
/// the type even materialises). The `#[v8_class]` macro recognises
/// `USVString` in argument position and converts via
/// [`read_usv_string_or_throw`] before any user method body runs —
/// which means user code receives owned UTF-8 with no remaining V8
/// `Local<Value>` borrows alive.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct USVString(pub String);

impl USVString {
    pub fn new() -> Self {
        USVString(String::new())
    }

    pub fn from_string(s: String) -> Self {
        USVString(s)
    }

    pub fn into_string(self) -> String {
        self.0
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for USVString {
    fn from(s: String) -> Self {
        USVString(s)
    }
}

impl From<USVString> for String {
    fn from(u: USVString) -> Self {
        u.0
    }
}

impl AsRef<str> for USVString {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::ops::Deref for USVString {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

/// Convert a `v8::Value` to a USVString — the WHATWG WebIDL conversion
/// that replaces unmatched surrogate code units with U+FFFD. Returns
/// `None` only on `Value::to_string` failure (e.g. a Symbol triggers
/// a pending TypeError in V8 which the caller surfaces).
pub fn read_usv_string(
    scope: &mut v8::PinScope,
    value: v8::Local<v8::Value>,
) -> Option<String> {
    let s = value.to_string(scope)?;
    let len = s.length();
    let mut buf = vec![0u16; len];
    s.write_v2(scope, 0, &mut buf, v8::WriteFlags::empty());

    let mut out = String::with_capacity(len);
    let mut i = 0usize;
    while i < buf.len() {
        let cu = buf[i];
        if (0xD800..=0xDBFF).contains(&cu) {
            // High surrogate: needs a low surrogate to follow.
            if i + 1 < buf.len() {
                let next = buf[i + 1];
                if (0xDC00..=0xDFFF).contains(&next) {
                    let hi = (cu as u32) - 0xD800;
                    let lo = (next as u32) - 0xDC00;
                    let cp = 0x10000 + ((hi << 10) | lo);
                    if let Some(c) = char::from_u32(cp) {
                        out.push(c);
                    } else {
                        out.push('\u{FFFD}');
                    }
                    i += 2;
                    continue;
                }
            }
            // Lone high surrogate.
            out.push('\u{FFFD}');
            i += 1;
            continue;
        }
        if (0xDC00..=0xDFFF).contains(&cu) {
            // Lone low surrogate.
            out.push('\u{FFFD}');
            i += 1;
            continue;
        }
        out.push(char::from_u32(cu as u32).unwrap_or('\u{FFFD}'));
        i += 1;
    }
    Some(out)
}

/// Convert a `v8::Value` to a USVString or throw a TypeError. Used by
/// the `#[v8_class]` macro's `USVString` arg extraction.
pub fn read_usv_string_or_throw(
    scope: &mut v8::PinScope,
    value: v8::Local<v8::Value>,
) -> Result<String, OpError> {
    read_usv_string(scope, value)
        .ok_or_else(|| OpError::type_error("Cannot convert value to USVString"))
}

/// Serialize a list of (name, value) pairs as `application/x-www-form-
/// urlencoded` per https://url.spec.whatwg.org/#urlencoded-serializing
///
///   1. Let output be the empty string.
///   2. For each tuple of pairs:
///        Let outputPair be the percent-encoded name.
///        Append "=" plus the percent-encoded value to outputPair.
///        If output is non-empty, append "&" to it. Append outputPair.
///   3. Return output.
///
/// The byte serializer percent-encodes via the `application/x-www-form-
/// urlencoded` percent-encode set, which is everything except 0x2A
/// ("*"), 0x2D ("-"), 0x2E ("."), 0x30-0x39 (digits), 0x41-0x5A
/// (uppercase), 0x5F ("_"), 0x61-0x7A (lowercase). Plus space (0x20) is
/// special: it's emitted as "+" rather than "%20".
pub fn url_encoded_serialize(pairs: &[(String, String)]) -> String {
    let mut out = String::new();
    for (i, (name, value)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        url_encoded_serialize_byte(name.as_bytes(), &mut out);
        out.push('=');
        url_encoded_serialize_byte(value.as_bytes(), &mut out);
    }
    out
}

fn url_encoded_serialize_byte(bytes: &[u8], out: &mut String) {
    // M10: avoid per-byte `format!("{:02X}", b)` allocation. Hex
    // lookup against a static table writes 2 ASCII chars per
    // percent-encoded byte with zero allocation.
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for &b in bytes {
        if b == 0x20 {
            out.push('+');
        } else if b == b'*' || b == b'-' || b == b'.' || b == b'_'
            || b.is_ascii_alphanumeric()
        {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0F) as usize] as char);
        }
    }
}

/// Parse an `application/x-www-form-urlencoded` string per
/// https://url.spec.whatwg.org/#urlencoded-parsing
///
///   1. Let sequences be the result of splitting input on "&".
///   2. Let output be an empty list of name-value tuples.
///   3. For each byte sequence bytes in sequences:
///      a. If bytes is empty, continue.
///      b. If bytes contains "=", let name be the part before the first
///         "=", let value be the part after. Otherwise let name be bytes
///         and value be the empty string.
///      c. Replace any 0x2B ("+") with 0x20 (SP) in name and value.
///      d. Let nameString be UTF-8 percent-decode of name.
///         Let valueString be UTF-8 percent-decode of value.
///      e. Append (nameString, valueString) to output.
///
/// Bytes that don't form valid UTF-8 after percent-decode survive via
/// the WHATWG-mandated UTF-8 replacement-character semantics
/// (`String::from_utf8_lossy` matches that).
pub fn url_encoded_parse(input: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if input.is_empty() {
        return out;
    }
    for chunk in input.split('&') {
        if chunk.is_empty() {
            continue;
        }
        let (name, value) = match chunk.find('=') {
            Some(i) => (&chunk[..i], &chunk[i + 1..]),
            None => (chunk, ""),
        };

        // Step c+d: replace '+' with ' ', then percent-decode.
        let name_replaced: Vec<u8> = name
            .bytes()
            .map(|b| if b == b'+' { 0x20 } else { b })
            .collect();
        let value_replaced: Vec<u8> = value
            .bytes()
            .map(|b| if b == b'+' { 0x20 } else { b })
            .collect();

        let name_decoded = percent_decode(&name_replaced);
        let value_decoded = percent_decode(&value_replaced);

        // M11: avoid forced allocation when the bytes are valid UTF-8.
        // `String::from_utf8_lossy` returns a `Cow::Borrowed` for ASCII-
        // clean / UTF-8-clean input, but `.into_owned()` then copies
        // the borrowed slice. `String::from_utf8` reuses the Vec
        // directly when valid; on invalid UTF-8 we fall back to the
        // lossy path.
        out.push((
            from_utf8_or_lossy(name_decoded),
            from_utf8_or_lossy(value_decoded),
        ));
    }
    out
}

/// Take ownership of a byte vector as a String. If the bytes are valid
/// UTF-8 the Vec is consumed in-place (no allocation). Otherwise
/// `from_utf8_lossy` produces a Cow<&str> with U+FFFD substitution and
/// we materialise it to an owned String.
fn from_utf8_or_lossy(bytes: Vec<u8>) -> String {
    match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(&e.into_bytes()).into_owned(),
    }
}

/// UTF-8 percent-decode per https://url.spec.whatwg.org/#percent-decode
/// — interpret each `%XX` triple as a single byte; on a malformed
/// triple (non-hex), keep the literal `%`.
pub fn percent_decode(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        let b = input[i];
        if b == b'%' && i + 2 < input.len() {
            let h = input[i + 1];
            let l = input[i + 2];
            if let (Some(hi), Some(lo)) = (hex_val(h), hex_val(l)) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(b);
        i += 1;
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(10 + b - b'a'),
        b'A'..=b'F' => Some(10 + b - b'A'),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_empty() {
        assert_eq!(url_encoded_serialize(&[]), "");
    }

    #[test]
    fn serialize_simple() {
        let pairs = vec![("a".into(), "1".into()), ("b".into(), "2".into())];
        assert_eq!(url_encoded_serialize(&pairs), "a=1&b=2");
    }

    #[test]
    fn serialize_space_as_plus() {
        let pairs = vec![("a".into(), "b c".into())];
        assert_eq!(url_encoded_serialize(&pairs), "a=b+c");
    }

    #[test]
    fn serialize_percent_encoding() {
        let pairs = vec![("a".into(), "&".into())];
        assert_eq!(url_encoded_serialize(&pairs), "a=%26");
    }

    #[test]
    fn serialize_unreserved_chars() {
        let pairs = vec![("a".into(), "*-._".into())];
        assert_eq!(url_encoded_serialize(&pairs), "a=*-._");
    }

    #[test]
    fn parse_simple() {
        let r = url_encoded_parse("a=1&b=2");
        assert_eq!(r, vec![("a".into(), "1".into()), ("b".into(), "2".into())]);
    }

    #[test]
    fn parse_no_value() {
        let r = url_encoded_parse("a&b=2");
        assert_eq!(r, vec![("a".into(), "".into()), ("b".into(), "2".into())]);
    }

    #[test]
    fn parse_plus_to_space() {
        let r = url_encoded_parse("a=b+c");
        assert_eq!(r, vec![("a".into(), "b c".into())]);
    }

    #[test]
    fn parse_percent_decode() {
        let r = url_encoded_parse("a=%26");
        assert_eq!(r, vec![("a".into(), "&".into())]);
    }

    #[test]
    fn parse_empty_chunk_skipped() {
        let r = url_encoded_parse("a=1&&b=2");
        assert_eq!(r, vec![("a".into(), "1".into()), ("b".into(), "2".into())]);
    }

    #[test]
    fn parse_lone_equals_survives() {
        let r = url_encoded_parse("a=&a=b");
        assert_eq!(r, vec![("a".into(), "".into()), ("a".into(), "b".into())]);
    }
}
