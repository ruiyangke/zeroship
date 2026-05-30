//! Relay alias persistence on `auth.app_user_identities` (Slice 5b).
//!
//! Four things live here, all keyed on the row the gateway (Slice 4) writes:
//!
//! 1. [`ensure_relay_alias`] — mint (or reuse) the `{token}@{relay_domain}`
//!    alias at consent time (sub-spec §2/§6.1). Off the hot path, with
//!    collision generate-and-retry and re-grant stability (the SAME alias is
//!    reused on re-grant, never rotated — Apple Hide-My-Email model).
//! 2. [`resolve_active_alias`] — the inbound handler's alias→real-inbox JOIN
//!    with TWO ANDed liveness gates: the local `revoked_at IS NULL` flag
//!    (sub-spec §4.5) AND a structural `EXISTS (control.oauth_grants …)` on the
//!    grant ledger (sub-spec §6, "STRUCTURAL gate, NOT a shared lock" — the
//!    load-bearing guard that closes the cross-service revoke↔re-consent race
//!    without the writers sharing a lock). A revoked/unknown alias, or one whose
//!    grant was DELETEd, returns `None`, which the handler turns into an explicit
//!    bounce.
//! 3. [`already_seen`] — a NON-committing read-only probe of the `MessageID`
//!    dedup sentinel, run early for replay economy (sub-spec §7.1).
//! 4. [`commit_seen`] — commits the `MessageID` dedup sentinel, run ONLY at a
//!    terminal outcome (forward sent / deliberate drop / bounce). NEVER before a
//!    retryable (503) gate, so a transient-fault retry from Postmark is not
//!    deduped away into a silent drop (sub-spec §7.1 / §8 never-silent-drop).
//!
//! Every function takes a `&Client` (the shared `Arc<Client>` deref or a
//! dedicated owned client) — none opens a transaction, so the `Arc<Client>`
//! constraint the sub-spec calls out (no `&mut self`) is respected.

use compio_postgres::Client;
use rand::Rng;
use uuid::Uuid;

use crate::error::{AuthError, Result};

/// Alias token length (lowercase base36 chars). 12 base36 chars ≈ 62 bits of
/// entropy — unguessable, and collisions are astronomically rare (the
/// generate-and-retry loop below is belt-and-suspenders, not a hot path).
const ALIAS_TOKEN_LEN: usize = 12;
/// Lowercase base36 alphabet. Lowercase-only so the §4.4a normalize-on-read
/// (which lowercases the inbound `OriginalRecipient`) is never lossy.
const ALIAS_ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
/// Bounded retries on the (vanishingly unlikely) active-alias collision.
const MAX_ALIAS_MINT_RETRIES: usize = 8;

/// Resolved active-alias mapping row (sub-spec §4.5).
#[derive(Debug, Clone)]
pub struct AliasTarget {
    /// The user's REAL inbox (`auth.users.email`). Used ONLY as the SMTP
    /// envelope recipient — never written into any forwarded header.
    pub real_inbox: String,
    /// The per-app OAuth client_id (`oac_<base62>`) — the per-app rate-limit
    /// bucket key + the value a revoke cascade matches on.
    pub app_client_id: String,
    pub global_user_id: Uuid,
}

