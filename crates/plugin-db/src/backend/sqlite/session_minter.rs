//! SQLite `SessionMinter` building blocks — HMAC-SHA256 + bounded
//! LRU nonce cache.
//!
//! ## Why this module exists
//!
//! P3 §5 (`docs/proposals/p3-sqlite-auth-implementation-plan.md`)
//! specifies SQLite's session-minter as **in-memory only**: no
//! `__zeroship_sessions` table, no ATTACH, no schema. Tokens are
//! signed with `HMAC-SHA256(secret, payload)`; replay protection is
//! a bounded LRU set (default 10K). Process restart resets the
//! cache — acceptable per design §12 ("Dev tier").
//!
//! ## Cross-backend payload equivalence
//!
//! [`canonical_payload`] produces the same bytes as the PG SECURITY
//! DEFINER `__zeroship_admin.sign_session` for the same
//! `(actor_kind, actor_id, pid, nonce, expires_at)` tuple:
//!
//! ```text
//! actor_kind || '|' || actor_id || '|' || pid || '|'
//!            || hex(nonce) || '|' || expires_at_iso
//! ```
//!
//! On the PG side `pid` is `p_pid::TEXT` (the integer backend PID
//! as a string); on the SQLite side `pid` is the cross-backend
//! `SessionInit::pid` carried verbatim. The cross-backend
//! equivalence test in PR 4 pins the bytes when both sides are
//! handed positionally identical strings.
//!
//! ## Gating
//!
//! The module compiles only under `--features sqlite` — it's
//! reached from `backend::sqlite::mod` which is itself sqlite-gated.

use std::cell::RefCell;
use std::collections::{HashSet, VecDeque};
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::auth::util::{hex_decode, hex_encode};
use crate::error::DbError;

type HmacSha256 = Hmac<Sha256>;

/// Env var that supplies the active HMAC secret (hex-encoded).
/// Missing in `from_env()` -> `DbError::Configuration { code:
/// "not_configured", … }`. SQLite backends without this set may
/// still boot — the failure is deferred to first mint/init.
pub(crate) const ENV_SECRET: &str = "ZEROSHIP_SESSION_SECRET";

/// Env var that supplies the previous-generation HMAC secret
/// (hex-encoded) during a rotation grace window. Optional.
pub(crate) const ENV_SECRET_PREV: &str = "ZEROSHIP_SESSION_SECRET_PREV";

/// Env var that overrides the nonce LRU capacity. Optional;
/// defaults to [`DEFAULT_NONCE_CAPACITY`].
pub(crate) const ENV_NONCE_CAPACITY: &str = "ZEROSHIP_SESSION_NONCE_CAPACITY";

/// Default ring-buffer capacity for the nonce LRU. Tuned per design
/// §12 — 10K nonces × ~80 bytes ≈ 800 KB per worker.
pub(crate) const DEFAULT_NONCE_CAPACITY: usize = 10_000;

/// Configuration bundle for the SQLite session minter. Read from
/// env vars by [`SqliteSessionMinterConfig::from_env`] or built
/// explicitly by [`crate::backend::sqlite::SqliteBackend::new_with_secrets`]
/// in tests.
#[derive(Debug, Clone)]
pub(crate) struct SqliteSessionMinterConfig {
    /// Active HMAC secret. Mint signs with this; init verifies
    /// against this AND [`Self::secret_prev`] (when present).
    pub(crate) secret: Vec<u8>,
    /// Previous-generation HMAC secret. Tokens minted before a
    /// rotation are still accepted within the grace window. The
    /// `verify_signature` helper always runs both branches when
    /// this is `Some`, never short-circuits, so timing leaks
    /// neither key.
    pub(crate) secret_prev: Option<Vec<u8>>,
    /// Nonce LRU ring-buffer capacity.
    pub(crate) nonce_capacity: usize,
}

