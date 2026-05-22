//! HMAC-signed session-init plumbing — the Rust side of the trust
//! anchor.
//!
//! Two operations matter to the runtime:
//!
//! 1. **Mint a token** (`mint_session_token`).
//!    Called by the platform role over a privileged connection. Asks
//!    Postgres to compute `HMAC(secret, actor_kind || actor_id ||
//!    pid || nonce || expires_at)` — the secret never leaves the DB.
//!
//! 2. **Init a session** (`init_session`).
//!    Called by the worker on every connection acquire. Hands the
//!    signed token to `__zeroship_admin.init_session(...)` which
//!    verifies HMAC + nonce + expiry, then records the session
//!    context for downstream audit-write SECURITY DEFINERs.
//!
//! A nonce is 32 random bytes generated client-side; replay
//! protection is mediated by `__zeroship_admin.session_nonces` with
//! PRIMARY KEY conflict surface as `nonce replay detected`.

use std::time::{SystemTime, UNIX_EPOCH};

use compio_postgres::{Client, Pool};

use super::{ADMIN_SCHEMA, DEFAULT_TOKEN_TTL_SECS};
use crate::error::DbError;

/// Wrap a `compio_postgres::Error` in [`DbError`] with a context phrase
/// so operators see *what* the session layer was doing when the SQL
/// failed. The SQLSTATE classification still drives the `.code`.
///
/// Thin wrapper around the shared variant-walker
/// [`crate::error::coded_sql`] — stamps the `auth/session` module
/// prefix onto the context phrase.
fn coded_sql(context: &str, e: compio_postgres::Error) -> DbError {
    crate::error::coded_sql(&format!("auth/session: {context}"), e)
}

/// A token minted by the platform — the signature + the canonical
/// `(actor_kind, actor_id, nonce, expires_at)` tuple the SECURITY
/// DEFINER `verify_signature` will reconstruct.
///
/// The `pid` is bound at mint time to the Postgres backend that will
/// present the token. That ties the token to a specific connection;
/// stealing the token without also impersonating that backend's PID
/// is useless (the recomputed HMAC won't match).
#[derive(Debug, Clone)]
pub struct MintedToken {
    pub app_id: String,
    pub actor_kind: String,
    pub actor_id: Option<String>,
    pub backend_pid: i32,
    pub nonce: Vec<u8>,
    pub expires_at_iso: String,
    pub signature: Vec<u8>,
}

/// Inputs for [`mint_session_token`]. Keeping this as a struct so the
/// callsite stays readable when the param list grows.
#[derive(Debug, Clone)]
pub struct SessionInit {
    pub app_id: String,
    pub actor_kind: String,
    pub actor_id: Option<String>,
}