/// Mint (or reuse) a relay alias at CONSENT time (sub-spec §2/§6.1).
///
/// **Why an UPDATE, not the §6.1 INSERT-upsert (resolved ambiguity).** The
/// sub-spec §6.1 sketches the mint as `INSERT … ON CONFLICT … DO UPDATE`, but
/// that INSERT needs `pairwise_sub`, which is **NOT NULL** and, in the
/// implemented Slice-4 architecture, is derivable ONLY by the gateway (it alone
/// holds `pairwise_salt` + the route `sector_identifier`; auth has neither).
/// So the gateway is the single writer of `pairwise_sub` + the row's existence,
/// and consent is the writer of `relay_email`. This function therefore mints
/// onto the **gateway-written row** with an idempotent `UPDATE … COALESCE`:
///
/// - row exists, `relay_email` NULL ⇒ mint a fresh token, generate-and-retry on
///   the active-unique collision, return it.
/// - row exists, `relay_email` set ⇒ reuse it, clear `revoked_at` (re-grant
///   stability, §6.1) — the address is unchanged.
/// - row absent (consent ran before the gateway's first `ZeroShip-User`
///   projection) ⇒ returns `None`. The gateway's **lazy-mint on read-through
///   miss** (main spec §7.1) is the INSERT path that supplies `pairwise_sub`,
///   so the app-facing email is never spuriously null.
///
/// Runs under the consent grant's advisory lock on a dedicated owned `Client`,
/// so two concurrent first-consents mint exactly one alias.
///
/// # Errors
///
/// `AuthError::Db` on PG failure; `AuthError::Internal` if retries are exhausted.
pub async fn mint_alias_at_consent(
    conn: &Client,
    app_client_id: &str,
    global_user_id: Uuid,
    relay_domain: &str,
) -> Result<Option<String>> {
    let existing = conn
        .query(
            "SELECT relay_email, revoked_at IS NULL AS active \
             FROM auth.app_user_identities \
             WHERE app_client_id = $1 AND global_user_id = $2",
            &[&app_client_id, &global_user_id],
        )
        .await
        .map_err(|e| AuthError::Db(format!("consent alias select: {e}")))?;

    let Some(row) = existing.first() else {
        // Row not written yet — defer to the gateway's lazy-mint (§7.1).
        return Ok(None);
    };

    if let Some(email) = row.get::<_, Option<String>>("relay_email") {
        if !row.get::<_, bool>("active") {
            conn.execute(
                "UPDATE auth.app_user_identities SET revoked_at = NULL \
                 WHERE app_client_id = $1 AND global_user_id = $2",
                &[&app_client_id, &global_user_id],
            )
            .await
            .map_err(|e| AuthError::Db(format!("consent alias un-revoke: {e}")))?;
        }
        return Ok(Some(email));
    }

    // relay_email NULL on an existing row — mint with generate-and-retry.
    for _ in 0..MAX_ALIAS_MINT_RETRIES {
        let alias = format!("{}@{relay_domain}", gen_token());
        let affected = conn
            .execute(
                "UPDATE auth.app_user_identities \
                     SET relay_email = $3, revoked_at = NULL \
                 WHERE app_client_id = $1 AND global_user_id = $2 \
                   AND relay_email IS NULL",
                &[&app_client_id, &global_user_id, &alias],
            )
            .await;
        match affected {
            Ok(n) if n > 0 => return Ok(Some(alias)),
            // 0 rows ⇒ a concurrent mint already set it; read it back.
            Ok(_) => {
                let row = conn
                    .query_one(
                        "SELECT relay_email FROM auth.app_user_identities \
                         WHERE app_client_id = $1 AND global_user_id = $2",
                        &[&app_client_id, &global_user_id],
                    )
                    .await
                    .map_err(|e| AuthError::Db(format!("consent alias readback: {e}")))?;
                return Ok(row.get::<_, Option<String>>("relay_email"));
            }
            Err(e) if is_unique_violation(&e) => continue, // token collision; retry
            Err(e) => return Err(AuthError::Db(format!("consent alias mint: {e}"))),
        }
    }
    Err(AuthError::Internal(
        "consent alias mint exhausted retries (active-token collision)".into(),
    ))
}

/// Generate one candidate alias token (lowercase base36).
fn gen_token() -> String {
    let mut rng = rand::thread_rng();
    (0..ALIAS_TOKEN_LEN)
        .map(|_| ALIAS_ALPHABET[rng.gen_range(0..ALIAS_ALPHABET.len())] as char)
        .collect()
}

