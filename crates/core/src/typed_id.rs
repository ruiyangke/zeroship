//! Typed IDs — UUIDv7 with entity-type prefix and base62 encoding.
//!
//! Format: `{prefix}_{base62(uuidv7)}` — e.g. `usr_0Bk3Np4qR5sT7uV8wYz1A`
//!
//! - UUIDv7: timestamp-ordered, globally unique, sortable by creation time
//! - Base62: `0-9A-Za-z`, 22 chars for 128 bits, case-sensitive
//! - Prefix: entity type (`usr`, `app`, `ses`) for debuggability
//! - PG stores raw UUID; the typed ID is the app-facing format

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

/// Strip the prefix and decode to UUID string (hyphenated).
pub fn to_uuid_string(typed_id: &str) -> Result<String, String> {
    let (_, uuid) = parse(typed_id)?;
    Ok(uuid.to_string())
}

/// Encode a UUID string (hyphenated) to typed ID with the given prefix.
pub fn from_uuid_string(prefix: &str, uuid_str: &str) -> Result<String, String> {
    let uuid = uuid::Uuid::parse_str(uuid_str)
        .map_err(|e| format!("invalid UUID: {e}"))?;
    Ok(format!("{}_{}", prefix, uuid_to_base62(&uuid)))
}

// ---------------------------------------------------------------------------
// Well-known prefixes
// ---------------------------------------------------------------------------

pub const USER_PREFIX: &str = "usr";
pub const APP_PREFIX: &str = "app";
pub const SESSION_PREFIX: &str = "ses";

/// Generate a new user ID: `usr_{base62(uuidv7)}`
pub fn new_user_id() -> String {
    generate(USER_PREFIX)
}

/// Generate a new app ID: `app_{base62(uuidv7)}`
pub fn new_app_id() -> String {
    generate(APP_PREFIX)
}

/// Generate a new session ID: `ses_{base62(uuidv7)}`
pub fn new_session_id() -> String {
    generate(SESSION_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_base62() {
        let uuid = uuid::Uuid::now_v7();
        let encoded = uuid_to_base62(&uuid);
        assert_eq!(encoded.len(), 22);
        let decoded = base62_to_uuid(&encoded).unwrap();
        assert_eq!(uuid, decoded);
    }

    #[test]
    fn roundtrip_typed_id() {
        let id = new_user_id();
        assert!(id.starts_with("usr_"));
        assert_eq!(id.len(), 26); // "usr_" + 22
        let (prefix, uuid) = parse(&id).unwrap();
        assert_eq!(prefix, "usr");
        let back = from_uuid_string("usr", &uuid.to_string()).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn sort_order_preserved() {
        // IDs generated later should sort after earlier ones
        let id1 = generate("usr");
        // Small delay to ensure different timestamp
        std::thread::sleep(std::time::Duration::from_millis(2));
        let id2 = generate("usr");
        assert!(id2 > id1, "id2 ({id2}) should sort after id1 ({id1})");
    }

    #[test]
    fn parse_invalid() {
        assert!(parse("nounderscore").is_err());
        assert!(base62_to_uuid("short").is_err());
        assert!(base62_to_uuid("!@#$%^&*()_+{}|:<>?!ab").is_err());
    }

    #[test]
    fn all_prefixes() {
        let u = new_user_id();
        let a = new_app_id();
        let s = new_session_id();
        assert!(u.starts_with("usr_"));
        assert!(a.starts_with("app_"));
        assert!(s.starts_with("ses_"));
    }
}
