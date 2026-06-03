//! Auth utilities: control key validation, API key hashing, bearer extraction,
//! HMAC signing for cross-service identity propagation.

pub mod trusted_clients;

pub use trusted_clients::{
    default_trusted_oauth_clients, is_trusted_client_id, resolve_trusted_oauth_clients,
};

use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

pub const ZEROSHIP_USER_MAX_AGE_SECS: u64 = 60;
pub const ZEROSHIP_USER_FUTURE_SKEW_SECS: u64 = 5;

/// Constant-time comparison to prevent timing attacks.
/// Returns true if `provided` and `expected` are equal.
///
/// The comparison iterates a FIXED number of times — always over the
/// `expected` length — regardless of `provided`'s length. There is no
/// length-dependent early exit and no shorter-of-the-two iteration bound, so
/// the work done (and hence the timing) depends only on `expected.len()`
/// (which is not secret) and never on the attacker-controlled `provided.len()`.
/// A length mismatch folds into the accumulator instead of short-circuiting.
#[must_use]
pub fn validate_control_key(provided: &str, expected: &str) -> bool {
    let (equal, _iterations) = control_key_compare(provided.as_bytes(), expected.as_bytes());
    equal
}

/// Core constant-time comparison. Returns `(equal, iterations)` where
/// `iterations` is the loop count performed — exposed so tests can assert the
/// iteration count depends ONLY on `expected.len()`, never on `provided.len()`.
///
/// Implementation notes (security-critical):
/// - We iterate exactly `expected.len()` times. For each index we read the
///   corresponding `provided` byte if it exists, else a fixed `0`. This keeps
///   the loop bound independent of `provided.len()`.
/// - The length difference is folded into `diff` (via `len_mismatch`) rather
///   than producing an early return, so a wrong-length input takes the same
///   path as a wrong-content input of the right length.
/// - Indexing uses `.get()` (no panic) instead of slice indexing, so an empty
///   or short `provided` cannot panic.
fn control_key_compare(provided: &[u8], expected: &[u8]) -> (bool, usize) {
    let len_mismatch: u8 = u8::from(provided.len() != expected.len());

    let mut diff: u8 = 0;
    for (i, &e) in expected.iter().enumerate() {
        // `provided.get(i)` is `None` past the end of a short input; substitute
        // a constant so the per-iteration work is uniform and panic-free.
        let p = *provided.get(i).unwrap_or(&0);
        diff |= p ^ e;
    }

    // Fold the length check in last so it cannot be observed as an early exit.
    (diff == 0 && len_mismatch == 0, expected.len())
}

/// SHA-256 hash of `key`, returned as a lowercase hex string.
/// Used to store API key hashes in the routing table instead of plaintext.
pub fn hash_api_key(key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    hex::encode(hasher.finalize())
}

/// Constant-time validation of a provided API key against its stored SHA-256 hash.
pub fn validate_api_key(provided: &str, stored_hash: &str) -> bool {
    let computed = hash_api_key(provided);
    validate_control_key(&computed, stored_hash)
}

/// Strip the `Bearer ` prefix from an Authorization header value.
/// Returns `None` if the header does not start with `"Bearer "`.
pub fn extract_bearer(header: &str) -> Option<&str> {
    header.strip_prefix("Bearer ")
}

// ---------------------------------------------------------------------------
// HMAC-SHA256 signing — used to sign forwarded identity across trust boundaries
// ---------------------------------------------------------------------------

/// Compute an HMAC-SHA256 over `payload` with `key`, returned as the
/// raw 32-byte tag.
///
/// Used by the federation stash cookie (auth: `ui::oauth_stash`),
/// the pending-link token (auth: `identity::linker`), and the
/// gateway-side RP-callback stash (gateway: `oidc_rp`). Each
/// previously inlined the same three lines — keep it in one place so
/// the constant-time-compare wrappers above stay co-located with the
/// MAC primitive.
#[must_use]
pub fn hmac_sha256(key: &[u8], payload: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(payload);
    mac.finalize().into_bytes().into()
}

/// Compute an HMAC-SHA256 over `payload` with `key`, returned as lowercase hex.
#[must_use]
pub fn hmac_sha256_hex(key: &[u8], payload: &[u8]) -> String {
    hex::encode(hmac_sha256(key, payload))
}

/// Constant-time verify of `expected_hex` against `payload` HMAC-signed with `key`.
#[must_use]
pub fn verify_hmac_sha256_hex(key: &[u8], payload: &[u8], expected_hex: &str) -> bool {
    let computed = hmac_sha256_hex(key, payload);
    validate_control_key(&computed, expected_hex)
}