impl SqliteSessionMinterConfig {
    /// Read minter configuration from environment variables. Missing
    /// `ZEROSHIP_SESSION_SECRET` returns
    /// `DbError::Configuration { code: "not_configured", … }`;
    /// every other env var is optional.
    pub(crate) fn from_env() -> Result<Self, DbError> {
        let secret_hex = std::env::var(ENV_SECRET).map_err(|_| {
            DbError::Configuration {
                code: "not_configured",
                message: format!(
                    "SQLite SessionMinter not configured (set {ENV_SECRET})"
                ),
                hint: Some(format!(
                    "Generate a 32-byte hex secret with `openssl rand -hex 32` \
                     and export it as {ENV_SECRET}."
                )),
            }
        })?;
        let secret = hex_decode(&secret_hex).map_err(|e| DbError::Configuration {
            code: "not_configured",
            message: format!("{ENV_SECRET} is not valid hex: {e}"),
            hint: None,
        })?;

        let secret_prev = match std::env::var(ENV_SECRET_PREV) {
            Ok(s) if !s.is_empty() => Some(hex_decode(&s).map_err(|e| {
                DbError::Configuration {
                    code: "not_configured",
                    message: format!("{ENV_SECRET_PREV} is not valid hex: {e}"),
                    hint: None,
                }
            })?),
            _ => None,
        };

        let nonce_capacity = match std::env::var(ENV_NONCE_CAPACITY) {
            Ok(s) if !s.is_empty() => s.parse::<usize>().map_err(|e| {
                DbError::Configuration {
                    code: "not_configured",
                    message: format!("{ENV_NONCE_CAPACITY} is not a valid usize: {e}"),
                    hint: None,
                }
            })?,
            _ => DEFAULT_NONCE_CAPACITY,
        };

        Ok(Self {
            secret,
            secret_prev,
            nonce_capacity,
        })
    }
}

/// Bounded LRU nonce cache. Single-threaded by construction —
/// every consumer wraps it in `Rc<RefCell<NonceCache>>` and the
/// SQLite arm runs one backend per worker thread.
///
/// Insertion is `O(1)` amortised; eviction is `O(1)` (`pop_front`
/// + `HashSet::remove`). Memory bound: `capacity × (~32 bytes
/// nonce + 8 bytes expiry + HashSet overhead)` ≈ 800 KB at the
/// default 10K capacity.
#[derive(Debug)]
pub(crate) struct NonceCache {
    /// FIFO ring: oldest at front, newest at back. Each entry is
    /// `(nonce, expires_at_ms)`. We keep `expires_at_ms` alongside
    /// the nonce so the opportunistic-expiry sweep can drop stale
    /// entries without re-parsing ISO strings.
    pub(crate) ring: VecDeque<(Vec<u8>, i64)>,
    /// Membership index for O(1) replay-rejection lookup.
    pub(crate) set: HashSet<Vec<u8>>,
    /// Max entries retained. When `ring.len() == capacity`, the
    /// oldest entry is evicted on the next insert.
    pub(crate) capacity: usize,
}

impl NonceCache {
    /// Construct an empty cache with the given capacity.
    pub(crate) fn new(capacity: usize) -> Self {
        // `capacity == 0` is nonsense — clamp to 1 so the LRU
        // path stays well-defined (always evict before insert).
        let capacity = capacity.max(1);
        Self {
            ring: VecDeque::with_capacity(capacity),
            set: HashSet::with_capacity(capacity),
            capacity,
        }
    }

    /// Build a fresh `Rc<RefCell<NonceCache>>` of the configured
    /// capacity. Convenience for [`crate::backend::sqlite::SqliteBackend`]
    /// constructors.
    pub(crate) fn new_shared(capacity: usize) -> Rc<RefCell<NonceCache>> {
        Rc::new(RefCell::new(NonceCache::new(capacity)))
    }