/// Mint a session token. Requires a platform-role-authorised pool
/// (otherwise the GRANT EXECUTE on `sign_session` fails with
/// `permission denied`).
///
/// The token's `backend_pid` is queried inside the same transaction
/// as the sign call, so it always matches the backend that produced
/// it. The downstream `init_session` call (on a different connection,
/// possibly under a different role) ties the signature back to the
/// **caller's** PID via `pg_backend_pid()` — meaning the mint flow
/// must run from the **same** backend that will later present the
/// token. The simplest deployment pattern is: every connection
/// acquires its own token, immediately after BEGIN, using its own
/// `pg_backend_pid()`.
///
/// `ttl_secs` defaults to [`DEFAULT_TOKEN_TTL_SECS`] (5 min). A short
/// TTL bounds the replay window for a captured-but-unsigned-token.
pub async fn mint_session_token(
    client: &Client,
    init: SessionInit,
    ttl_secs: Option<i64>,
) -> Result<MintedToken, DbError> {
    // Caller-supplied TTL is taken verbatim — including negative
    // values, which produce a deliberately-expired token for tests.
    // In production the runtime calls with `None` (default 5 min) or
    // a positive seconds value.
    let ttl = ttl_secs.unwrap_or(DEFAULT_TOKEN_TTL_SECS);

    // 32 random bytes — same length the proposal recommends. We
    // generate client-side rather than ask Postgres for the nonce so
    // the same nonce can be presented on the verifying connection
    // (the proposal's flow assumes the signer and verifier
    // co-ordinate on the nonce).
    let mut nonce = vec![0u8; 32];
    getrandom_or_fallback(&mut nonce);

    // We need the backend PID of the connection that will later
    // present this token. The simplest contract: the caller hands us
    // a `Client` that IS that connection, so `pg_backend_pid()`
    // queried here matches.
    let pid_row = client
        .query_text_params("SELECT pg_backend_pid()::text AS pid", &[])
        .await
        .map_err(|e| coded_sql("pg_backend_pid", e))?;
    let pid_str: String = pid_row
        .first()
        .and_then(|r| r.try_get::<_, String>("pid").ok())
        .ok_or_else(|| DbError::internal("auth/session: pg_backend_pid returned no row"))?;
    let pid: i32 = pid_str
        .parse()
        .map_err(|e| DbError::internal(format!("auth/session: parse pid {pid_str:?}: {e}")))?;

    let expires_at_iso = iso_timestamp_after(ttl);

    // Send the nonce as a hex string and have the SQL decode it —
    // simpler than depending on the BYTEA wire-format quirks of
    // text-mode parameters.
    let nonce_hex = hex_encode(&nonce);

    let row = client
        .query_text_params(
            &format!(
                "SELECT encode(\"{ADMIN_SCHEMA}\".sign_session(
                    $1, $2, $3::int4, decode($4, 'hex'), $5::timestamptz
                 ), 'hex') AS sig"
            ),
            &[
                &init.actor_kind,
                &init.actor_id.clone().unwrap_or_default(),
                &pid.to_string(),
                &nonce_hex,
                &expires_at_iso,
            ],
        )
        .await
        .map_err(|e| coded_sql("sign_session", e))?;

    let sig_hex: String = row
        .first()
        .and_then(|r| r.try_get::<_, String>("sig").ok())
        .ok_or_else(|| {
            DbError::internal("auth/session: sign_session returned no signature")
        })?;
    let signature = hex_decode(&sig_hex)
        .map_err(|e| DbError::internal(format!("auth/session: decode sig: {e}")))?;

    Ok(MintedToken {
        app_id: init.app_id,
        actor_kind: init.actor_kind,
        actor_id: init.actor_id,
        backend_pid: pid,
        nonce,
        expires_at_iso,
        signature,
    })
}

/// Extract the structured DETAIL field from a P0001 RAISE EXCEPTION.
///
/// The SECURITY DEFINER `init_session` function in
/// [`crate::auth::bootstrap`] tags each refusal with a stable
/// machine-readable DETAIL token (`session_signature_expired`,
/// `session_nonce_replay`, etc.). The Rust side reads `e.detail()` so
/// classification is locale- / formatter-independent.
///
/// Returns `Some((static_code, operator_message))` if the error is a
/// known P0001 + DETAIL pair; `None` otherwise (the caller falls
/// through to generic SQLSTATE classification).
fn classify_p0001_detail(
    e: &compio_postgres::Error,
) -> Option<(&'static str, &'static str)> {
    let db_err = e.as_db_error()?;
    if db_err.code() != &compio_postgres::error::SqlState::RAISE_EXCEPTION {
        return None;
    }
    classify_detail_token(db_err.detail()?)
}

/// Pure DETAIL-token → (code, operator-facing message) map.
///
/// Extracted from [`classify_p0001_detail`] so the
/// SDK-contract surface (the 5 session refusal codes) is
/// unit-testable without standing up a real `compio_postgres::Error`
/// fixture. Any change to one of these tokens MUST be paired with the
/// matching `USING DETAIL = '<token>'` literal in
/// [`crate::auth::bootstrap`]'s CREATE FUNCTION body — the
/// `classify_detail_*` test cluster pins the contract.
fn classify_detail_token(detail: &str) -> Option<(&'static str, &'static str)> {
    match detail {
        "session_signature_expired" => {
            Some(("session_signature_expired", "auth/session: signature expired"))
        }
        "session_nonce_replay" => {
            Some(("session_nonce_replay", "auth/session: nonce replay detected"))
        }
        "session_invalid_signature" => Some((
            "session_invalid_signature",
            "auth/session: invalid signature",
        )),
        "session_invalid_actor_kind" => Some((
            "session_invalid_actor_kind",
            "auth/session: invalid actor_kind",
        )),
        "session_nonce_too_short" => Some((
            "session_nonce_too_short",
            "auth/session: nonce too short (need >=16 bytes)",
        )),
        _ => None,
    }
}