/// Length of the base62 body of a `pws_…` pairwise subject (after the
/// `pws_` prefix). 20 base62 chars ≈ 119 bits of the HMAC tag — ample to
/// avoid collisions while keeping the id compact (auth-sdk §6.2).
pub const PAIRWISE_SUB_BODY_LEN: usize = 20;

/// The fixed prefix every per-app pairwise subject carries (`pws_…`). The
/// gateway-issued wrapper token's `sub` is ALWAYS one of these (auth-sdk
/// §6.2/G4) — the self-describing-subject invariant (Batch A fix 2) lets the
/// wrapper fast-paths reject, defense-in-depth, any wrapper whose `sub` is the
/// global Hydra UUID rather than a projected pairwise pseudonym.
pub const PAIRWISE_SUB_PREFIX: &str = "pws_";

/// Whether `sub` has the EXACT shape [`derive_pairwise`] mints: the `pws_`
/// prefix followed by exactly [`PAIRWISE_SUB_BODY_LEN`] base62 (`[0-9A-Za-z]`)
/// chars. The gateway wrapper / session cookie is JS-readable by app code, so
/// it MUST NOT carry the global Hydra UUID in any claim; a `sub` that survives
/// this predicate can never be the un-projected global identity (a bare UUID
/// has no `pws_` prefix).
///
/// ## This is a SHAPE inverse, NOT a forgery/trust gate (security-review I8)
///
/// The predicate proves only that a string is shaped like a `derive_pairwise`
/// output — it does NOT prove the string was actually derived for any real
/// `(global_user_id, sector)`. Authenticity is established UPSTREAM, by the
/// cryptographic check that gates every call site:
///
/// - The session-cookie arms (`router/auth.rs`, `auth_token.rs`) call this
///   ONLY AFTER `session_verifier.verify(...)` authenticates a cookie the
///   gateway itself signed — so the `sub` is already trusted; this predicate is
///   a defense-in-depth minter-bug containment check (reject a wrapper a mint
///   bug failed to project), never the thing that decides trust.
/// - The Bearer/DPoP arms NEVER consume an inbound `pws_` as identity at all —
///   they re-derive it with [`derive_pairwise`] from the verified global Hydra
///   subject. There is no path where an attacker-supplied `pws_` is trusted for
///   an authz/lookup decision on the strength of its shape.
///
/// Anchoring on the EXACT minted shape (prefix + fixed length + base62
/// alphabet) rather than a loose `pws_<anything>` closes the gap where a
/// malformed or forged-shape body (`pws_short`, a punctuation/path-traversal
/// body, an over-length splice) would silently pass: such a string is provably
/// not a `derive_pairwise` output, so it is rejected here too.
#[must_use]
pub fn is_pairwise_subject(sub: &str) -> bool {
    match sub.strip_prefix(PAIRWISE_SUB_PREFIX) {
        Some(body) => {
            body.len() == PAIRWISE_SUB_BODY_LEN
                && body.bytes().all(|c| c.is_ascii_alphanumeric())
        }
        None => false,
    }
}

/// Derive the platform-wide pairwise salt from its OWN dedicated secret
/// (auth-sdk §6.2). BOTH the gateway (which mints wrappers / builds the
/// `ZeroShip-User` header and so derives `pws_` per request) AND the control
/// plane (which revokes the per-app token family on a dashboard "disconnect
/// app" — Batch A fix 4) MUST produce byte-identical `pws_…` subjects, so the
/// salt derivation lives in ONE place rather than being re-inlined per crate (a
/// drift in the derive prefix would silently desync the revocation key from the
/// wrapper subject).
///
/// ## Why a DEDICATED secret (MAJOR fix)
///
/// `pws_` is the PERMANENT per-app identity anchor: an app stores it as the
/// foreign key for "who is this user". It MUST NOT change for the life of an
/// app. Earlier this salt was derived from the gateway `stash_signing_key` — a
/// *rotatable* "short-lived OIDC stash cookie" HMAC — which meant rotating that
/// operational key would silently re-key every app's `pws_` for every user and
/// break their stored FKs. This input is therefore its OWN config secret
/// (`PAIRWISE_SALT` / `--pairwise-salt[-file]`), documented as a permanent,
/// never-rotate-without-migration value, independent of the stash/signing key
/// lifecycle. The SAME value must be configured on gateway AND control.
///
/// Domain-separated from `anchor_enc_key` (and the legacy stash usage) by the
/// distinct `pairwise-subject:` prefix so the two derived keys never collide.
#[must_use]
pub fn derive_pairwise_salt(pairwise_salt_secret_bytes: &[u8]) -> [u8; 32] {
    let seed = format!(
        "pairwise-subject:{}",
        String::from_utf8_lossy(pairwise_salt_secret_bytes)
    );
    crate::crypto::derive_key(&seed)
}