/// Resolve an alias to its real inbox via the active-map JOIN + revocation gate
/// (sub-spec §4.5). Returns `None` for an unknown or revoked alias (the handler
/// then emits an explicit bounce). `relay_email` is matched exactly (the caller
/// passes a `normalize_alias`-ed, lowercased value).
///
/// `u.email::text` mirrors `users::find_by_id` — the column is `CITEXT`.
///
/// ## Structural revoke-coherence gate (BLOCKER fix, sub-spec §6)
///
/// Forwarding is gated on **TWO** conditions, ANDed:
///
/// 1. the alias's own `revoked_at IS NULL` (the auth-side revocation flag), AND
/// 2. an active grant STILL EXISTS in `control.oauth_grants` for the SAME
///    `(client_id, user_id)` — `EXISTS (SELECT 1 …)`.
///
/// Condition (2) is the load-bearing structural guard. Auth's `accept_consent`
/// (grant upsert + alias un-revoke) and control's `revoke_grant_cascade`
/// (grant DELETE + alias UPDATE) write across two schemas with NO shared mutex
/// — a `pg_advisory_lock` held only by auth does not block control's row
/// DELETE/UPDATE. So `(grant ABSENT + alias revoked_at NULL)` — live forwarding
/// to a real inbox after the grant was revoked — is reachable from an
/// interleaving where control's alias UPDATE loses to a concurrent
/// re-consent's un-revoke. Because a revoke DELETEs the grant row, the
/// `EXISTS` subquery makes a deleted grant **structurally** silence the alias
/// regardless of which writer won the race on `revoked_at`: no grant ⇒ no
/// forwarding, full stop. The two writers no longer need to share a lock; the
/// inbound read derives liveness from the grant ledger (the single source of
/// truth, spec §5.2/§5.4). Cross-schema read on one PG instance is fine
/// (AGENTS.md: one database, separate schemas) — the existing
/// `control.oauth_grants`/`control.app_scope_defs` reads in
/// `ui/consent.rs` already do exactly this from the auth service.
///
/// # Errors
///
/// `AuthError::Db` on PG failure.
pub async fn resolve_active_alias(conn: &Client, alias: &str) -> Result<Option<AliasTarget>> {
    let rows = conn
        .query(
            "SELECT u.email::text AS real_inbox, i.app_client_id, i.global_user_id \
             FROM auth.app_user_identities i \
             JOIN auth.users u ON u.id = i.global_user_id \
             WHERE i.relay_email = $1 \
               AND i.revoked_at IS NULL \
               AND EXISTS ( \
                 SELECT 1 FROM control.oauth_grants g \
                 WHERE g.client_id = i.app_client_id \
                   AND g.user_id = i.global_user_id \
               )",
            &[&alias],
        )
        .await
        .map_err(|e| AuthError::Db(format!("relay alias resolve: {e}")))?;
    Ok(rows.first().map(|row| AliasTarget {
        real_inbox: row.get("real_inbox"),
        app_client_id: row.get("app_client_id"),
        global_user_id: row.get("global_user_id"),
    }))
}

/// Locally disable (revoke) the relay alias for `(app_client_id, global_user_id)`
/// by stamping `revoked_at = now()` on the auth-owned `auth.app_user_identities`
/// row — the IMMEDIATE protection the auth service can apply on its OWN
/// connection without a cross-service call (abuse auto-revoke, sub-spec §7).
///
/// This is the honest, in-scope half of the abuse auto-revoke: the auth service
/// owns `app_user_identities.relay_email`, so it can stop its OWN forwarding
/// right now (`resolve_active_alias`'s `revoked_at IS NULL` gate then fails).
/// It does NOT touch `control.oauth_grants` (the full cross-service revoke is a
/// separate, admin-authenticated control endpoint that does not exist yet) — so
/// it must NEVER be reported as a completed cross-service revoke. Returns the
/// number of rows newly revoked (0 ⇒ already revoked / row absent), so the
/// caller can audit reality.
///
/// # Errors
///
/// `AuthError::Db` on PG failure.
pub async fn revoke_local_alias(
    conn: &Client,
    app_client_id: &str,
    global_user_id: Uuid,
) -> Result<u64> {
    conn.execute(
        "UPDATE auth.app_user_identities \
            SET revoked_at = now() \
          WHERE app_client_id = $1 \
            AND global_user_id = $2 \
            AND revoked_at IS NULL",
        &[&app_client_id, &global_user_id],
    )
    .await
    .map_err(|e| AuthError::Db(format!("relay local alias revoke: {e}")))
}