/// Hand a minted token to `__zeroship_admin.init_session`. Must be
/// called on the same backend PID the token was minted for — the
/// SECURITY DEFINER function uses `pg_backend_pid()` to re-derive the
/// payload and re-verify the HMAC.
pub async fn init_session(client: &Client, token: &MintedToken) -> Result<(), DbError> {
    let nonce_hex = hex_encode(&token.nonce);
    let sig_hex = hex_encode(&token.signature);
    client
        .query_text_params(
            &format!(
                "SELECT \"{ADMIN_SCHEMA}\".init_session(
                    $1, $2, $3, decode($4, 'hex'), decode($5, 'hex'), $6::timestamptz
                 )"
            ),
            &[
                &token.app_id,
                &token.actor_kind,
                &token.actor_id.clone().unwrap_or_default(),
                &sig_hex,
                &nonce_hex,
                &token.expires_at_iso,
            ],
        )
        .await
        .map(|_| ())
        .map_err(|e| {
            // Promote structured RAISEs to typed ValidationFailed
            // variants with stable `.code`s. The SECURITY DEFINER
            // function raises SQLSTATE P0001 with a machine-readable
            // DETAIL token (set in auth/bootstrap.rs's CREATE FUNCTION
            // body) — discriminate on DETAIL via
            // [`classify_p0001_detail`], not on free-text message
            // substrings (MAJOR-R5-1: substring matching was fragile
            // against RAISE additions, formatter changes, locale).
            //
            // Anything else — SQLSTATE class 23, transient connection
            // failures, unknown P0001 detail, etc. — flows through
            // [`coded_sql`] verbatim. (perf r7 N7-M0: the prior
            // implementation built `format!("{e}")` + walked the
            // source chain even though `classify_p0001_detail` reads
            // detail() borrow-only; removed the dead allocation.)
            match classify_p0001_detail(&e) {
                Some((code, op_msg)) => DbError::validation(code, op_msg),
                None => coded_sql("init_session", e),
            }
        })
}

/// Convenience: mint a token on `client` and then immediately
/// present it back to the same connection. Most call-sites want this
/// shape — the worker runs `pg_backend_pid()` once, signs against it,
/// then proves the binding by calling init_session on the same conn.
pub async fn mint_and_init(
    client: &Client,
    init: SessionInit,
    ttl_secs: Option<i64>,
) -> Result<MintedToken, DbError> {
    let token = mint_session_token(client, init, ttl_secs).await?;
    init_session(client, &token).await?;
    Ok(token)
}

/// Open a separate connection (under the same URL) to run the
/// platform-bound mint/init pair. Used by tests + by call-sites that
/// don't yet have a Client they own.
pub async fn mint_and_init_via_pool(
    pool: &Pool,
    init: SessionInit,
    ttl_secs: Option<i64>,
) -> Result<MintedToken, DbError> {
    let client = pool
        .get()
        .await
        .map_err(|e| coded_sql("pool get", e))?;
    mint_and_init(&*client, init, ttl_secs).await
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Best-effort random fill — prefers `/dev/urandom`; falls back to a
/// time-perturbed XOR stream if unavailable. The XOR fallback is good
/// enough for "nonce" uniqueness (the proposal's threat model assumes
/// the HMAC key, not the nonce, is the secret) but logs a warning so
/// production deployments notice the missing entropy source.
fn getrandom_or_fallback(buf: &mut [u8]) {
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        if f.read_exact(buf).is_ok() {
            return;
        }
    }
    // Fallback — never expected in production. The proposal requires
    // pgcrypto for the HMAC key (which IS the secret); the nonce only
    // needs to be unique within the retention window.
    tracing::error!("auth/session: /dev/urandom unavailable, using time-perturbed fallback");
    let mut t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    for b in buf.iter_mut() {
        t = t.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *b = (t >> 33) as u8;
    }
}

/// ISO-8601 with millisecond precision in UTC — matches the SQL
/// format string `YYYY-MM-DD"T"HH24:MI:SS.MS`.
fn iso_timestamp_after(ttl_secs: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let total_ms = now + ttl_secs.saturating_mul(1000);
    format_unix_millis(total_ms)
}