/// Derive the per-app pairwise subject (`pws_…`) for a global user under an
/// app's sector identifier (auth-sdk §6.2, the shipped F4-B gateway
/// projection):
///
/// ```text
/// pws = "pws_" + base62( HMAC-SHA256(salt, canonical(global_user_id) || ":" || sector) )[:20]
/// ```
///
/// Deterministic: the same `(global_user_id, sector)` always yields the same
/// `pws_…`, so the gateway can derive it without a DB round-trip and a
/// re-login yields a stable sub. Two apps (distinct sectors) get different
/// subs for the same human, so app JS decoding its own token cannot correlate
/// the user across apps. Rotating `salt` rotates every sub (a deliberate
/// break-glass).
///
/// `salt` is the platform-wide pairwise secret (config); `global_user_id` is
/// the global Hydra subject (the `usr_…` UUID string); `sector` is the app's
/// stable apex origin (`RouteEntry.sector_identifier`).
///
/// ## Input canonicalization (Batch A M1)
///
/// The `global_user_id` argument reaches this function from two shapes across
/// the writers/readers: the RAW Hydra `sub` string (the raw-Hydra Bearer +
/// DPoP-introspection readers) and `Uuid::to_string()` (`/session` exchange +
/// `?mint=1`, `/signout`, the control disconnect-app cascade). Those are
/// byte-identical only WHILE Hydra emits a canonical hyphenated-lowercase
/// UUID. If Hydra ever emits a non-canonical form (uppercase / braces /
/// no-dash), a session cookie's `pws_` would diverge from the
/// `/signout`-written family marker's `pws_`, silently breaking cross-arm
/// revocation. To make the `pws_` independent of the inbound spelling, we
/// canonicalize HERE in the ONE place every writer and reader funnels through:
/// if `global_user_id` parses as a UUID we hash its canonical
/// `Uuid::to_string()` (hyphenated lowercase); otherwise (a non-UUID subject —
/// which the user-session arms already reject before deriving) we hash it
/// verbatim. Either convention picked consistently would work; canonicalizing
/// in the derive itself means no caller can pick the wrong one.
#[must_use]
pub fn derive_pairwise(salt: &[u8], global_user_id: &str, sector: &str) -> String {
    // Normalize the subject to its canonical UUID spelling when it is one, so
    // the derived `pws_` is byte-identical no matter whether the caller passed
    // the raw Hydra sub or `Uuid::to_string()`. Non-UUID subjects (never a
    // real end-user identity on the pairwise paths) hash verbatim.
    let canonical = Uuid::parse_str(global_user_id)
        .map(|u| u.to_string())
        .unwrap_or_else(|_| global_user_id.to_string());
    let mut payload = Vec::with_capacity(canonical.len() + 1 + sector.len());
    payload.extend_from_slice(canonical.as_bytes());
    payload.push(b':');
    payload.extend_from_slice(sector.as_bytes());
    let tag = hmac_sha256(salt, &payload);
    let mut body = crate::typed_id::base62_encode_bytes(&tag);
    body.truncate(PAIRWISE_SUB_BODY_LEN);
    format!("pws_{body}")
}

/// Sign a `ZeroShip-User` JSON payload as:
/// `<base64(json)>.<request_id>.<issued_at_unix_secs>.<hex-hmac>`.
///
/// The HMAC covers the first three segments, binding the identity to one
/// gateway dispatch request and a short issuance window.
#[must_use]
pub fn sign_zeroship_user_header_at(
    key: &[u8],
    user_json: &[u8],
    request_id: Uuid,
    issued_at_secs: u64,
) -> String {
    let payload_b64 = base64::engine::general_purpose::STANDARD.encode(user_json);
    let signed = format!("{payload_b64}.{request_id}.{issued_at_secs}");
    let mac = hmac_sha256_hex(key, signed.as_bytes());
    format!("{signed}.{mac}")
}

#[must_use]
pub fn sign_zeroship_user_header(key: &[u8], user_json: &[u8], request_id: Uuid) -> String {
    let issued_at_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before Unix epoch")
        .as_secs();
    sign_zeroship_user_header_at(key, user_json, request_id, issued_at_secs)
}

/// Verify and decode a request-bound `ZeroShip-User` header.
#[must_use]
pub fn verify_zeroship_user_header(key: &[u8], header: &str) -> Option<String> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    verify_zeroship_user_header_at(key, header, now)
}

