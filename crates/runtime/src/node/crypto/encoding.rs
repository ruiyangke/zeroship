//! Node `Buffer` / encoding registry.
//!
//! See `docs/proposals/node-crypto-native.md` §I.6. The 7 named
//! encodings Node accepts on hash/hmac digest output and string input:
//!
//! - `utf8` / `utf-8`           UTF-8 (default)
//! - `utf16le` / `utf-16le` / `ucs2` / `ucs-2`  UTF-16 LE
//! - `latin1` / `binary`        ISO-8859-1 (Node treats them as aliases)
//! - `ascii`                    7-bit ASCII (high bit stripped per Node)
//! - `hex`                      lowercase hex
//! - `base64`                   standard base64 with padding
//! - `base64url`                URL-safe base64 without padding
//!
//! Used by `Hash.digest(encoding)` / `Hmac.digest(encoding)` / etc.

use crate::state::OpError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf16Le,
    Latin1,
    Ascii,
    Hex,
    Base64,
    Base64Url,
}

/// Map a Node-style encoding name to the enum. Case-insensitive.
/// Returns `None` for unknown encoding (caller emits
/// `ERR_UNKNOWN_ENCODING`).
pub fn from_str(name: &str) -> Option<Encoding> {
    match name.to_ascii_lowercase().as_str() {
        "utf8" | "utf-8" => Some(Encoding::Utf8),
        "utf16le" | "utf-16le" | "ucs2" | "ucs-2" => Some(Encoding::Utf16Le),
        "latin1" | "binary" => Some(Encoding::Latin1),
        "ascii" => Some(Encoding::Ascii),
        "hex" => Some(Encoding::Hex),
        "base64" => Some(Encoding::Base64),
        "base64url" => Some(Encoding::Base64Url),
        _ => None,
    }
}

/// Decode a JS string to bytes per the named encoding.
pub fn decode(input: &str, encoding: Encoding) -> Result<Vec<u8>, OpError> {
    match encoding {
        Encoding::Utf8 => Ok(input.as_bytes().to_vec()),
        Encoding::Utf16Le => {
            // Each UTF-16 code unit (u16) emits two bytes (little-endian).
            // input is a Rust String (UTF-8); collect its UTF-16 view.
            let mut out = Vec::with_capacity(input.len() * 2);
            for u16_unit in input.encode_utf16() {
                out.push((u16_unit & 0xff) as u8);
                out.push((u16_unit >> 8) as u8);
            }
            Ok(out)
        }
        Encoding::Latin1 => {
            // Node's latin1 = take the low byte of each UTF-16 code unit.
            let mut out = Vec::with_capacity(input.len());
            for u16_unit in input.encode_utf16() {
                out.push((u16_unit & 0xff) as u8);
            }
            Ok(out)
        }
        Encoding::Ascii => {
            // Node's ascii = take the low byte AND mask 0x7f.
            let mut out = Vec::with_capacity(input.len());
            for u16_unit in input.encode_utf16() {
                out.push((u16_unit & 0x7f) as u8);
            }
            Ok(out)
        }
        Encoding::Hex => decode_hex(input),
        Encoding::Base64 | Encoding::Base64Url => decode_base64(input, encoding == Encoding::Base64Url),
    }
}

/// Encode bytes to a string per the named encoding. The Buffer-default
/// (no encoding requested) path is handled by the caller — this only
/// covers requested-encoding string emission.
pub fn encode(bytes: &[u8], encoding: Encoding) -> String {
    match encoding {
        Encoding::Utf8 => {
            // Node's `digest('utf8')` is well-defined as LOSSY for binary
            // digest output — invalid UTF-8 byte sequences are replaced
            // with U+FFFD per
            // https://nodejs.org/api/buffer.html#buffers-and-character-encodings
            String::from_utf8_lossy(bytes).into_owned()
        }
        Encoding::Utf16Le => {
            // Each pair of bytes → one UTF-16 code unit (little-endian).
            // Pad with one zero byte if odd length (Node truncates instead;
            // we keep it lossy — same outcome for digest output, never odd).
            let mut units = Vec::with_capacity(bytes.len() / 2);
            let mut i = 0;
            while i + 1 < bytes.len() {
                let u = bytes[i] as u16 | ((bytes[i + 1] as u16) << 8);
                units.push(u);
                i += 2;
            }
            String::from_utf16_lossy(&units)
        }
        Encoding::Latin1 => {
            // Lossless 1:1 mapping byte → code-point u+0000..u+00ff.
            let mut s = String::with_capacity(bytes.len());
            for &b in bytes {
                s.push(b as char);
            }
            s
        }
        Encoding::Ascii => {
            // Like latin1 but with high bit stripped per Node.
            let mut s = String::with_capacity(bytes.len());
            for &b in bytes {
                s.push((b & 0x7f) as char);
            }
            s
        }
        Encoding::Hex => encode_hex(bytes),
        Encoding::Base64 => encode_base64(bytes, false),
        Encoding::Base64Url => encode_base64(bytes, true),
    }
}

