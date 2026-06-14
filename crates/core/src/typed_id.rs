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
/// C-7-LT-PR2: wake-job typed-id prefix. Three chars to match the
/// global `^[a-z]{3}_[A-Za-z0-9]{22}$` shape every other entity uses
/// (api-surface-r16 R16-API2). The pg `wake_jobs.wake_id` column
/// stores the full typed-id string (`wak_<base62>`).
pub const WAKE_PREFIX: &str = "wak";

/// Per-app OAuth `client_id` prefix (auth-sdk Slice 1d, spec §1.1): the
/// deterministic, stable-for-app-life OAuth client id is `oac_<base62-app-id>`.
/// Distinct from [`APP_PREFIX`] (the app *entity* typed_id) on purpose — the
/// OAuth `client_id` is a derived identifier, not a typed_id.
///
/// This is the SINGLE source of truth for the prefix string. The control plane
/// mints the id ([`app_oauth_client_id`]) and the auth consent classifier
/// decodes it back to the app UUID ([`app_id_from_oauth_client_id`]); both go
/// through this constant so they can never drift.
pub const APP_OAUTH_CLIENT_PREFIX: &str = "oac";

/// Pricing-plan typed-id prefix (billing PR4). Three chars to match the
/// global `^[a-z]{3}_[A-Za-z0-9]{22}$` shape every other entity uses
/// (R16-API2). The `zeroship.plans.id` column stores the full typed-id
/// string (`pln_<base62>`); `apps.plan_id` is an FK into it (no more
/// free-text self-escalation — CT-A1). The catalog mints ids via
/// [`new_plan_id`] and the built-in tiers are seeded with real `pln_…` ids
/// at control bootstrap.
pub const PLAN_PREFIX: &str = "pln";

/// Invoice typed-id prefix (billing schema redesign). Three chars to match the
/// global `^[a-z]{3}_[A-Za-z0-9]{22}$` shape every other entity uses
/// (R16-API2). The `zeroship.invoices.id` column stores the full typed-id
/// string (`inv_<base62>`); the provider-ref + line side tables FK into it.
pub const INVOICE_PREFIX: &str = "inv";

/// Invoice-payment typed-id prefix (billing-ops gap #26, PR-1). Three chars to
/// match the global `^[a-z]{3}_[A-Za-z0-9]{22}$` shape every other entity uses
/// (R16-API2). `ipy` (NOT the design's 4-char `ipay`, which would break the
/// 3-char invariant) and disjoint from `inv` so a payment id can never be
/// confused with the invoice it FKs into. The `zeroship.invoice_payments.id`
/// column stores the full typed-id string (`ipy_<base62>`), minted in Rust by
/// the payment-confirmation webhook (no SQL `DEFAULT` — there is no in-DB base62
/// generator).
pub const INVOICE_PAYMENT_PREFIX: &str = "ipy";

/// Mint the per-app OAuth `client_id` for an app: `oac_<base62-app-id>`.
/// Deterministic and stable for the life of the app (spec §1.1).
#[must_use]
pub fn app_oauth_client_id(app_id: &uuid::Uuid) -> String {
    format!("{APP_OAUTH_CLIENT_PREFIX}_{}", uuid_to_base62(app_id))
}

/// Decode a per-app OAuth `client_id` (`oac_<base62-app-id>`) back to its app
/// UUID. Returns `None` for any client id that is not a per-app end-user client
/// (a missing `oac_` prefix or a non-base62 tail), e.g. the builder/console
/// clients. The exact inverse of [`app_oauth_client_id`].
#[must_use]
pub fn app_id_from_oauth_client_id(client_id: &str) -> Option<uuid::Uuid> {
    let encoded = client_id.strip_prefix(APP_OAUTH_CLIENT_PREFIX)?.strip_prefix('_')?;
    base62_to_uuid(encoded).ok()
}

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

/// Generate a new wake-job ID: `wak_{base62(uuidv7)}`. Used by the
/// C-7-LT async wake state machine to mint the polling handle handed
/// back to the client on `POST /admin/sandboxes/{id}/wake`.
pub fn new_wake_id() -> String {
    generate(WAKE_PREFIX)
}

/// Generate a new pricing-plan ID: `pln_{base62(uuidv7)}`. Minted by the
/// plan-catalog `upsert` and by the bootstrap seeder for the built-in
/// tiers (billing PR4).
pub fn new_plan_id() -> String {
    generate(PLAN_PREFIX)
}

/// Generate a new invoice ID: `inv_{base62(uuidv7)}`. Minted by the
/// billing reconciler when it claims a `(creator, period)` invoice row.
pub fn new_invoice_id() -> String {
    generate(INVOICE_PREFIX)
}

