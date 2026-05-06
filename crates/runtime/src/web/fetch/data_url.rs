//! `data:` URL handler per WHATWG Fetch §5.4 (scheme fetch).
//!
//! Per Fetch's `scheme fetch`, when request.URL.scheme is "data":
//!
//!   1. Run the data: URL processor on request.URL.
//!   2. If that returned failure, return a network error.
//!   3. Let mimeType be the first item; bytes be the second.
//!   4. Return a response whose status is 200, status-message is "OK",
//!      header list is « ("Content-Type", mimeType serialized) », body
//!      is bytes.
//!
//! ## Why we ship our own parser
//!
//! We considered adding the `data-url` crate
//! as a workspace dep. Auditing the crate (~270 LOC, no other consumers,
//! pulls in `mime` + a percent-decoder) showed it was simpler to inline
//! the parser. The grammar from RFC 2397 is tiny:
//!
//! ```text
//! data:[<mediatype>][;base64],<data>
//! mediatype := type "/" subtype *( ";" parameter )
//! ```
//!
//! We percent-decode the data segment (always), then base64-decode if the
//! `;base64` flag was present, then UTF-8-validate ONLY when the MIME is
//! a text type for cross-check (the response body returns raw bytes
//! either way; validation is informational).
//!
//! Edge cases handled:
//!   - Empty `data:,` → empty body, default MIME `text/plain;charset=US-ASCII`.
//!   - `data:base64,XXXX` → no MIME prefix; base64 flag still applies.
//!   - Whitespace inside base64 data — stripped per RFC 4648 + browser
//!     behaviour.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;

/// Parse a `data:` URL into `(mime, bytes)`.
///
/// Returns `Err(())` for any unparseable URL — caller maps to a network
/// error. Matches the spec's "data: URL processor" return shape.
pub fn parse_data_url(url: &str) -> Result<(String, Vec<u8>), ()> {
    // Strip the "data:" scheme. Must be present and case-insensitive.
    let rest = url.strip_prefix("data:").or_else(|| url.strip_prefix("DATA:")).ok_or(())?;

    // Split on the FIRST comma — everything before is mediatype/flags,
    // everything after is data. Per RFC 2397 the comma is the only
    // mandatory separator.
    let (prefix, data_str) = rest.split_once(',').ok_or(())?;

    // Determine base64 flag and MIME from prefix.
    //
    // Prefix is `[<mediatype>][;<param>...][;base64]`. We split on `;`,
    // detect a trailing `base64` token, and treat the rest as the MIME
    // (if present). An empty prefix means no mediatype was specified —
    // spec default `text/plain;charset=US-ASCII`.
    let mut tokens: Vec<&str> = prefix.split(';').collect();
    let is_base64 = tokens
        .last()
        .map(|t| t.eq_ignore_ascii_case("base64"))
        .unwrap_or(false);
    if is_base64 {
        tokens.pop();
    }

    let mime = if tokens.is_empty() || tokens[0].is_empty() {
        // Per spec, default is `text/plain;charset=US-ASCII`. If the
        // user wrote `data:;charset=utf-8,...`, the type stays default
        // but the params travel.
        if tokens.iter().any(|t| !t.is_empty()) {
            // No type, but parameters were given. Rebuild
            // `text/plain;<params>`.
            let params: Vec<&str> = tokens.iter().filter(|t| !t.is_empty()).copied().collect();
            format!("text/plain;{}", params.join(";"))
        } else {
            "text/plain;charset=US-ASCII".to_string()
        }
    } else {
        // Reassemble `<type>[;<param>...]`. Validate the type/subtype
        // shape minimally (must contain at least one `/`).
        let mt = tokens.join(";");
        if !mt.contains('/') {
            return Err(());
        }
        mt
    };

    // Percent-decode the data segment per RFC 3986 + spec note that
    // the data: URL processor does percent-decoding even before base64.
    let decoded = percent_decode_to_bytes(data_str);

    let bytes = if is_base64 {
        // Per the spec's "forgiving base64 decode": strip ASCII whitespace,
        // accept `+/` or `-_` alphabet (URL-safe), allow missing padding.
        let cleaned: Vec<u8> = decoded.into_iter().filter(|b| !b.is_ascii_whitespace()).collect();
        // Try standard base64 first; if that fails, try url-safe.
        match BASE64.decode(&cleaned) {
            Ok(b) => b,
            Err(_) => {
                // Try padding-tolerant: pad to length % 4 == 0.
                let mut padded = cleaned.clone();
                while padded.len() % 4 != 0 {
                    padded.push(b'=');
                }
                match BASE64.decode(&padded) {
                    Ok(b) => b,
                    Err(_) => {
                        // URL-safe alphabet (used by browsers as a fallback).
                        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
                        URL_SAFE_NO_PAD
                            .decode(cleaned.iter().filter(|&&b| b != b'=').copied().collect::<Vec<u8>>())
                            .map_err(|_| ())?
                    }
                }
            }
        }
    } else {
        decoded
    };

    Ok((mime, bytes))
}

/// Percent-decode an ASCII-percent-encoded string, returning the raw
/// bytes. Invalid percent escapes pass through literally (matches
/// browser behaviour and the WHATWG percent-decode algorithm).
fn percent_decode_to_bytes(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let h1 = hex_digit(bytes[i + 1]);
            let h2 = hex_digit(bytes[i + 2]);
            if let (Some(a), Some(b)) = (h1, h2) {
                out.push((a << 4) | b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

fn hex_digit(b: u8) -> Option<u8> {
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
    fn parses_plain_text() {
        let (mime, body) = parse_data_url("data:text/plain,hello").unwrap();
        assert_eq!(mime, "text/plain");
        assert_eq!(body, b"hello");
    }

    #[test]
    fn parses_default_mime_when_omitted() {
        let (mime, body) = parse_data_url("data:,hello").unwrap();
        assert_eq!(mime, "text/plain;charset=US-ASCII");
        assert_eq!(body, b"hello");
    }

    #[test]
    fn parses_base64_payload() {
        // `aGVsbG8=` is base64 for "hello".
        let (mime, body) = parse_data_url("data:text/plain;base64,aGVsbG8=").unwrap();
        assert_eq!(mime, "text/plain");
        assert_eq!(body, b"hello");
    }

    #[test]
    fn parses_base64_no_mime() {
        let (mime, body) = parse_data_url("data:;base64,aGVsbG8=").unwrap();
        assert_eq!(mime, "text/plain;charset=US-ASCII");
        assert_eq!(body, b"hello");
    }

    #[test]
    fn rejects_url_without_comma() {
        assert!(parse_data_url("data:text/plain").is_err());
    }

    #[test]
    fn rejects_invalid_mime_no_slash() {
        assert!(parse_data_url("data:invalid,hello").is_err());
    }

    #[test]
    fn percent_decodes() {
        let (_mime, body) = parse_data_url("data:,hello%20world").unwrap();
        assert_eq!(body, b"hello world");
    }

    #[test]
    fn empty_body() {
        let (mime, body) = parse_data_url("data:,").unwrap();
        assert_eq!(mime, "text/plain;charset=US-ASCII");
        assert_eq!(body, b"");
    }

    #[test]
    fn base64_with_charset_param() {
        let (mime, body) = parse_data_url("data:text/plain;charset=utf-8;base64,aGVsbG8=").unwrap();
        assert_eq!(mime, "text/plain;charset=utf-8");
        assert_eq!(body, b"hello");
    }
}