    /// Insert `nonce` if it hasn't been seen. On duplicate, return
    /// `DbError::validation("session_nonce_replay", …)` — the same
    /// `.code` the PG SECURITY DEFINER emits for the unique-key
    /// conflict path. On insert, evict the oldest entry when the
    /// ring is at capacity, plus opportunistically drop expired
    /// entries from the front of the ring.
    pub(crate) fn insert_if_fresh(
        &mut self,
        nonce: &[u8],
        expires_at_ms: i64,
    ) -> Result<(), DbError> {
        if self.set.contains(nonce) {
            return Err(DbError::validation(
                "session_nonce_replay",
                "session-init nonce replay detected",
            ));
        }

        // Opportunistic expired-entry sweep. Bounded by the
        // ring length but cheap in practice — the front is the
        // oldest, so a single while-loop tail-trims the dead
        // entries every insert.
        let now_ms = current_unix_millis();
        while let Some(front) = self.ring.front() {
            if front.1 < now_ms {
                let (evicted, _) = self.ring.pop_front().expect("front exists");
                self.set.remove(&evicted);
            } else {
                break;
            }
        }

        // Hard cap eviction — only after the expiry sweep so we
        // don't accidentally drop a live entry while a dead one
        // sits at the front.
        if self.ring.len() >= self.capacity {
            if let Some((evicted, _)) = self.ring.pop_front() {
                self.set.remove(&evicted);
            }
        }

        let owned = nonce.to_vec();
        self.set.insert(owned.clone());
        self.ring.push_back((owned, expires_at_ms));
        Ok(())
    }
}

/// Compute the HMAC-SHA256 of `payload` keyed by `secret`. Returns
/// 32 bytes. Wraps the `hmac` + `sha2` crates so callers don't
/// import them directly.
pub(crate) fn compute_signature(secret: &[u8], payload: &[u8]) -> Vec<u8> {
    // `Hmac::new_from_slice` only fails for HMAC backends with
    // fixed key-length requirements — `Hmac<Sha256>` accepts any
    // length, so `expect` is safe and idiomatic for this crate.
    let mut mac =
        HmacSha256::new_from_slice(secret).expect("HMAC-SHA256 accepts any key length");
    mac.update(payload);
    mac.finalize().into_bytes().to_vec()
}

/// Build the canonical signing payload — byte-identical to the
/// PG SECURITY DEFINER `__zeroship_admin.sign_session`:
///
/// ```text
/// actor_kind || '|' || actor_id || '|' || pid || '|'
///            || hex(nonce) || '|' || expires_at_iso
/// ```
///
/// Empty `actor_id` / `pid` mirror PG's `COALESCE(p_actor_id, '')`
/// + `p_pid::TEXT` (the PG impl casts a `NULL`-able integer to
/// text via `COALESCE`).
pub(crate) fn canonical_payload(
    actor_kind: &str,
    actor_id: &str,
    pid: &str,
    nonce: &[u8],
    expires_at_iso: &str,
) -> Vec<u8> {
    let mut out = String::with_capacity(
        actor_kind.len()
            + 1
            + actor_id.len()
            + 1
            + pid.len()
            + 1
            + nonce.len() * 2
            + 1
            + expires_at_iso.len(),
    );
    out.push_str(actor_kind);
    out.push('|');
    out.push_str(actor_id);
    out.push('|');
    out.push_str(pid);
    out.push('|');
    out.push_str(&hex_encode(nonce));
    out.push('|');
    out.push_str(expires_at_iso);
    out.into_bytes()
}

/// Constant-time HMAC verification.
///
/// Computes `HMAC(secret, payload)` and compares to `presented`
/// via a hand-rolled XOR-accumulator (the PG `const_eq` SECURITY
/// DEFINER precedent — no `subtle` dep). When `secret_prev` is
/// `Some(...)`, **both branches always run** (no short-circuit)
/// so the wall-clock time does not leak which key matched.
///
/// Returns `true` iff at least one branch matched.
pub(crate) fn verify_signature(
    secret: &[u8],
    secret_prev: Option<&[u8]>,
    payload: &[u8],
    presented: &[u8],
) -> bool {
    let mut matched = false;

    let computed = compute_signature(secret, payload);
    if const_eq(&computed, presented) {
        matched = true;
    }

    if let Some(prev) = secret_prev {
        let computed_prev = compute_signature(prev, payload);
        if const_eq(&computed_prev, presented) {
            matched = true;
        }
    }

    matched
}

