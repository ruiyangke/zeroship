//! HMAC-signed session-init plumbing — the Rust side of the trust
//! anchor.
//!
//! Part of the always-compiled `auth/*` subtree. See
//! `docs/proposals/db-system-design.md` §12 for the threat-model
//! discussion of the PG-side SECURITY DEFINER design vs the SQLite-side
//! Rust HMAC alternative.
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

use compio_postgres::{Client, Pool};

use super::ADMIN_SCHEMA;
use crate::auth::util::{
    getrandom_or_fallback, hex_decode, hex_encode, iso_timestamp_after, DEFAULT_TOKEN_TTL_SECS,
};
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
///
/// Thin wrapper around [`init_session_with_pid`] that passes
/// `p_pid = None` — i.e. the SECURITY DEFINER falls back to
/// `pg_backend_pid()`, byte-for-byte preserving today's behaviour
/// for every legacy caller (every `b8c_*` integration test, the
/// existing platform mint-and-init flow, etc.).
pub async fn init_session(client: &Client, token: &MintedToken) -> Result<(), DbError> {
    init_session_with_pid(client, token, None).await
}

/// Like [`init_session`], but lets the caller pass the explicit
/// `p_pid` the SECURITY DEFINER uses for HMAC verification.
///
/// **Why this exists** (P3 PR 2, Q-P3-A — the riskiest decision in
/// `docs/proposals/p3-sqlite-auth-implementation-plan.md` §10): the
/// new `SessionMinter` trait separates `mint_session_token` from
/// `init_session`. Trait callers acquire a fresh pool client per
/// call — the init-side `pg_backend_pid()` no longer matches the
/// mint-side PID, so HMAC verification fails on every legitimate
/// call. The trait impl threads the original `token.backend_pid`
/// down to this fn, which passes it as the SECURITY DEFINER's new
/// `p_pid` parameter; the verifier reproduces the mint-time payload
/// even on a different connection.
///
/// `p_pid = None` → SECURITY DEFINER uses `pg_backend_pid()` (the
/// legacy [`init_session`] wrapper takes this path). `Some(pid)` →
/// SECURITY DEFINER uses the explicit value.
///
/// The cryptographic verify body itself is unchanged — only the
/// PID-source-of-truth (PG `pg_backend_pid()` vs caller-supplied)
/// changes. See `crate::auth::bootstrap::install_init_session_function`
/// for the SQL diff.
pub async fn init_session_with_pid(
    client: &Client,
    token: &MintedToken,
    p_pid: Option<i32>,
) -> Result<(), DbError> {
    let nonce_hex = hex_encode(&token.nonce);
    let sig_hex = hex_encode(&token.signature);
    // The 7th argument is the new `p_pid INTEGER DEFAULT NULL`.
    // `query_text_params` takes `&[&str]` — no Option/NULL path on
    // the wire — so we emit the empty string when `p_pid = None`
    // and unwrap it inside SQL via `NULLIF($7, '')::integer`. That
    // produces a TRUE NULL the SECURITY DEFINER's `COALESCE(p_pid,
    // pg_backend_pid())` falls through, byte-for-byte the legacy
    // 6-arg behaviour. When `p_pid = Some(n)`, NULLIF returns the
    // text, and `::integer` parses it.
    let pid_text: String = p_pid.map(|p| p.to_string()).unwrap_or_default();
    client
        .query_text_params(
            &format!(
                "SELECT \"{ADMIN_SCHEMA}\".init_session(
                    $1, $2, $3, decode($4, 'hex'), decode($5, 'hex'), $6::timestamptz,
                    NULLIF($7, '')::integer
                 )"
            ),
            &[
                &token.app_id,
                &token.actor_kind,
                &token.actor_id.clone().unwrap_or_default(),
                &sig_hex,
                &nonce_hex,
                &token.expires_at_iso,
                &pid_text,
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
// PostgresBackend impl of the cross-backend `SessionMinter` trait
// ---------------------------------------------------------------------------
//
// P3 PR 2: declares `impl crate::backend::SessionMinter for
// crate::backend::PostgresBackend` so the PG arm of the
// `BackendHandle::as_postgres()` / `as_sqlite()` accessors carries
// the same surface the SQLite impl (PR 3) will. The impl bodies
// translate the cross-backend `crate::backend::{SessionInit,
// MintedToken}` shape (carries `pid: Option<String>`) into the
// legacy local `auth::session::{SessionInit, MintedToken}` shape
// (no `pid`), call the existing free fns, and translate back.
//
// **Behaviour preservation**: every existing PG caller of the free
// fns (`mint_session_token`, `init_session`, `mint_and_init`,
// `mint_and_init_via_pool`) continues to pass through unchanged —
// they all route through `init_session(...)`, which is now a thin
// wrapper around `init_session_with_pid(client, token, None)` that
// makes the SECURITY DEFINER fall back to `pg_backend_pid()`. The
// trait impl is the ONLY caller that passes `Some(token.backend_pid)`.
//
// **`pid` field — cross-backend vs PG-legacy**: the new
// `SessionInit::pid` / `MintedToken::pid` (project-id per design
// §12) is NOT yet consumed by the PG SECURITY DEFINER body — the PG
// canonical payload still uses `pg_backend_pid()`. SQLite uses
// `pid` verbatim from day one. Per the plan §4, enabling the new
// payload format on PG is a follow-up PR. For now the PG impl
// carries `pid` through `MintedToken` verbatim so SDK round-trips
// don't lose data; the SECURITY DEFINER ignores it.
//
impl crate::backend::SessionMinter for crate::backend::PostgresBackend {
    async fn mint_session_token(
        &self,
        init: crate::backend::SessionInit,
        ttl_secs: Option<i64>,
    ) -> Result<crate::backend::MintedToken, DbError> {
        // Acquire a fresh client from the pool. mint_session_token
        // takes `&Client` (so the SAME backend serves the
        // pg_backend_pid() probe + the sign_session call); the
        // client must outlive the await.
        let client = self
            .pool()
            .get()
            .await
            .map_err(|e| coded_sql("pool get (SessionMinter::mint_session_token)", e))?;

        // Trait → legacy struct translation. The legacy SessionInit
        // has no `pid` field — the PG free fn re-derives it from
        // `pg_backend_pid()` inside the function body. The
        // cross-backend `init.pid` is carried through into the
        // returned MintedToken so the SDK round-trip preserves it.
        let local_init = SessionInit {
            app_id: init.app_id.clone(),
            actor_kind: init.actor_kind.clone(),
            actor_id: init.actor_id.clone(),
        };
        let local_token = mint_session_token(&*client, local_init, ttl_secs).await?;

        // Legacy → trait struct translation. backend_pid is the
        // mint-time `pg_backend_pid()` captured inside the free fn;
        // the trait callsite (init_session below) re-presents it to
        // the SECURITY DEFINER as `p_pid` so HMAC verification
        // reproduces the mint-time payload even on a different
        // pool client.
        Ok(crate::backend::MintedToken {
            app_id: local_token.app_id,
            actor_kind: local_token.actor_kind,
            actor_id: local_token.actor_id,
            pid: init.pid,
            backend_pid: local_token.backend_pid,
            nonce: local_token.nonce,
            expires_at_iso: local_token.expires_at_iso,
            signature: local_token.signature,
        })
    }

    async fn init_session(
        &self,
        token: &crate::backend::MintedToken,
    ) -> Result<(), DbError> {
        let client = self
            .pool()
            .get()
            .await
            .map_err(|e| coded_sql("pool get (SessionMinter::init_session)", e))?;

        // Trait → legacy struct translation. We MUST pass
        // `Some(token.backend_pid)` as `p_pid` — the new client's
        // `pg_backend_pid()` doesn't match the mint-time PID, and
        // without the explicit override the SECURITY DEFINER's HMAC
        // verify would fail on every legitimate call. This is the
        // crux of Q-P3-A (the riskiest decision).
        let local_token = MintedToken {
            app_id: token.app_id.clone(),
            actor_kind: token.actor_kind.clone(),
            actor_id: token.actor_id.clone(),
            backend_pid: token.backend_pid,
            nonce: token.nonce.clone(),
            expires_at_iso: token.expires_at_iso.clone(),
            signature: token.signature.clone(),
        };
        init_session_with_pid(&*client, &local_token, Some(token.backend_pid)).await
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------
//
// TTL default, getrandom fallback, ISO timestamp formatter, and hex
// codec used to live here. They were relocated to `crate::auth::util`
// in P3 PR 1 so the SQLite `SessionMinter` impl (gated only by the
// `sqlite` feature) can reuse them without dragging in the rest of the
// PG-only `auth::*` surface. See
// `docs/proposals/p3-sqlite-auth-implementation-plan.md` §6 (H-1).

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Helper-level tests (hex_roundtrip, iso_format_*,
    // nonce_random_bytes_are_not_all_zero, etc.) moved to
    // `crate::auth::util::tests` alongside the helper bodies in P3 PR 1.

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