// ---------------------------------------------------------------------------
// Hex
// ---------------------------------------------------------------------------

const HEX: &[u8; 16] = b"0123456789abcdef";

pub fn encode_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

fn decode_hex(s: &str) -> Result<Vec<u8>, OpError> {
    // Node accepts an odd length by truncating after the last full byte
    // and accepts whitespace/newline by ignoring (per Node's Buffer.from
    // hex behaviour — verified against
    // https://github.com/nodejs/node/blob/main/lib/buffer.js). We follow.
    let mut bytes = Vec::with_capacity(s.len() / 2);
    let mut nibble: Option<u8> = None;
    for c in s.chars() {
        let hi = match c {
            '0'..='9' => (c as u8) - b'0',
            'a'..='f' => (c as u8) - b'a' + 10,
            'A'..='F' => (c as u8) - b'A' + 10,
            _ => continue, // Skip whitespace + other.
        };
        if let Some(prev) = nibble {
            bytes.push((prev << 4) | hi);
            nibble = None;
        } else {
            nibble = Some(hi);
        }
    }
    Ok(bytes)
}

// ---------------------------------------------------------------------------
// Base64 — uses the existing `base64` crate dep.
// ---------------------------------------------------------------------------

fn encode_base64(bytes: &[u8], url_safe: bool) -> String {
    use base64::Engine;
    if url_safe {
        // base64url unpadded per RFC 4648 §5.
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    } else {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }
}

fn decode_base64(s: &str, url_safe: bool) -> Result<Vec<u8>, OpError> {
    use base64::Engine;
    // Strip whitespace + newlines (Node tolerates them).
    let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    let result = if url_safe {
        // Try URL-safe first; fall back to lenient if user passed standard.
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&cleaned)
            .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(&cleaned))
            .or_else(|_| base64::engine::general_purpose::STANDARD.decode(&cleaned))
    } else {
        // Try standard first; fall back to URL-safe (Node is lenient).
        base64::engine::general_purpose::STANDARD
            .decode(&cleaned)
            .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(&cleaned))
            .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&cleaned))
    };
    result.map_err(|_| {
        OpError::node(
            "ERR_INVALID_ARG_VALUE",
            "Invalid base64 string",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let b = b"hello";
        let h = encode_hex(b);
        assert_eq!(h, "68656c6c6f");
        let back = decode_hex(&h).unwrap();
        assert_eq!(back, b);
    }

    #[test]
    fn base64_round_trip() {
        let b = b"hello world";
        let s = encode_base64(b, false);
        assert_eq!(s, "aGVsbG8gd29ybGQ=");
        let back = decode_base64(&s, false).unwrap();
        assert_eq!(back, b);
    }

    #[test]
    fn base64url_unpadded() {
        let b = b"hello world";
        let s = encode_base64(b, true);
        assert_eq!(s, "aGVsbG8gd29ybGQ"); // No trailing =
        let back = decode_base64(&s, true).unwrap();
        assert_eq!(back, b);
    }

    #[test]
    fn encoding_aliases() {
        assert_eq!(from_str("UTF8"), Some(Encoding::Utf8));
        assert_eq!(from_str("utf-8"), Some(Encoding::Utf8));
        assert_eq!(from_str("Hex"), Some(Encoding::Hex));
        assert_eq!(from_str("BINARY"), Some(Encoding::Latin1));
        assert_eq!(from_str("latin1"), Some(Encoding::Latin1));
        assert_eq!(from_str("ucs2"), Some(Encoding::Utf16Le));
        assert_eq!(from_str("nope"), None);
    }
}