/// Format a Unix-millisecond timestamp as `YYYY-MM-DDTHH:MM:SS.mmm`
/// in UTC. We roll our own to avoid pulling chrono into plugin-db's
/// dependency graph (the rest of the crate gets by without it).
fn format_unix_millis(ms: i64) -> String {
    // Algorithm: Howard Hinnant's "days_from_civil" inversion.
    let secs = ms / 1000;
    let ms_frac = (ms % 1000).abs();
    let days = secs.div_euclid(86_400);
    let time_in_day = secs.rem_euclid(86_400);
    let h = time_in_day / 3600;
    let m = (time_in_day % 3600) / 60;
    let s = time_in_day % 60;

    let (y, mo, d) = civil_from_days(days);
    format!(
        "{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}.{ms_frac:03}",
    )
}

/// Convert days-since-Unix-epoch to (year, month, day) — Hinnant's
/// algorithm. Handles negative inputs (we never see those, but the
/// math is the same).
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = (y + i64::from(m <= 2)) as i32;
    (year, m as u32, d as u32)
}

fn hex_encode(b: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(b.len() * 2);
    for &x in b {
        out.push(HEX[(x >> 4) as usize] as char);
        out.push(HEX[(x & 0xF) as usize] as char);
    }
    out
}

fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    if s.len() % 2 != 0 {
        return Err("odd-length hex string".into());
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    for i in (0..bytes.len()).step_by(2) {
        let hi = hex_nibble(bytes[i])?;
        let lo = hex_nibble(bytes[i + 1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_nibble(c: u8) -> Result<u8, String> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(format!("invalid hex digit {:?}", c as char)),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let a = [0xde, 0xad, 0xbe, 0xef, 0x00, 0xff, 0x42];
        let s = hex_encode(&a);
        assert_eq!(s, "deadbeef00ff42");
        assert_eq!(hex_decode(&s).unwrap(), a);
    }

    #[test]
    fn hex_decode_rejects_odd_length() {
        assert!(hex_decode("abc").is_err());
    }

    #[test]
    fn hex_decode_rejects_garbage() {
        assert!(hex_decode("xy").is_err());
    }

    #[test]
    fn iso_format_unix_epoch() {
        assert_eq!(format_unix_millis(0), "1970-01-01T00:00:00.000");
    }

    #[test]
    fn iso_format_known_value() {
        // 2026-05-07T00:00:00.000 UTC = 1_778_112_000_000 ms since epoch.
        let ms: i64 = 1_778_112_000_000;
        let s = format_unix_millis(ms);
        assert_eq!(s, "2026-05-07T00:00:00.000");
    }

    #[test]
    fn iso_format_includes_milliseconds() {
        let ms: i64 = 1_778_112_000_123;
        assert!(
            format_unix_millis(ms).ends_with(".123"),
            "got: {}",
            format_unix_millis(ms)
        );
    }

    #[test]
    fn nonce_random_bytes_are_not_all_zero() {
        let mut b = [0u8; 32];
        getrandom_or_fallback(&mut b);
        assert!(b.iter().any(|&x| x != 0), "got all-zero nonce");
    }

    #[test]
    fn token_struct_shape_has_required_fields() {
        let t = MintedToken {
            app_id: "x".into(),
            actor_kind: "platform".into(),
            actor_id: None,
            backend_pid: 1234,
            nonce: vec![1, 2, 3],
            expires_at_iso: "2026-01-01T00:00:00.000".into(),
            signature: vec![9, 9, 9],
        };
        assert_eq!(t.backend_pid, 1234);
        assert_eq!(t.nonce.len(), 3);
    }

    // -----------------------------------------------------------------
    // Typed-error sweep [I28]
    //
    // The signature of `mint_session_token` / `init_session` is now
    // `Result<_, DbError>`. The wire-shape promotion (`P0001` RAISE →
    // `ValidationFailed { code: "session_*"}`) happens inside
    // `init_session` and is exercised end-to-end by
    // `tests/integration.rs::b8c_init_session_rejects_*`. Here we pin
    // the *signature* (type-level contract) so a future refactor that
    // accidentally flattens back to `Result<_, String>` fails compile.
    // -----------------------------------------------------------------

    /// Type-level guard: `mint_session_token` / `init_session` /
    /// `mint_and_init` / `mint_and_init_via_pool` all return
    /// `Result<_, DbError>`. If any of them regresses to
    /// `Result<_, String>` the assignment below stops compiling.
    #[test]
    fn session_helpers_signatures_are_typed() {
        use compio_postgres::{Client, Pool};
        fn _mint(c: &Client) -> impl std::future::Future<Output = Result<MintedToken, DbError>> + '_
        {
            mint_session_token(
                c,
                SessionInit {
                    app_id: "x".into(),
                    actor_kind: "platform".into(),
                    actor_id: None,
                },
                None,
            )
        }
        fn _init<'a>(
            c: &'a Client,
            t: &'a MintedToken,
        ) -> impl std::future::Future<Output = Result<(), DbError>> + 'a {
            init_session(c, t)
        }
        fn _mi(c: &Client) -> impl std::future::Future<Output = Result<MintedToken, DbError>> + '_ {
            mint_and_init(
                c,
                SessionInit {
                    app_id: "x".into(),
                    actor_kind: "platform".into(),
                    actor_id: None,
                },
                None,
            )
        }
        fn _mip(
            p: &Pool,
        ) -> impl std::future::Future<Output = Result<MintedToken, DbError>> + '_ {
            mint_and_init_via_pool(
                p,
                SessionInit {
                    app_id: "x".into(),
                    actor_kind: "platform".into(),
                    actor_id: None,
                },
                None,
            )
        }
        // No actual call — just instantiating the futures proves the
        // signatures are typed.
        let _ = (_mint as fn(_) -> _, _init as fn(_, _) -> _, _mi as fn(_) -> _, _mip as fn(_) -> _);
    }

    // ----- classify_detail_token contract (MAJOR-R5-1; test-coverage r8) -----
    //
    // The 5 DETAIL tokens are the SDK-facing contract. Any change to
    // a token name on the PG side (auth/bootstrap.rs's CREATE
    // FUNCTION body) MUST be matched here or the SDK silently loses
    // its `.code` discrimination on that refusal class.

    #[test]
    fn classify_detail_signature_expired() {
        let (code, msg) = classify_detail_token("session_signature_expired").unwrap();
        assert_eq!(code, "session_signature_expired");
        assert!(msg.contains("signature expired"));
    }

    #[test]
    fn classify_detail_nonce_replay() {
        let (code, msg) = classify_detail_token("session_nonce_replay").unwrap();
        assert_eq!(code, "session_nonce_replay");
        assert!(msg.contains("nonce replay"));
    }

    #[test]
    fn classify_detail_invalid_signature() {
        let (code, msg) = classify_detail_token("session_invalid_signature").unwrap();
        assert_eq!(code, "session_invalid_signature");
        assert!(msg.contains("invalid signature"));
    }

    #[test]
    fn classify_detail_invalid_actor_kind() {
        let (code, msg) = classify_detail_token("session_invalid_actor_kind").unwrap();
        assert_eq!(code, "session_invalid_actor_kind");
        assert!(msg.contains("actor_kind"));
    }

    #[test]
    fn classify_detail_nonce_too_short() {
        let (code, msg) = classify_detail_token("session_nonce_too_short").unwrap();
        assert_eq!(code, "session_nonce_too_short");
        assert!(msg.contains(">=16"));
    }

    #[test]
    fn classify_detail_unknown_token_returns_none() {
        // Critical SDK-contract property: unknown DETAIL must NOT
        // bucket into one of the known codes — caller falls through
        // to generic SQLSTATE classification.
        assert!(classify_detail_token("session_unknown_future_token").is_none());
        assert!(classify_detail_token("").is_none());
        assert!(classify_detail_token("nonce_replay").is_none()); // missing prefix
    }

    #[test]
    fn classify_detail_codes_are_distinct() {
        let codes: std::collections::HashSet<&str> = [
            "session_signature_expired",
            "session_nonce_replay",
            "session_invalid_signature",
            "session_invalid_actor_kind",
            "session_nonce_too_short",
        ]
        .iter()
        .map(|t| classify_detail_token(t).unwrap().0)
        .collect();
        assert_eq!(codes.len(), 5, "all 5 detail tokens must map to distinct codes");
    }
}
