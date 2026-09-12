//! Typed-id codec used by [`crate::migration::MigrationId`].
//!
//! Keep this leaf implementation byte-compatible with `zeroship-id`; the
//! migration-server parity test cross-encodes and decodes both implementations.

/// Base36 alphabet - sorted so lexicographic order matches numeric order
/// for the high bits (timestamp), preserving `UUIDv7` sort order.
const BASE36: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// Width of a base36-encoded UUID.
pub const BODY_LEN: usize = 25;

/// Reverse lookup table: ASCII byte -> base36 digit (255 = invalid)
const fn build_decode_table() -> [u8; 128] {
    let mut table = [255u8; 128];
    let mut i = 0;
    while i < 36 {
        table[BASE36[i] as usize] = i as u8;
        i += 1;
    }
    table
}

const DECODE: [u8; 128] = build_decode_table();

/// Encode UUID bytes as fixed-width base36.
#[must_use]
pub fn uuid_to_base36(uuid: &uuid::Uuid) -> String {
    let bytes = uuid.as_bytes();
    // Treat the UUID as one big-endian integer.
    let mut n = u128::from_be_bytes(*bytes);
    let mut buf = [0u8; BODY_LEN];
    for i in (0..BODY_LEN).rev() {
        buf[i] = BASE36[(n % 36) as usize];
        n /= 36;
    }
    String::from_utf8(buf.to_vec()).expect("base36 chars are valid UTF-8")
}

/// Encode an arbitrary byte slice as a base36 string by treating it as a
/// big-endian integer.
///
/// Unlike [`uuid_to_base36`], this
/// handles inputs of any length, so it can encode an HMAC tag. The output
/// length is not fixed; callers that want a bounded id should truncate the
/// returned string (e.g. the pairwise-subject derivation takes the first 20
/// chars). Empty input yields an empty string.
#[must_use]
pub fn base36_encode_bytes(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    // Big-endian byte-array long division, collecting remainders.
    let mut digits = bytes.to_vec();
    let mut out = Vec::new();
    // Strip leading zero bytes only after the loop preserves value; we loop
    // until the running number is zero.
    loop {
        let mut rem: u16 = 0;
        let mut all_zero = true;
        for d in &mut digits {
            let cur = (rem << 8) | u16::from(*d);
            let q = cur / 36;
            rem = cur % 36;
            *d = u8::try_from(q).unwrap_or(0);
            if *d != 0 {
                all_zero = false;
            }
        }
        out.push(BASE36[rem as usize]);
        if all_zero {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).expect("base36 chars are valid UTF-8")
}

/// Decode a fixed-width base36 string to UUID bytes.
pub fn base36_to_uuid(s: &str) -> Result<uuid::Uuid, String> {
    if s.len() != BODY_LEN {
        return Err(format!("expected {BODY_LEN} base36 chars, got {}", s.len()));
    }
    let mut n: u128 = 0;
    for &b in s.as_bytes() {
        if b >= 128 {
            return Err(format!("invalid base36 character: {}", b as char));
        }
        let digit = DECODE[b as usize];
        if digit == 255 {
            return Err(format!("invalid base36 character: {}", b as char));
        }
        n = n
            .checked_mul(36)
            .and_then(|n| n.checked_add(u128::from(digit)))
            .ok_or_else(|| "base36 overflow".to_string())?;
    }
    Ok(uuid::Uuid::from_bytes(n.to_be_bytes()))
}

/// Generate a new `UUIDv7` (timestamp-ordered).
#[must_use]
pub fn new_v7() -> uuid::Uuid {
    uuid::Uuid::now_v7()
}

/// Generate a typed ID: `{prefix}_{base36(uuidv7)}`
#[must_use]
pub fn generate(prefix: &str) -> String {
    let uuid = new_v7();
    format!("{}_{}", prefix, uuid_to_base36(&uuid))
}

/// Parse a typed ID: extract the prefix and decode to UUID.
pub fn parse(typed_id: &str) -> Result<(&str, uuid::Uuid), String> {
    let (prefix, encoded) = typed_id
        .split_once('_')
        .ok_or_else(|| format!("invalid typed ID (no prefix): {typed_id}"))?;
    let uuid = base36_to_uuid(encoded)?;
    Ok((prefix, uuid))
}

/// Parse error for [`parse_with_prefix`]. Distinguishes a wrong-prefix
/// boundary check from a malformed-id parse error so a caller can map them onto
/// distinct error variants without losing the underlying detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// The id parsed cleanly but its prefix did not match the expected
    /// entity-type prefix. Used by `parse_with_prefix` as the
    /// path-traversal-hardening boundary check: a caller that asked for one
    /// entity type must never receive an id minted for another.
    WrongPrefix { expected: String, got: String },
    /// The id failed to parse - wrong shape, invalid base36, missing
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
/// Layered safety check on top of [`parse`]. A caller that knows the entity
/// type it expects uses this helper to refuse a mismatched prefix BEFORE the
/// value reaches any downstream wire (SQL, filesystem path, HTTP header), so an
/// id minted for one entity cannot be walked into another's namespace. Checks
/// the prefix only: [`parse`] has already proven the shape, and neither call
/// proves the id names a row that exists.
///
/// Returns the embedded UUID on success.
pub fn parse_with_prefix(typed_id: &str, expected_prefix: &str) -> Result<uuid::Uuid, ParseError> {
    let (got, uuid) = parse(typed_id).map_err(ParseError::Malformed)?;
    if got != expected_prefix {
        return Err(ParseError::WrongPrefix {
            expected: expected_prefix.to_string(),
            got: got.to_string(),
        });
    }
    Ok(uuid)
}