/// Verify and decode a request-bound `ZeroShip-User` header at a fixed clock.
#[must_use]
pub fn verify_zeroship_user_header_at(key: &[u8], header: &str, now_secs: u64) -> Option<String> {
    verify_zeroship_user_header_parts_at(key, header, now_secs).map(|verified| verified.user_json)
}

/// Verify and decode a `ZeroShip-User` header for one expected dispatch request.
#[must_use]
pub fn verify_zeroship_user_header_for_request(
    key: &[u8],
    header: &str,
    expected_request_id: Uuid,
) -> Option<String> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    verify_zeroship_user_header_for_request_at(key, header, expected_request_id, now)
}

/// Verify and decode a `ZeroShip-User` header for one expected dispatch request
/// at a fixed clock.
#[must_use]
pub fn verify_zeroship_user_header_for_request_at(
    key: &[u8],
    header: &str,
    expected_request_id: Uuid,
    now_secs: u64,
) -> Option<String> {
    let verified = verify_zeroship_user_header_parts_at(key, header, now_secs)?;
    if verified.request_id != expected_request_id {
        return None;
    }
    Some(verified.user_json)
}

struct VerifiedZeroShipUserHeader {
    user_json: String,
    request_id: Uuid,
}

fn verify_zeroship_user_header_parts_at(
    key: &[u8],
    header: &str,
    now_secs: u64,
) -> Option<VerifiedZeroShipUserHeader> {
    let mut parts = header.split('.');
    let payload_b64 = parts.next()?;
    let request_id = parts.next()?;
    let issued_at = parts.next()?;
    let mac = parts.next()?;
    if parts.next().is_some()
        || payload_b64.is_empty()
        || request_id.is_empty()
        || issued_at.is_empty()
        || mac.is_empty()
    {
        return None;
    }
    let issued_at_secs = issued_at.parse::<u64>().ok()?;
    let request_id = Uuid::parse_str(request_id).ok()?;
    if now_secs.saturating_sub(issued_at_secs) > ZEROSHIP_USER_MAX_AGE_SECS {
        return None;
    }
    if issued_at_secs.saturating_sub(now_secs) > ZEROSHIP_USER_FUTURE_SKEW_SECS {
        return None;
    }

    let signed = format!("{payload_b64}.{request_id}.{issued_at}");
    if !verify_hmac_sha256_hex(key, signed.as_bytes(), mac) {
        return None;
    }

    let json = base64::engine::general_purpose::STANDARD
        .decode(payload_b64)
        .ok()?;
    Some(VerifiedZeroShipUserHeader {
        user_json: String::from_utf8(json).ok()?,
        request_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER_JSON: &[u8] =
        br#"{"id":"usr_123","email":"a@example.com","name":"A","email_verified":true}"#;

    fn legacy_payload_only_user_header(key: &[u8], user_json: &[u8]) -> String {
        let payload_b64 = base64::engine::general_purpose::STANDARD.encode(user_json);
        let mac = hmac_sha256_hex(key, payload_b64.as_bytes());
        format!("{payload_b64}.{mac}")
    }

    fn legacy_payload_only_user_header_verifies(key: &[u8], header: &str) -> bool {
        let Some((payload_b64, mac)) = header.split_once('.') else {
            return false;
        };
        verify_hmac_sha256_hex(key, payload_b64.as_bytes(), mac)
    }

    #[test]
    fn control_key_equal() {
        assert!(validate_control_key("secret", "secret"));
    }

    #[test]
    fn pairwise_is_deterministic_per_app_and_prefixed() {
        let salt = b"platform-pairwise-salt";
        let uid = "0192f1aa-bbbb-7ccc-8ddd-eeeeffff0001";
        let a = derive_pairwise(salt, uid, "https://app-a.zeroship.ai");
        let a2 = derive_pairwise(salt, uid, "https://app-a.zeroship.ai");
        let b = derive_pairwise(salt, uid, "https://app-b.zeroship.ai");

        // Deterministic for the same (user, sector).
        assert_eq!(a, a2);
        // Prefixed + bounded body length.
        assert!(a.starts_with("pws_"), "{a}");
        assert_eq!(a.len(), 4 + PAIRWISE_SUB_BODY_LEN, "{a}");
        // Different sector ⇒ different sub (no cross-app correlation).
        assert_ne!(a, b);
        // The global UUID never appears in the derived sub.
        assert!(!a.contains(uid), "global UUID must not leak into pws_: {a}");
    }

    #[test]
    fn is_pairwise_subject_accepts_derived_and_rejects_uuid() {
        let salt = b"platform-pairwise-salt";
        let uid = "0192f1aa-bbbb-7ccc-8ddd-eeeeffff0001";
        let pws = derive_pairwise(salt, uid, "https://app.zeroship.ai");
        // Every minted pws_ satisfies the inbound invariant.
        assert!(is_pairwise_subject(&pws), "derived pws_ must be accepted: {pws}");
        // A bare global UUID (what the un-projected wrapper would carry) fails.
        assert!(
            !is_pairwise_subject(uid),
            "a global UUID must NOT pass the pairwise-subject invariant"
        );
        // A typed usr_ id is not a pairwise subject.
        assert!(!is_pairwise_subject("usr_abc123"));
        // The dashed-UUID form (with no prefix) is the global identity → reject.
        assert!(!is_pairwise_subject("0192f1aa-bbbb-7ccc-8ddd-eeeeffff0001"));
        // The no-dash (simple) UUID form, also unprefixed → reject.
        assert!(!is_pairwise_subject("0192f1aabbbb7ccc8dddeeeeffff0001"));
        // Empty / prefix-only fail.
        assert!(!is_pairwise_subject(""));
        assert!(!is_pairwise_subject("pws_"));
        // A `pws_…` whose body is a UUID's 32-char no-dash form is NOT a real
        // `derive_pairwise` output (which is exactly PAIRWISE_SUB_BODY_LEN
        // chars), so the tightened shape inverse rejects it (security-review I8).
        assert!(
            !is_pairwise_subject(&format!("pws_{}", uid.replace('-', ""))),
            "a 32-char no-dash-UUID body is over-length → not a derivable subject"
        );
    }

    /// I8 (security-review 2026-06-02): `is_pairwise_subject` must be the EXACT
    /// inverse of [`derive_pairwise`]'s output shape — `pws_` + exactly
    /// [`PAIRWISE_SUB_BODY_LEN`] base62 chars — so a `pws_`-SHAPED string that
    /// `derive_pairwise` could never have minted (wrong length, or a non-base62
    /// byte) is rejected. This is hardening only: at every production call site
    /// the predicate runs DOWNSTREAM of cryptographic authentication (the
    /// session-cookie signature) and the gateway never trusts an inbound `pws_`
    /// for an authz/lookup decision — it always re-derives via `derive_pairwise`
    /// from the verified global subject. But making the predicate an exact
    /// shape-inverse closes the "any `pws_<anything>` passes" gap so the check
    /// cannot silently wave through a malformed/forged-shape subject.
    #[test]
    fn is_pairwise_subject_rejects_forged_non_derived_shapes() {
        let salt = b"platform-pairwise-salt";
        let uid = "0192f1aa-bbbb-7ccc-8ddd-eeeeffff0001";
        let minted = derive_pairwise(salt, uid, "https://app.zeroship.ai");
        // The real minted shape is still accepted (sanity).
        assert!(is_pairwise_subject(&minted), "minted pws_ must pass: {minted}");
        // The minted body is exactly PAIRWISE_SUB_BODY_LEN base62 chars.
        let minted_body = minted.strip_prefix(PAIRWISE_SUB_PREFIX).unwrap();
        assert_eq!(minted_body.len(), PAIRWISE_SUB_BODY_LEN);

        // A body SHORTER than a real derivation — never minted → reject.
        assert!(
            !is_pairwise_subject("pws_short"),
            "an under-length pws_ body is not a derivable pairwise subject"
        );
        // A body LONGER than a real derivation (e.g. the 32-char no-dash UUID a
        // test fixture might splice in) — never minted by derive_pairwise → reject.
        assert!(
            !is_pairwise_subject(&format!("pws_{}", "a".repeat(PAIRWISE_SUB_BODY_LEN + 1))),
            "an over-length pws_ body is not a derivable pairwise subject"
        );
        // A body of the RIGHT length but carrying a non-base62 byte — the
        // base62 alphabet is [0-9A-Za-z], so '-', '_', '.', '/', '+', '=' are
        // all out. A forged path-traversal-shaped or punctuation-laced body
        // that happens to be 20 chars must still be rejected.
        assert!(
            !is_pairwise_subject(&format!("pws_{}", "-".repeat(PAIRWISE_SUB_BODY_LEN))),
            "a right-length body of non-base62 bytes is not a derivable subject"
        );
        assert!(
            !is_pairwise_subject("pws_..%2F..%2Fetc%2Fpw"),
            "a punctuation/encoded forged body must be rejected even at length"
        );
        // A right-length, all-base62 body IS accepted — we reject SHAPE, not
        // value (the value's authenticity is the signature's job, not the
        // predicate's). This documents the trust boundary: the predicate is a
        // shape inverse, never a forgery gate on its own.
        assert!(
            is_pairwise_subject(&format!("pws_{}", "a".repeat(PAIRWISE_SUB_BODY_LEN))),
            "a well-shaped (right-length base62) body passes the shape inverse"
        );
    }

    #[test]
    fn derive_pairwise_salt_matches_inline_seed_and_is_deterministic() {
        let stash = b"server-stash-signing-key-bytes----";
        let a = derive_pairwise_salt(stash);
        let b = derive_pairwise_salt(stash);
        assert_eq!(a, b, "salt derivation must be deterministic");
        // Pins the exact seed the gateway + control both rely on — a drift here
        // would desync the revocation key from the wrapper subject.
        let expected = crate::crypto::derive_key(&format!(
            "pairwise-subject:{}",
            String::from_utf8_lossy(stash)
        ));
        assert_eq!(a, expected);
        // Different stash ⇒ different salt (rotating the secret rotates subs).
        assert_ne!(derive_pairwise_salt(b"other-stash-key-32-bytes-long----"), a);
    }

    /// MAJOR fix (pairwise_salt secret lifecycle) — `pws_` derives from its OWN
    /// dedicated secret, so the per-app identity anchor is a pure function of
    /// the DEDICATED `PAIRWISE_SALT` value alone. The same dedicated secret
    /// always yields the same `pws_` for a `(user, sector)` regardless of any
    /// other operational key, and a DIFFERENT dedicated secret would re-key it.
    /// This is what makes the salt safe to keep stable while the stash key (a
    /// distinct, rotatable operational secret) rotates freely.
    #[test]
    fn pws_is_a_pure_function_of_the_dedicated_salt_secret() {
        let dedicated = b"dedicated-pairwise-salt-32+bytes-stable!";
        let user = "0192f1aa-bbbb-7ccc-8ddd-eeeeffff0001";
        let sector = "https://myapp.zeroship.ai";

        let salt = derive_pairwise_salt(dedicated);
        let pws_a = derive_pairwise(&salt, user, sector);
        // Re-deriving from the SAME dedicated secret yields the SAME pws_ — the
        // app's stored FK is stable as long as PAIRWISE_SALT is unchanged.
        let pws_b = derive_pairwise(&derive_pairwise_salt(dedicated), user, sector);
        assert_eq!(pws_a, pws_b, "pws_ must be stable for a fixed dedicated salt");

        // A genuinely DIFFERENT dedicated secret re-keys the anchor — which is
        // exactly why rotating it requires a migration (and why it is NOT the
        // rotatable stash key).
        let pws_rotated =
            derive_pairwise(&derive_pairwise_salt(b"a-completely-different-32+byte-salt-value"), user, sector);
        assert_ne!(
            pws_a, pws_rotated,
            "a different dedicated salt re-keys pws_ (migration-only rotation)"
        );
    }

    #[test]
    fn pairwise_changes_with_salt_and_user() {
        let uid = "0192f1aa-bbbb-7ccc-8ddd-eeeeffff0001";
        let other = "0192f1aa-bbbb-7ccc-8ddd-eeeeffff0002";
        let sector = "https://app.zeroship.ai";
        assert_ne!(
            derive_pairwise(b"salt-1", uid, sector),
            derive_pairwise(b"salt-2", uid, sector),
            "rotating the salt must rotate the sub"
        );
        assert_ne!(
            derive_pairwise(b"salt", uid, sector),
            derive_pairwise(b"salt", other, sector),
            "different users get different subs"
        );
    }

    #[test]
    fn pairwise_is_invariant_to_inbound_uuid_spelling() {
        // Batch A M1 regression: the writers feed `derive_pairwise` either the
        // RAW Hydra sub string (e.g. `/token` passing `claims.sub`) or the
        // canonical `Uuid::to_string()` (e.g. `/signout`, control cascade). If
        // Hydra ever emits a NON-canonical sub spelling (uppercase / braces /
        // no-dash), a `/token`-minted wrapper's `pws_` MUST still equal the
        // `pws_` a `/signout`-style `revoke_family(derive_pairwise(uuid.to_string()))`
        // writes — otherwise a signout silently fails to revoke the live wrapper.
        let salt = b"platform-pairwise-salt";
        let sector = "https://app.zeroship.ai";
        let canonical = "0192f1aa-bbbb-7ccc-8ddd-eeeeffff0001";
        let expected = derive_pairwise(salt, canonical, sector);

        // Every non-canonical spelling of the SAME UUID derives the SAME pws_.
        for spelling in [
            canonical.to_uppercase(),                          // uppercase hex
            "0192F1AABBBB7CCC8DDDEEEEFFFF0001".to_string(),    // uppercase, no dashes
            "0192f1aabbbb7ccc8dddeeeeffff0001".to_string(),    // lowercase, no dashes
            "{0192f1aa-bbbb-7ccc-8ddd-eeeeffff0001}".to_string(), // braced
            "urn:uuid:0192f1aa-bbbb-7ccc-8ddd-eeeeffff0001".to_string(), // urn form
        ] {
            assert_eq!(
                derive_pairwise(salt, &spelling, sector),
                expected,
                "pws_ must be invariant to inbound UUID spelling: {spelling}"
            );
        }

        // A genuinely different user still derives a different pws_ (the
        // canonicalization does not collapse distinct UUIDs).
        assert_ne!(
            derive_pairwise(salt, "0192f1aa-bbbb-7ccc-8ddd-eeeeffff0002", sector),
            expected
        );

        // A non-UUID subject (never a real end-user identity on the pairwise
        // paths) hashes verbatim — still deterministic, still prefixed.
        let non_uuid = derive_pairwise(salt, "not-a-uuid", sector);
        assert!(non_uuid.starts_with("pws_"));
        assert_eq!(non_uuid, derive_pairwise(salt, "not-a-uuid", sector));
    }

    #[test]
    fn control_key_different() {
        assert!(!validate_control_key("wrong", "secret"));
    }

    #[test]
    fn control_key_length_mismatch() {
        assert!(!validate_control_key("sec", "secret"));
        assert!(!validate_control_key("secretextra", "secret"));
    }

    /// I10 regression: the comparison must iterate a FIXED number of times —
    /// always `expected.len()` — regardless of `provided`'s length, so the
    /// timing cannot leak the expected secret's length via a min-length /
    /// early-exit iteration bound. The pre-fix implementation iterated
    /// `min(provided.len(), expected.len())`, so a short provided input did
    /// fewer iterations (and a long one did `expected.len()` only because the
    /// bound clamped) — making the iteration count depend on the attacker's
    /// input. This asserts the count is constant across short / exact / long /
    /// empty provided inputs.
    #[test]
    fn control_key_iteration_count_is_independent_of_provided_length() {
        let expected = b"secret-of-known-length";
        let n = expected.len();

        // Various provided lengths: empty, much shorter, one short, exact,
        // one longer, much longer.
        for provided in [
            b"".as_slice(),
            b"x".as_slice(),
            b"short".as_slice(),
            &expected[..n - 1],
            expected.as_slice(),
            b"secret-of-known-length-and-then-some-extra".as_slice(),
        ] {
            let (_equal, iterations) = control_key_compare(provided, expected);
            assert_eq!(
                iterations, n,
                "iteration count must equal expected.len()={n} regardless of \
                 provided.len()={}, got {iterations}",
                provided.len()
            );
        }
    }

    /// I10 correctness: the constant-time comparison still accepts the exact
    /// key and rejects every wrong / wrong-length input.
    #[test]
    fn control_key_constant_time_preserves_correctness() {
        assert!(validate_control_key("secret", "secret"));
        assert!(validate_control_key("", ""));
        // Wrong content, right length.
        assert!(!validate_control_key("secreX", "secret"));
        // Shorter.
        assert!(!validate_control_key("sec", "secret"));
        // Longer (prefix-equal — the classic length-extension trap).
        assert!(!validate_control_key("secretX", "secret"));
        // Empty provided against a non-empty key.
        assert!(!validate_control_key("", "secret"));
        // Non-empty provided against an empty key.
        assert!(!validate_control_key("secret", ""));
    }

    #[test]
    fn hash_is_hex_sha256() {
        let h = hash_api_key("test");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn api_key_roundtrip() {
        let key = "my-api-key";
        let stored = hash_api_key(key);
        assert!(validate_api_key(key, &stored));
        assert!(!validate_api_key("wrong-key", &stored));
    }

    #[test]
    fn bearer_extraction() {
        assert_eq!(extract_bearer("Bearer abc123"), Some("abc123"));
        assert_eq!(extract_bearer("Basic abc123"), None);
        assert_eq!(extract_bearer("Bearer "), Some(""));
        assert_eq!(extract_bearer(""), None);
    }

    #[test]
    fn hmac_roundtrip() {
        let key = b"shared-secret";
        let payload = b"user-payload";
        let mac = hmac_sha256_hex(key, payload);
        assert!(verify_hmac_sha256_hex(key, payload, &mac));
    }

    #[test]
    fn hmac_rejects_tampered_payload() {
        let key = b"shared-secret";
        let mac = hmac_sha256_hex(key, b"original");
        assert!(!verify_hmac_sha256_hex(key, b"tampered", &mac));
    }

    #[test]
    fn hmac_rejects_wrong_key() {
        let mac = hmac_sha256_hex(b"key-a", b"payload");
        assert!(!verify_hmac_sha256_hex(b"key-b", b"payload", &mac));
    }

    /// Raw 32-byte HMAC tag matches the hex-encoded form bit-for-bit.
    /// Regression test for the dedupe in
    /// `auth: ui::oauth_stash`/`identity::linker` and `gateway: oidc_rp`
    /// — if `hmac_sha256` ever drifts from `hmac_sha256_hex`, every
    /// federation cookie + pending-link token signed under one and
    /// verified under the other would silently reject. The fixture is a
    /// known-answer test from RFC 4231 §4.2 (HMAC-SHA-256, key 20×0x0b,
    /// data "Hi There").
    #[test]
    fn hmac_raw_matches_hex_and_rfc4231_kat() {
        let key = [0x0b_u8; 20];
        let data = b"Hi There";
        let raw = hmac_sha256(&key, data);
        let hex_form = hmac_sha256_hex(&key, data);
        assert_eq!(hex::encode(raw), hex_form);
        // RFC 4231 §4.2 expected output.
        assert_eq!(
            hex_form,
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn zeroship_user_header_accepts_fresh_request_bound_header() {
        let key = b"worker-secret";
        let request_id = Uuid::parse_str("018f6df3-43f7-7f68-84e0-4f1f9f5f0021").unwrap();
        let now = 1_900_000_000;
        let header = sign_zeroship_user_header_at(key, USER_JSON, request_id, now);

        assert_eq!(
            verify_zeroship_user_header_at(key, &header, now),
            Some(String::from_utf8(USER_JSON.to_vec()).unwrap())
        );
    }

    #[test]
    fn zeroship_user_header_rejects_old_issued_at() {
        let key = b"worker-secret";
        let request_id = Uuid::parse_str("018f6df3-43f7-7f68-84e0-4f1f9f5f0021").unwrap();
        let now = 1_900_000_000;
        let stale = now - 120;
        let header = sign_zeroship_user_header_at(key, USER_JSON, request_id, stale);

        assert_eq!(verify_zeroship_user_header_at(key, &header, now), None);

        let legacy_header = legacy_payload_only_user_header(key, USER_JSON);
        assert!(
            legacy_payload_only_user_header_verifies(key, &legacy_header),
            "payload-only validation had no timestamp to reject a replay"
        );
        assert_eq!(verify_zeroship_user_header_at(key, &legacy_header, now), None);
    }

    #[test]
    fn zeroship_user_header_rejects_future_issued_at() {
        let key = b"worker-secret";
        let request_id = Uuid::parse_str("018f6df3-43f7-7f68-84e0-4f1f9f5f0021").unwrap();
        let now = 1_900_000_000;
        let future = now + 60;
        let header = sign_zeroship_user_header_at(key, USER_JSON, request_id, future);

        assert_eq!(verify_zeroship_user_header_at(key, &header, now), None);
    }

    #[test]
    fn zeroship_user_header_rejects_rebound_request_id() {
        let key = b"worker-secret";
        let request_id = Uuid::parse_str("018f6df3-43f7-7f68-84e0-4f1f9f5f0021").unwrap();
        let other_request_id = Uuid::parse_str("018f6df3-43f7-7f68-84e0-4f1f9f5f0022").unwrap();
        let now = 1_900_000_000;
        let header = sign_zeroship_user_header_at(key, USER_JSON, request_id, now);
        let mut parts: Vec<&str> = header.split('.').collect();
        parts[1] = "018f6df3-43f7-7f68-84e0-4f1f9f5f0022";
        let rebound = parts.join(".");

        assert_eq!(verify_zeroship_user_header_at(key, &rebound, now), None);
        assert_eq!(
            verify_zeroship_user_header_at(
                key,
                &sign_zeroship_user_header_at(key, USER_JSON, other_request_id, now),
                now,
            ),
            Some(String::from_utf8(USER_JSON.to_vec()).unwrap())
        );
    }

    #[test]
    fn zeroship_user_header_rejects_replay_on_different_request() {
        let key = b"worker-secret";
        let request_id = Uuid::parse_str("018f6df3-43f7-7f68-84e0-4f1f9f5f0021").unwrap();
        let other_request_id = Uuid::parse_str("018f6df3-43f7-7f68-84e0-4f1f9f5f0022").unwrap();
        let now = 1_900_000_000;
        let header = sign_zeroship_user_header_at(key, USER_JSON, request_id, now);

        assert_eq!(
            verify_zeroship_user_header_for_request_at(key, &header, request_id, now),
            Some(String::from_utf8(USER_JSON.to_vec()).unwrap())
        );
        assert_eq!(
            verify_zeroship_user_header_for_request_at(key, &header, other_request_id, now),
            None
        );
    }
}