/// Generate a new invoice-payment ID: `ipy_{base62(uuidv7)}`. Minted by the
/// payment-confirmation webhook when it appends a `charge` row recording the
/// cash actually collected against a finalized invoice (billing-ops gap #26,
/// PR-1).
pub fn new_invoice_payment_id() -> String {
    generate(INVOICE_PAYMENT_PREFIX)
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
    fn base62_encode_bytes_only_base62_chars() {
        // The HMAC-tag encoder must emit only base62 alphabet chars and be
        // deterministic + injective enough that distinct tags differ.
        let a = base62_encode_bytes(&[0xde, 0xad, 0xbe, 0xef, 0x01, 0x02]);
        let b = base62_encode_bytes(&[0xde, 0xad, 0xbe, 0xef, 0x01, 0x03]);
        assert_ne!(a, b);
        assert_eq!(a, base62_encode_bytes(&[0xde, 0xad, 0xbe, 0xef, 0x01, 0x02]));
        assert!(a.bytes().all(|c| BASE62.contains(&c)), "{a}");
        assert!(base62_encode_bytes(&[]).is_empty());
        // A full 32-byte HMAC tag yields >= 20 chars (enough to truncate to
        // the pairwise body length).
        let tag = [0xffu8; 32];
        assert!(base62_encode_bytes(&tag).len() >= 20);
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
    fn app_oauth_client_id_round_trips() {
        let app = uuid::Uuid::now_v7();
        let client_id = app_oauth_client_id(&app);
        assert!(client_id.starts_with("oac_"), "got {client_id}");
        // oac_ + 22 base62 chars.
        assert_eq!(client_id.len(), 26, "got {client_id}");
        // Distinct from the app_ entity typed_id namespace.
        assert!(!client_id.starts_with("app_"));
        // The decode is the exact inverse of the mint — the single-source-of-
        // truth that keeps control (minter) and auth (decoder) from drifting.
        assert_eq!(app_id_from_oauth_client_id(&client_id), Some(app));
        // Non-per-app clients (builder/console) and malformed tails → None.
        assert_eq!(app_id_from_oauth_client_id("zeroship-builder-abc"), None);
        assert_eq!(app_id_from_oauth_client_id("oac_not-base62"), None);
        // The bare prefix with no underscore must not decode.
        assert_eq!(app_id_from_oauth_client_id("oacsomething"), None);
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
        let w = new_wake_id();
        let p = new_plan_id();
        assert!(u.starts_with("usr_"));
        assert!(a.starts_with("app_"));
        assert!(s.starts_with("ses_"));
        assert!(w.starts_with("wak_"));
        assert!(p.starts_with("pln_"));
        // pln_ + 22 base62 chars = 26, and it round-trips through parse().
        assert_eq!(p.len(), 26);
        assert_eq!(PLAN_PREFIX.len(), 3, "plan prefix must be 3 chars (R16-API2)");
        let (prefix, _) = parse(&p).expect("new_plan_id must roundtrip");
        assert_eq!(prefix, "pln");
    }

    /// C-7-LT-PR2 / R16-API2: every typed-id prefix in this crate is
    /// 3 chars. `wak_` (not `wake_`) keeps the global
    /// `^[a-z]{3}_[A-Za-z0-9]{22}$` shape dashboards/log filters key
    /// on. Re-asserted as an invariant test so a future "looks like
    /// 4 chars would be clearer" suggestion fails CI.
    #[test]
    fn wake_prefix_is_three_chars() {
        assert_eq!(WAKE_PREFIX.len(), 3, "wake prefix must be 3 chars (R16-API2)");
        let w = new_wake_id();
        assert_eq!(w.len(), 26, "wak_ + 22 base62 = 26 chars");
        let (prefix, _) = parse(&w).expect("new_wake_id must roundtrip");
        assert_eq!(prefix, "wak");
    }

    #[test]
    fn invoice_prefix_is_three_chars_and_roundtrips() {
        assert_eq!(INVOICE_PREFIX.len(), 3, "invoice prefix must be 3 chars (R16-API2)");
        let i = new_invoice_id();
        assert!(i.starts_with("inv_"));
        assert_eq!(i.len(), 26, "inv_ + 22 base62 = 26 chars");
        let (prefix, _) = parse(&i).expect("new_invoice_id must roundtrip");
        assert_eq!(prefix, "inv");
    }

    #[test]
    fn invoice_payment_prefix_is_three_chars_and_roundtrips() {
        assert_eq!(
            INVOICE_PAYMENT_PREFIX.len(),
            3,
            "invoice-payment prefix must be 3 chars (R16-API2)"
        );
        let p = new_invoice_payment_id();
        assert!(p.starts_with("ipy_"));
        assert_eq!(p.len(), 26, "ipy_ + 22 base62 = 26 chars");
        let (prefix, _) = parse(&p).expect("new_invoice_payment_id must roundtrip");
        assert_eq!(prefix, "ipy");
        assert_ne!(prefix, INVOICE_PREFIX, "payment id must be disjoint from invoice id");
    }

    #[test]
    fn parse_with_prefix_matches() {
        let id = generate("sbx");
        let uuid = parse_with_prefix(&id, "sbx").expect("matching prefix should parse");
        let (_, expected) = parse(&id).unwrap();
        assert_eq!(uuid, expected);
    }

    #[test]
    fn parse_with_prefix_rejects_wrong_prefix() {
        let id = new_user_id(); // prefix = "usr"
        let err =
            parse_with_prefix(&id, "sbx").expect_err("wrong prefix must error");
        match err {
            ParseError::WrongPrefix { expected, got } => {
                assert_eq!(expected, "sbx");
                assert_eq!(got, "usr");
            }
            ParseError::Malformed(_) => panic!("should not classify as malformed"),
        }
    }

    #[test]
    fn parse_with_prefix_rejects_malformed() {
        // No underscore → underlying parse fails first.
        let err = parse_with_prefix("garbage", "sbx").expect_err("malformed must error");
        assert!(matches!(err, ParseError::Malformed(_)));

        // Invalid base62 after a real prefix.
        let err = parse_with_prefix("sbx_!!!notbase62!!!", "sbx")
            .expect_err("invalid base62 must error");
        assert!(matches!(err, ParseError::Malformed(_)));
    }

    #[test]
    fn parse_with_prefix_rejects_empty() {
        let err =
            parse_with_prefix("", "sbx").expect_err("empty input must error");
        assert!(matches!(err, ParseError::Malformed(_)));
    }
}