/// The `MessageID` dedup sentinel key in `auth.rate_limits` (used as a generic
/// short-TTL KV: `tokens` is unused, `updated_at` carries the seen-time).
fn seen_key(message_id: &str) -> String {
    format!("relay_seen:{message_id}")
}

/// NON-committing read-only probe of the `MessageID` dedup sentinel
/// (sub-spec §7.1). Returns `true` when a NON-expired sentinel already exists —
/// i.e. we already terminally handled (forwarded / bounced / dropped) this
/// message, so the caller drops the replay with no side effect. Returns `false`
/// for a fresh id (the caller proceeds through the gates).
///
/// **Crucially this does NOT write the sentinel.** The sentinel is committed
/// only at a terminal outcome via [`commit_seen`], AFTER all the retryable (503)
/// gates. That ordering is what keeps a Postmark retry of a transient-fault
/// message (DB/mailer momentarily down, or a rate-limit spike) from being
/// deduped away into a silent drop — the bug a commit-before-the-gates ordering
/// caused (sub-spec §8 never-silent-drop / §7 retry-smoothing).
///
/// The TTL is 24h (longer than Postmark's ≤6h retry window); a row older than
/// that is treated as expired and ignored (and reclaimed by [`commit_seen`]).
///
/// # Errors
///
/// `AuthError::Db` on PG failure.
pub async fn already_seen(conn: &Client, message_id: &str) -> Result<bool> {
    let key = seen_key(message_id);
    let rows = conn
        .query(
            "SELECT 1 FROM auth.rate_limits \
             WHERE bucket_key = $1 \
               AND updated_at >= NOW() - INTERVAL '24 hours'",
            &[&key],
        )
        .await
        .map_err(|e| AuthError::Db(format!("relay already_seen: {e}")))?;
    Ok(!rows.is_empty())
}

/// Commit the `MessageID` dedup sentinel at a TERMINAL outcome (sub-spec §7.1) —
/// the message has been forwarded, bounced, or deliberately dropped, so a later
/// replay (or a lost-200 Postmark retry of THIS exact message) is correctly
/// deduped away by [`already_seen`].
///
/// MUST be called only on a 200 (terminal) path, NEVER before a 503 (retryable)
/// gate: a 503 asks Postmark to retry, and a committed sentinel would dedupe
/// that legitimate retry into a silent drop (sub-spec §8).
///
/// Idempotent: re-committing the same id refreshes the sentinel's TTL. Errors
/// are surfaced so the caller can log them, but a commit failure on an
/// already-forwarded message is not itself fatal (worst case a genuine future
/// replay is re-processed — at-least-once, never the silent-drop direction).
///
/// # Errors
///
/// `AuthError::Db` on PG failure.
pub async fn commit_seen(conn: &Client, message_id: &str) -> Result<()> {
    let key = seen_key(message_id);
    // Insert the sentinel, refreshing the TTL on an existing (possibly expired)
    // row. `tokens` is unused (0); `updated_at` carries the seen-time.
    conn.execute(
        "INSERT INTO auth.rate_limits (bucket_key, tokens, updated_at) \
         VALUES ($1, 0, NOW()) \
         ON CONFLICT (bucket_key) DO UPDATE SET updated_at = NOW()",
        &[&key],
    )
    .await
    .map_err(|e| AuthError::Db(format!("relay commit_seen: {e}")))?;
    Ok(())
}

fn is_unique_violation(e: &compio_postgres::Error) -> bool {
    e.as_db_error()
        .is_some_and(|db| db.code().code() == "23505")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gen_token_is_lowercase_base36_of_fixed_len() {
        for _ in 0..50 {
            let t = gen_token();
            assert_eq!(t.len(), ALIAS_TOKEN_LEN);
            assert!(
                t.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
                "token must be lowercase base36: {t}"
            );
            // Critically: lowercasing it (the §4.4a normalize-on-read) is a
            // no-op, so a minted alias always exact-matches its normalized form.
            assert_eq!(t, t.to_ascii_lowercase());
        }
    }
}
