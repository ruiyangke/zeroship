//! Vendored typed-id subset (base62/UUIDv7 id machinery for `MigrationId`).
//!
//! Copied byte-identically from `zeroship_core::typed_id` so this crate can be
//! embedded as a lean library without a runtime dependency on `zeroship-core`.
//! The base62/uuid encoding is a wire contract; a `tests/core_id_parity.rs`
//! drift guard asserts these copies stay identical to core while both crates
//! coexist in-tree.

/// Base62 alphabet — sorted so lexicographic order matches numeric order
/// for the high bits (timestamp), preserving UUIDv7 sort order.
const BASE62: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// Reverse lookup table: ASCII byte → base62 digit (255 = invalid)
const fn build_decode_table() -> [u8; 128] {
    let mut table = [255u8; 128];
    let mut i = 0;
    while i < 62 {
        table[BASE62[i] as usize] = i as u8;
        i += 1;
    }
    table
}

const DECODE: [u8; 128] = build_decode_table();

/// Encode 128-bit UUID bytes to 22-char base62 string.
pub fn uuid_to_base62(uuid: &uuid::Uuid) -> String {
    let bytes = uuid.as_bytes();
    // Treat as a 128-bit big-endian integer and repeatedly divide by 62
    let mut n = u128::from_be_bytes(*bytes);
    let mut buf = [0u8; 22];
    for i in (0..22).rev() {
        buf[i] = BASE62[(n % 62) as usize];
        n /= 62;
    }
    String::from_utf8(buf.to_vec()).expect("base62 chars are valid UTF-8")
}

/// Encode an arbitrary byte slice as a base62 string by treating it as a
/// big-endian integer and repeatedly dividing by 62.
///
/// Unlike [`uuid_to_base62`] (fixed 22-char width for a 128-bit UUID), this
/// handles inputs of any length, so it can encode an HMAC tag. The output
/// length is not fixed; callers that want a bounded id should truncate the
/// returned string (e.g. the pairwise-subject derivation takes the first 20
/// chars). Empty input yields an empty string.
#[must_use]
pub fn base62_encode_bytes(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    // Big-endian byte-array long division by 62, collecting remainders.
    let mut digits = bytes.to_vec();
    let mut out = Vec::new();
    // Strip leading zero bytes only after the loop preserves value; we loop
    // until the running number is zero.
    loop {
        let mut rem: u16 = 0;
        let mut all_zero = true;
        for d in &mut digits {
            let cur = (rem << 8) | u16::from(*d);
            let q = cur / 62;
            rem = cur % 62;
            *d = u8::try_from(q).unwrap_or(0);
            if *d != 0 {
                all_zero = false;
            }
        }
        out.push(BASE62[rem as usize]);
        if all_zero {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).expect("base62 chars are valid UTF-8")
}

/// Decode 22-char base62 string to UUID bytes.
pub fn base62_to_uuid(s: &str) -> Result<uuid::Uuid, String> {
    if s.len() != 22 {
        return Err(format!("expected 22 base62 chars, got {}", s.len()));
    }
    let mut n: u128 = 0;
    for &b in s.as_bytes() {
        if b >= 128 {
            return Err(format!("invalid base62 character: {}", b as char));
        }
        let digit = DECODE[b as usize];
        if digit == 255 {
            return Err(format!("invalid base62 character: {}", b as char));
        }
        n = n.checked_mul(62)
            .and_then(|n| n.checked_add(digit as u128))
            .ok_or_else(|| "base62 overflow".to_string())?;
    }
    Ok(uuid::Uuid::from_bytes(n.to_be_bytes()))
}

/// Generate a new UUIDv7 (timestamp-ordered).
pub fn new_v7() -> uuid::Uuid {
    uuid::Uuid::now_v7()
}

/// Generate a typed ID: `{prefix}_{base62(uuidv7)}`
pub fn generate(prefix: &str) -> String {
    let uuid = new_v7();
    format!("{}_{}", prefix, uuid_to_base62(&uuid))
}

/// Parse a typed ID: extract the prefix and decode to UUID.
pub fn parse(typed_id: &str) -> Result<(&str, uuid::Uuid), String> {
    let (prefix, encoded) = typed_id
        .split_once('_')
        .ok_or_else(|| format!("invalid typed ID (no prefix): {typed_id}"))?;
    let uuid = base62_to_uuid(encoded)?;
    Ok((prefix, uuid))
}

/// Parse error for [`parse_with_prefix`]. Distinguishes a wrong-prefix
/// boundary check from a malformed-id parse error so callers (e.g.
/// `crates/sandbox/src/db.rs`) can map them onto distinct error
/// variants without losing the underlying detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// The id parsed cleanly but its prefix did not match the expected
    /// entity-type prefix. Used by `parse_with_prefix` as the
    /// path-traversal-hardening boundary check (Invariant 2 in
    /// `docs/proposals/sandbox-pg-state.md`).
    WrongPrefix { expected: String, got: String },
    /// The id failed to parse — wrong shape, invalid base62, missing
    /// underscore, etc. Carries the same string the underlying [`parse`]
    /// would have returned.
    Malformed(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongPrefix { expected, got } => {
                write!(f, "expected prefix '{expected}', got '{got}'")
            }
            Self::Malformed(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for ParseError {}

/// Parse a typed ID and assert its prefix matches `expected_prefix`.
///
/// Layered safety check on top of [`parse`]. Callers that have a
/// known entity type (e.g. `sandbox.db.insert_sandbox` knows it is
/// receiving an `sbx_…` id) use this helper to refuse mismatched
/// prefixes BEFORE the value reaches any downstream wire (SQL,
/// filesystem path, HTTP header). Mirrors the path-traversal
/// hardening posture in `crates/sandbox/src/persist.rs:36-40`.
///
/// Returns the embedded UUID on success.
pub fn parse_with_prefix(
    typed_id: &str,
    expected_prefix: &str,
) -> Result<uuid::Uuid, ParseError> {
    let (got, uuid) = parse(typed_id).map_err(ParseError::Malformed)?;
    if got != expected_prefix {
        return Err(ParseError::WrongPrefix {
            expected: expected_prefix.to_string(),
            got: got.to_string(),
        });
    }
    Ok(uuid)
}