/// Constant-time byte-slice equality. Returns `false` for
/// length-mismatched slices but still consumes the loop in
/// `min(len)` time so an attacker can't gate on early-exit
/// length checks. Mirrors the PG `__zeroship_admin.const_eq`
/// body.
fn const_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff: u8 = (a.len() ^ b.len()) as u8;
    let n = a.len().min(b.len());
    for i in 0..n {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// Current wall-clock time in Unix milliseconds. Used by
/// [`NonceCache::insert_if_fresh`] for opportunistic expiry.
pub(crate) fn current_unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Parse an ISO-8601 timestamp produced by
/// [`crate::auth::util::iso_timestamp_after`] (format
/// `YYYY-MM-DDTHH:MM:SS.mmm`) back to Unix milliseconds.
///
/// Returns `None` on any structural mismatch — `init_session`
/// treats that as a malformed token (caller maps to
/// `session_invalid_signature`, the catch-all reject for "this
/// token doesn't survive parsing"). We don't surface a separate
/// `session_invalid_expiry_format` code because the canonical
/// payload includes the literal ISO string — any tamper would
/// fail HMAC verify anyway, so an explicit-parse-error path
/// would only add a timing-distinguishable code without changing
/// the outcome.
pub(crate) fn parse_iso_to_millis(s: &str) -> Option<i64> {
    // Expected layout: "YYYY-MM-DDTHH:MM:SS.mmm" — fixed-width
    // ASCII, no timezone suffix (the producer always emits UTC).
    if s.len() != 23 {
        return None;
    }
    let b = s.as_bytes();
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':'
        || b[19] != b'.'
    {
        return None;
    }
    let year: i32 = std::str::from_utf8(&b[0..4]).ok()?.parse().ok()?;
    let month: u32 = std::str::from_utf8(&b[5..7]).ok()?.parse().ok()?;
    let day: u32 = std::str::from_utf8(&b[8..10]).ok()?.parse().ok()?;
    let hour: i64 = std::str::from_utf8(&b[11..13]).ok()?.parse().ok()?;
    let minute: i64 = std::str::from_utf8(&b[14..16]).ok()?.parse().ok()?;
    let second: i64 = std::str::from_utf8(&b[17..19]).ok()?.parse().ok()?;
    let millis: i64 = std::str::from_utf8(&b[20..23]).ok()?.parse().ok()?;

    let days = days_from_civil(year, month, day)?;
    let total_secs = days * 86_400 + hour * 3600 + minute * 60 + second;
    Some(total_secs * 1000 + millis)
}

/// Inverse of [`crate::auth::util::civil_from_days`] — converts
/// `(year, month, day)` to days-since-Unix-epoch via Howard
/// Hinnant's `days_from_civil`. Returns `None` for nonsense
/// month/day (out-of-range values trip the early bail; we don't
/// validate "Feb 30" because the producer is our own
/// `iso_timestamp_after` and emits a coherent calendar).
fn days_from_civil(y: i32, m: u32, d: u32) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { i64::from(y) - 1 } else { i64::from(y) };
    let era = y.div_euclid(400);
    let yoe = (y - era * 400) as u64;
    let m = m as i64;
    let d = d as i64;
    let doy = ((153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1) as u64;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe as i64 - 719_468)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_is_deterministic_for_same_input() {
        let secret = b"0123456789abcdef0123456789abcdef";
        let payload = b"actor_kind|alice|proj42|deadbeef|2026-01-01T00:00:00.000";
        let a = compute_signature(secret, payload);
        let b = compute_signature(secret, payload);
        assert_eq!(a, b, "HMAC-SHA256 must be deterministic");
        assert_eq!(a.len(), 32, "HMAC-SHA256 produces 32 bytes");
    }

    #[test]
    fn hmac_differs_when_key_or_payload_changes() {
        let payload = b"actor_kind|alice|proj42|deadbeef|2026-01-01T00:00:00.000";
        let a = compute_signature(b"keyA", payload);
        let b = compute_signature(b"keyB", payload);
        assert_ne!(a, b, "different keys produce different signatures");

        let secret = b"keyA";
        let c = compute_signature(secret, b"payload-1");
        let d = compute_signature(secret, b"payload-2");
        assert_ne!(c, d, "different payloads produce different signatures");
    }

    #[test]
    fn canonical_payload_byte_shape_matches_pg_format() {
        // Mirrors the PG SECURITY DEFINER format byte-for-byte.
        let p = canonical_payload(
            "user",
            "alice",
            "proj_42",
            &[0xde, 0xad, 0xbe, 0xef],
            "2026-05-22T12:00:00.000",
        );
        assert_eq!(
            std::str::from_utf8(&p).unwrap(),
            "user|alice|proj_42|deadbeef|2026-05-22T12:00:00.000",
        );
    }

    #[test]
    fn canonical_payload_handles_empty_actor_id_and_pid() {
        // PG's `COALESCE(p_actor_id, '')` + `p_pid::TEXT` on NULL
        // would never produce an empty `pid` (NULL::TEXT is the
        // literal "NULL" string in PG), but the SQLite-side
        // `init.pid.as_deref().unwrap_or("")` does — and the
        // resulting payload bytes must round-trip through the
        // helper without losing structure.
        let p = canonical_payload("auto", "", "", &[0x00], "1970-01-01T00:00:00.000");
        assert_eq!(
            std::str::from_utf8(&p).unwrap(),
            "auto|||00|1970-01-01T00:00:00.000",
        );
    }

    #[test]
    fn verify_signature_accepts_active_key() {
        let secret = b"active-key";
        let payload = b"some-canonical-payload";
        let sig = compute_signature(secret, payload);
        assert!(verify_signature(secret, None, payload, &sig));
    }

    #[test]
    fn verify_signature_rejects_wrong_signature() {
        let secret = b"active-key";
        let payload = b"some-canonical-payload";
        let mut sig = compute_signature(secret, payload);
        sig[0] ^= 0x01;
        assert!(!verify_signature(secret, None, payload, &sig));
    }

    #[test]
    fn verify_signature_accepts_previous_key_within_grace() {
        let active = b"active-key";
        let prev = b"previous-key";
        let payload = b"canonical-payload";
        let sig = compute_signature(prev, payload);
        assert!(
            verify_signature(active, Some(prev), payload, &sig),
            "token minted under previous key must verify when prev is configured"
        );
    }

    #[test]
    fn verify_signature_rejects_tampered_token_even_with_grace_key() {
        let active = b"active-key";
        let prev = b"previous-key";
        let payload = b"canonical-payload";
        let mut sig = compute_signature(prev, payload);
        sig[15] ^= 0x80;
        assert!(!verify_signature(active, Some(prev), payload, &sig));
    }

    #[test]
    fn const_eq_rejects_length_mismatch() {
        assert!(!const_eq(b"abc", b"abcd"));
        assert!(!const_eq(b"abcd", b"abc"));
        assert!(const_eq(b"abc", b"abc"));
        assert!(const_eq(b"", b""));
    }

    #[test]
    fn nonce_cache_inserts_and_rejects_replay() {
        let mut c = NonceCache::new(8);
        let n = [1u8; 32];
        let exp = current_unix_millis() + 60_000;
        c.insert_if_fresh(&n, exp).expect("first insert ok");
        let err = c
            .insert_if_fresh(&n, exp)
            .expect_err("second insert must be rejected");
        match err {
            DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "session_nonce_replay");
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    #[test]
    fn nonce_cache_lru_evicts_oldest_when_full() {
        // Cap=3, insert 4 distinct nonces, expect the FIRST to be
        // evicted. After eviction, re-inserting the first nonce
        // succeeds (it's no longer in the set).
        let mut c = NonceCache::new(3);
        // Far-future expiry so opportunistic-expiry sweep doesn't
        // interfere with the LRU-by-capacity behaviour under test.
        let exp = current_unix_millis() + 3_600_000;

        c.insert_if_fresh(b"nonce-A-padded-to-32-bytes_aaaa", exp).unwrap();
        c.insert_if_fresh(b"nonce-B-padded-to-32-bytes_bbbb", exp).unwrap();
        c.insert_if_fresh(b"nonce-C-padded-to-32-bytes_cccc", exp).unwrap();
        // Cap reached. Fourth insert evicts A.
        c.insert_if_fresh(b"nonce-D-padded-to-32-bytes_dddd", exp).unwrap();

        assert_eq!(c.ring.len(), 3);
        assert!(!c.set.contains(b"nonce-A-padded-to-32-bytes_aaaa" as &[u8]));
        assert!(c.set.contains(b"nonce-B-padded-to-32-bytes_bbbb" as &[u8]));
        assert!(c.set.contains(b"nonce-C-padded-to-32-bytes_cccc" as &[u8]));
        assert!(c.set.contains(b"nonce-D-padded-to-32-bytes_dddd" as &[u8]));

        // A was evicted — re-inserting it must now succeed.
        c.insert_if_fresh(b"nonce-A-padded-to-32-bytes_aaaa", exp)
            .expect("re-inserting evicted nonce succeeds");
    }

    #[test]
    fn nonce_cache_opportunistic_expiry_sweep() {
        // Bypass `insert_if_fresh` (which runs the sweep itself)
        // when seeding the cache with expired entries — push them
        // straight onto the ring so the sweep has something to
        // chew on. Then confirm a fresh insert evicts both old
        // entries and leaves only the new one behind.
        let mut c = NonceCache::new(10);
        let past = current_unix_millis() - 60_000;
        let n1 = b"old-1-padded-to-32-bytes________".to_vec();
        let n2 = b"old-2-padded-to-32-bytes________".to_vec();
        c.ring.push_back((n1.clone(), past));
        c.set.insert(n1.clone());
        c.ring.push_back((n2.clone(), past));
        c.set.insert(n2.clone());
        assert_eq!(c.ring.len(), 2);

        let future = current_unix_millis() + 60_000;
        c.insert_if_fresh(b"new-1-padded-to-32-bytes________", future)
            .unwrap();

        // The two expired entries should be gone; only the new
        // entry remains.
        assert_eq!(c.ring.len(), 1);
        assert!(!c.set.contains(&n1));
        assert!(!c.set.contains(&n2));
        assert!(c.set.contains(b"new-1-padded-to-32-bytes________" as &[u8]));
    }

    #[test]
    fn parse_iso_round_trips_through_format_unix_millis() {
        // Pick a non-trivial moment in time and round-trip the
        // formatter -> parser. Sub-millisecond precision (.123)
        // must survive verbatim.
        let ms: i64 = 1_778_112_000_123;
        let s = crate::auth::util::format_unix_millis(ms);
        assert_eq!(s, "2026-05-07T00:00:00.123");
        let back = parse_iso_to_millis(&s).expect("parser must accept the formatter's output");
        assert_eq!(back, ms);
    }

    #[test]
    fn parse_iso_rejects_malformed_input() {
        assert!(parse_iso_to_millis("not-an-iso").is_none());
        assert!(parse_iso_to_millis("2026-05-07 00:00:00.000").is_none());
        assert!(parse_iso_to_millis("2026-05-07T00:00:00.0000").is_none());
        assert!(parse_iso_to_millis("").is_none());
    }

    #[test]
    fn nonce_cache_capacity_zero_clamps_to_one() {
        // Defensive: capacity=0 would make `len >= capacity`
        // permanently true and would evict before inserting,
        // making every nonce always fresh — i.e. no replay
        // protection. Clamp to 1 instead.
        let mut c = NonceCache::new(0);
        assert_eq!(c.capacity, 1);
        let exp = current_unix_millis() + 60_000;
        c.insert_if_fresh(b"x", exp).unwrap();
        // Same nonce → replay.
        assert!(c.insert_if_fresh(b"x", exp).is_err());
    }
}
