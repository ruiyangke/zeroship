//! `auth.app_session_anchors` store + the SDK reload-recovery cookies.
//!
//! The anchor is the DEDICATED, durable reload-recovery credential for the
//! `@zeroship/auth` browser SDK (Slice 1b-anchors, spec §8.1) — a SEPARATE
//! store from the 12h/30-min interactive `auth.gateway_sessions`
//! (`crate::sessions`). Anchor-specific lifetime semantics:
//!
//!   - **No idle window.** A reload-recovery anchor exists precisely to
//!     survive long idle gaps, so there is no `idle_expires_at`/slide.
//!   - **`abs_expires_at = created_at + 30d`, set once at create, never
//!     slid.** The 720h Hydra refresh-family ceiling is NOT mirrored into
//!     it; that ceiling is enforced solely by Hydra returning
//!     `invalid_grant` on a `?mint=1` refresh, which the gateway treats as
//!     anchor-dead (delete the row + clear the breadcrumb).
//!   - **`refresh_token_enc`** holds the AES-256-GCM-encrypted server-held
//!     rotating refresh family (`zeroship_core::crypto`). In the default
//!     `server_anchor` mode the browser never holds a refresh token.
//!
//! BFF redesign (`2026-05-30-auth-bff-session-redesign` §3.1): the browser no
//! longer holds a wrapper access token, so the per-anchor cached-WRAPPER slot
//! (the former `cached_access_token` / `cached_access_exp` columns) is gone.
//! Reload-storm coalescing is now provided by the family-rotation single-flight
//! on `GET /__zs/auth/session?mint=1` (still here). The anchor remains the
//! reload-recovery + server-held refresh-family custody store.
//!
//! Every store fn takes a `&Client` (a `PooledClient` derefs to it), so the
//! caller checks a pooled connection out for exactly ONE operation and
//! releases it on drop — NO connection is ever held across the outbound
//! Hydra HTTP call (`crate::db`, the round-6 BLOCKER invariant).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use compio_postgres::Client;
use futures::future::Shared;
use uuid::Uuid;

use crate::error::{GatewayError, Result};

/// Anchor absolute lifetime in days. `abs_expires_at = created_at + 30d`,
/// set once at create and NEVER slid (spec §8.1/§8.3 round-6). This is the
/// SDK reload-recovery anchor's OWN clock — independent of the 720h Hydra
/// family ceiling, which the gateway learns about only via `invalid_grant`.
pub const ANCHOR_ABS_DAYS: i64 = 30;

/// Anchor cookie absolute max-age in seconds (30 days), matching
/// `ANCHOR_ABS_DAYS` and the breadcrumb.
pub const ANCHOR_COOKIE_MAX_AGE_SECS: i64 = ANCHOR_ABS_DAYS * 24 * 3600;

/// Wrapper access-token lifetime in seconds (10 min, §8.5). Still the
/// `exp` of the gateway wrappers minted on the surviving non-browser paths
/// (`/dpop-exchange`); no longer handed to the SPA (BFF redesign §3.1 — the
/// browser holds no wrapper).
pub const WRAPPER_TTL_SECS: i64 = 600;

/// Production anchor cookie name (`__Host-` prefix → Secure required).
///
/// DISTINCT from the interactive OIDC `__Host-zs_app_session`
/// (`oidc_rp::APP_SESSION_COOKIE_PROD`). These are TWO different storage
/// models on the same origin: the interactive flow's cookie is a
/// `auth.gateway_sessions.id` (SameSite=Lax, 12h); the SDK reload-recovery
/// anchor is a `auth.app_session_anchors.id` (SameSite=Strict, 30d). Sharing
/// one name would let a request carrying one be validated against the WRONG
/// table (MAJOR fix). One cookie name ⇒ exactly one table.
pub const ANCHOR_COOKIE_PROD: &str = "__Host-zs_app_anchor";
/// Dev anchor cookie name (no `__Host-` prefix, no Secure).
pub const ANCHOR_COOKIE_DEV: &str = "zs_app_anchor";

/// Resolve the anchor cookie name for the current environment. The anchor has
/// its OWN cookie name (`__Host-zs_app_anchor`), separate from the interactive
/// `oidc_rp::app_session_cookie_name` (`__Host-zs_app_session`), and uses
/// `SameSite=Strict` (vs the interactive cookie's `Lax`) per §8.3.
#[must_use]
pub fn anchor_cookie_name(insecure_dev: bool) -> &'static str {
    if insecure_dev { ANCHOR_COOKIE_DEV } else { ANCHOR_COOKIE_PROD }
}

/// Build the `Set-Cookie` value for the `__Host-zs_app_anchor` anchor.
///
/// `SameSite=Strict` (§8.3 round-2): the anchor is never legitimately
/// needed on a cross-site request, so a top-level navigation cannot ride
/// it. `HttpOnly` (XSS cannot read it). `insecure_dev` drops `Secure` AND
/// the `__Host-` prefix together (RFC 6265bis §4.1.3.2 requires `Secure`
/// for `__Host-`).
#[must_use]
pub fn set_anchor_cookie(anchor_id: &Uuid, insecure_dev: bool) -> String {
    let name = anchor_cookie_name(insecure_dev);
    let secure = if insecure_dev { "" } else { "; Secure" };
    format!(
        "{name}={anchor_id}; Path=/; HttpOnly; SameSite=Strict{secure}; Max-Age={ANCHOR_COOKIE_MAX_AGE_SECS}"
    )
}

/// Clear the anchor cookie (signout / anchor-dead recovery).
#[must_use]
pub fn clear_anchor_cookie(insecure_dev: bool) -> String {
    let name = anchor_cookie_name(insecure_dev);
    let secure = if insecure_dev { "" } else { "; Secure" };
    format!("{name}=; Path=/; HttpOnly; SameSite=Strict{secure}; Max-Age=0")
}

/// Parse the anchor id out of a `Cookie` header value.
#[must_use]
pub fn parse_anchor_cookie(cookie_header: &str, insecure_dev: bool) -> Option<Uuid> {
    let name = anchor_cookie_name(insecure_dev);
    let prefix = format!("{name}=");
    for part in cookie_header.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix(&prefix) {
            return Uuid::parse_str(rest).ok();
        }
    }
    None
}

/// The session-presence breadcrumb cookie name, keyed on the app host.
///
/// `zs.<host>.is.authenticated` — non-HttpOnly so the SDK can read it; it
/// is an OPTIMIZATION ONLY (gates whether the SDK skips a network probe),
/// never a security boundary (the HttpOnly anchor is authoritative, §8.3).
/// The gateway WRITES it server-side on every `/token` and `/session`
/// success so it tracks the anchor.
#[must_use]
pub fn breadcrumb_cookie_name(host: &str) -> String {
    format!("zs.{host}.is.authenticated")
}

/// Build the `Set-Cookie` value for the breadcrumb. Non-HttpOnly,
/// `SameSite=Lax`, `Secure` (dropped in dev), 30-day `Max-Age` matching the
/// anchor (§8.3).
#[must_use]
pub fn set_breadcrumb_cookie(host: &str, insecure_dev: bool) -> String {
    let name = breadcrumb_cookie_name(host);
    let secure = if insecure_dev { "" } else { "; Secure" };
    format!("{name}=true; Path=/; SameSite=Lax{secure}; Max-Age={ANCHOR_COOKIE_MAX_AGE_SECS}")
}

/// Clear the breadcrumb cookie (only on a `401 login_required`, §4.3).
#[must_use]
pub fn clear_breadcrumb_cookie(host: &str, insecure_dev: bool) -> String {
    let name = breadcrumb_cookie_name(host);
    let secure = if insecure_dev { "" } else { "; Secure" };
    format!("{name}=; Path=/; SameSite=Lax{secure}; Max-Age=0")
}

/// One anchor row.
#[derive(Debug, Clone)]
pub struct Anchor {
    pub id: Uuid,
    pub app_id: String,
    pub client_id: String,
    pub global_user_id: Uuid,
    /// AES-256-GCM ciphertext of the server-held refresh family.
    pub refresh_token_enc: Vec<u8>,
    pub refresh_family_id: String,
    pub granted_scopes: Vec<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub abs_expires_at: chrono::DateTime<chrono::Utc>,
}

/// Args for [`create`].
#[derive(Debug)]
pub struct NewAnchor<'a> {
    pub app_id: &'a str,
    pub client_id: &'a str,
    pub global_user_id: Uuid,
    pub refresh_token_enc: &'a [u8],
    pub refresh_family_id: &'a str,
    pub granted_scopes: &'a [String],
}

/// Insert a new anchor row. `abs_expires_at` is computed as
/// `created_at + ANCHOR_ABS_DAYS` IN THE DATABASE (`NOW() + interval`) and
/// returned — it is set ONCE here and never slid.
///
/// # Errors
/// [`GatewayError::Db`] on PG failure or empty return.
pub async fn create(conn: &Client, params: &NewAnchor<'_>) -> Result<Anchor> {
    let scopes: Vec<String> = params.granted_scopes.to_vec();
    let refresh_enc = params.refresh_token_enc.to_vec();
    let rows = conn
        .query(
            "INSERT INTO auth.app_session_anchors \
                (app_id, client_id, global_user_id, refresh_token_enc, refresh_family_id, \
                 granted_scopes, abs_expires_at) \
             VALUES ($1, $2, $3, $4, $5, $6, \
                     NOW() + ($7::text || ' days')::interval) \
             RETURNING id, app_id, client_id, global_user_id, refresh_token_enc, \
                       refresh_family_id, granted_scopes, created_at, abs_expires_at",
            &[
                &params.app_id,
                &params.client_id,
                &params.global_user_id,
                &refresh_enc,
                &params.refresh_family_id,
                &scopes,
                &ANCHOR_ABS_DAYS.to_string(),
            ],
        )
        .await
        .map_err(|e| GatewayError::Db(format!("app_session_anchors create: {e}")))?;

    let row = rows
        .first()
        .ok_or_else(|| GatewayError::Db("app_session_anchors create: empty return".into()))?;
    Ok(row_to_anchor(row))
}

/// Read a LIVE anchor by id. "Live" = exists, not revoked, and within its
/// own 30-day absolute lifetime (`abs_expires_at > NOW()`). Returns `None`
/// when the row is missing, revoked, or past its absolute expiry — the
/// caller treats `None` as `login_required` (and clears the cookie).
///
/// Does NOT slide any expiry — the anchor has no idle window.
///
/// # Errors
/// [`GatewayError::Db`] on PG failure.
pub async fn read_live(conn: &Client, id: Uuid) -> Result<Option<Anchor>> {
    let rows = conn
        .query(
            "SELECT id, app_id, client_id, global_user_id, refresh_token_enc, \
                    refresh_family_id, granted_scopes, created_at, abs_expires_at \
             FROM auth.app_session_anchors \
             WHERE id = $1 AND revoked_at IS NULL AND abs_expires_at > NOW()",
            &[&id],
        )
        .await
        .map_err(|e| GatewayError::Db(format!("app_session_anchors read_live: {e}")))?;
    Ok(rows.first().map(row_to_anchor))
}

/// Persist a rotated refresh family after a `?mint=1` Hydra refresh.
/// `abs_expires_at` and `created_at` are UNTOUCHED — the anchor's own 30-day
/// clock never slides. The browser no longer holds a wrapper (BFF redesign
/// §3.1), so there is no cached wrapper to persist here — only the rotated
/// encrypted refresh family + its lineage id.
///
/// # Errors
/// [`GatewayError::Db`] on PG failure.
pub async fn update_rotated_family(
    conn: &Client,
    id: Uuid,
    refresh_token_enc: &[u8],
    refresh_family_id: &str,
) -> Result<()> {
    let refresh_enc = refresh_token_enc.to_vec();
    conn.execute(
        "UPDATE auth.app_session_anchors SET \
            refresh_token_enc = $2, \
            refresh_family_id = $3 \
         WHERE id = $1 AND revoked_at IS NULL",
        &[&id, &refresh_enc, &refresh_family_id],
    )
    .await
    .map_err(|e| GatewayError::Db(format!("app_session_anchors update_rotated_family: {e}")))?;
    Ok(())
}

/// Hard-delete an anchor row (anchor-dead: Hydra `invalid_grant`, or
/// signout). Idempotent — deleting a missing id is a no-op.
///
/// # Errors
/// [`GatewayError::Db`] on PG failure.
pub async fn delete(conn: &Client, id: Uuid) -> Result<()> {
    conn.execute(
        "DELETE FROM auth.app_session_anchors WHERE id = $1",
        &[&id],
    )
    .await
    .map_err(|e| GatewayError::Db(format!("app_session_anchors delete: {e}")))?;
    Ok(())
}

/// Delete EVERY anchor row for an `(app_id, global_user_id)` pair and
/// return the encrypted refresh families that were removed (auth-sdk Slice
/// 1b-browser, `scope: 'global'` signout — "this app, every device", §1.2).
///
/// Returns each row's `(refresh_token_enc, client_id)` so the caller can
/// best-effort revoke each family at Hydra. The delete is the authoritative
/// step; the returned ciphertexts are only for the (best-effort) Hydra
/// revoke and the `(client_id, sub)` family-marker upsert.
///
/// # Errors
/// [`GatewayError::Db`] on PG failure.
pub async fn delete_all_for_user(
    conn: &Client,
    app_id: &str,
    global_user_id: Uuid,
) -> Result<Vec<DeletedFamily>> {
    let rows = conn
        .query(
            "DELETE FROM auth.app_session_anchors \
             WHERE app_id = $1 AND global_user_id = $2 \
             RETURNING refresh_token_enc, client_id",
            &[&app_id, &global_user_id],
        )
        .await
        .map_err(|e| GatewayError::Db(format!("app_session_anchors delete_all_for_user: {e}")))?;
    Ok(rows
        .iter()
        .map(|row| DeletedFamily {
            refresh_token_enc: row.get("refresh_token_enc"),
            client_id: row.get("client_id"),
        })
        .collect())
}

/// One deleted anchor's family ciphertext + its client_id (for the
/// best-effort Hydra revoke fan-out on `global` signout).
#[derive(Debug, Clone)]
pub struct DeletedFamily {
    pub refresh_token_enc: Vec<u8>,
    pub client_id: String,
}

fn row_to_anchor(row: &compio_postgres::Row) -> Anchor {
    Anchor {
        id: row.get("id"),
        app_id: row.get("app_id"),
        client_id: row.get("client_id"),
        global_user_id: row.get("global_user_id"),
        refresh_token_enc: row.get("refresh_token_enc"),
        refresh_family_id: row.get("refresh_family_id"),
        granted_scopes: row.get("granted_scopes"),
        created_at: row.get("created_at"),
        abs_expires_at: row.get("abs_expires_at"),
    }
}

// ─── Per-node family-rotation single-flight (round-6 BLOCKER) ────────────
//
// Concurrent `?mint=1` reloaders for the SAME anchor on ONE gateway worker
// thread coalesce into ONE Hydra refresh (the "family rotation"). The compio
// model is single-thread per worker (`!Send` futures), so the keyed map is a
// thread-local `RefCell<HashMap<AnchorId, Shared<…>>>` — NOT a cross-thread
// `Mutex`. A `Shared` future is `Clone`, so N callers clone-and-await the SAME
// future; exactly one drives the body (the Hydra refresh), and all N receive
// its cloned result.
//
// The single-flight holds NO db connection and NO lock across the Hydra
// call: the future body itself checks a pooled connection out, reads, then
// RELEASES it before awaiting Hydra, and checks another out afterwards to
// write — see `crate::auth_token::rotate_family` / `do_refresh`.
//
// NOTE on vocabulary: this is FAMILY-ROTATION machinery, not token "minting".
// The only thing the platform calls a "mint" is the deferred control-plane
// power-token mint; the `?mint=1` query flag is the SDK's reload-recovery
// trigger (kept for the wire contract), but the Rust types here are
// `Rotation*` to avoid that confusion.

/// The result a coalesced family-rotation future resolves to. `Clone` so a
/// `Shared` future can hand the same value to every awaiter (the underlying
/// `Output` must be `Clone`).
pub type RotationResult = std::result::Result<RotationOk, RotationError>;

/// A successful reload-recovery family rotation (BFF redesign §2.2 / §3.1).
///
/// The browser no longer receives a wrapper, so this no longer carries an
/// access token. It carries the identity facts the `?mint=1` handler needs to
/// re-create the gateway session from the rotated id_token — the global user
/// UUID (the gateway session `user_id`), the freshly-granted scopes, and the
/// id-token `email_verified` / `name` / `auth_time` / `amr` claims. The
/// relay-alias email swap and `pws_` projection happen in the handler (which
/// holds the route and salts), exactly as on the `/token` path, so the rotated
/// raw Hydra access JWT never leaves the gateway and no JWT reaches the browser.
#[derive(Debug, Clone)]
pub struct RotationOk {
    pub global_user_id: Uuid,
    pub granted_scopes: Vec<String>,
    pub email_verified: Option<bool>,
    pub name: Option<String>,
    /// The `picture` claim, sourced from the rotated id_token when the refresh
    /// grant returns one (BFF minor fix): without this, name/avatar silently
    /// degrade across a reload-recovery vs the original `/token` row, because
    /// the rotated raw ACCESS JWT does not always carry the profile claims.
    pub avatar_url: Option<String>,
    pub auth_time: Option<i64>,
    pub amr: Vec<String>,
}

/// Why a coalesced family rotation failed. `Clone` so a `Shared` future can fan it out.
#[derive(Debug, Clone)]
pub enum RotationError {
    /// The anchor is gone / its family was revoked or hit the 720h ceiling
    /// (Hydra `invalid_grant`). The caller deletes the anchor + clears the
    /// breadcrumb and surfaces `401 login_required`.
    LoginRequired,
    /// A transient upstream/internal failure (Hydra unreachable, DB write
    /// failed, issuer missing, …). The caller surfaces `503`/`500` and does
    /// NOT clear the breadcrumb.
    Upstream(String),
}

/// A type-erased, shared, cloneable rotation future. Boxed so the map can hold
/// futures of one concrete type regardless of the concrete `async` block.
pub type SharedRotationFuture =
    Shared<std::pin::Pin<Box<dyn std::future::Future<Output = RotationResult>>>>;

/// Per-worker-thread coalescing map: `anchor_id → in-flight rotation future`.
///
/// `!Send` (it lives behind a thread-local), matching the single-thread
/// compio worker model. An entry is inserted on the first miss and removed
/// once the future resolves, so the map only ever holds genuinely in-flight
/// rotations.
///
/// Because it is `!Send`, it CANNOT live in the `Arc<GateState>` shared
/// across ntex's worker arbiter threads — exactly the same constraint the
/// `!Send` compio-postgres `Pool` has. So it lives in a thread-local
/// ([`with_single_flight`]); coalescing is per worker thread, which is the
/// intended scope (cross-thread/cross-node concurrency is absorbed by the
/// short cached wrapper + Hydra's rotation grace, §1.2).
#[derive(Default, Clone)]
pub struct RotationSingleFlight {
    inner: Rc<RefCell<HashMap<Uuid, SharedRotationFuture>>>,
}

impl std::fmt::Debug for RotationSingleFlight {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RotationSingleFlight")
            .field("in_flight", &self.inner.borrow().len())
            .finish()
    }
}

impl RotationSingleFlight {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Look up the in-flight rotation for `anchor_id`, cloning the `Shared`
    /// future if one exists. `None` ⇒ the caller is the leader and must
    /// `insert` a fresh future.
    #[must_use]
    pub fn get(&self, anchor_id: Uuid) -> Option<SharedRotationFuture> {
        self.inner.borrow().get(&anchor_id).cloned()
    }

    /// Register `fut` as the in-flight rotation for `anchor_id` and return a
    /// clone to await. If a concurrent leader already registered one (it
    /// cannot on a single thread between two synchronous calls, but the API
    /// stays race-safe), the existing one is returned and `fut` is dropped.
    pub fn insert(&self, anchor_id: Uuid, fut: SharedRotationFuture) -> SharedRotationFuture {
        let mut map = self.inner.borrow_mut();
        map.entry(anchor_id).or_insert(fut).clone()
    }

    /// Remove the in-flight entry once the rotation resolves.
    pub fn remove(&self, anchor_id: Uuid) {
        self.inner.borrow_mut().remove(&anchor_id);
    }

    /// Number of in-flight rotations (test/observability only).
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.inner.borrow().len()
    }
}

thread_local! {
    /// One coalescing map per worker thread. `RotationSingleFlight` is `!Send`
    /// (it holds `Rc<RefCell<…>>`), so it lives here rather than in the
    /// `Arc<GateState>` shared across ntex worker arbiter threads — the
    /// same reason the compio-postgres `Pool` is thread-local (`crate::db`).
    static SINGLE_FLIGHT: RotationSingleFlight = RotationSingleFlight::new();
}

/// Run `f` with this worker thread's family-rotation single-flight. The closure gets a
/// cheap `Clone` of the per-thread map (the `Rc` clone is shared state), so
/// it can `get`/`insert`/`remove` across `.await` points without holding a
/// `RefCell` borrow.
pub fn with_single_flight<R>(f: impl FnOnce(RotationSingleFlight) -> R) -> R {
    SINGLE_FLIGHT.with(|sf| f(sf.clone()))
}

/// RAII guard that removes an `anchor_id` from THIS worker thread's rotation
/// single-flight map when dropped (round-6 BLOCKER invariant: "remove
/// `single_flight.entry` once `fut` resolves").
///
/// The guard is owned by the SHARED rotation future's body, NOT by the leader
/// request task. That distinction is the whole point: a `futures::Shared`
/// future is driven to completion by whichever awaiter is alive, so if the
/// leader's request is cancelled mid-flight (client disconnect / ntex
/// timeout) after the entry was inserted, a surviving follower still drives
/// the future, and the guard fires when the future (and thus its body) is
/// dropped. If EVERY awaiter is dropped before the future resolves, the
/// future body is dropped too and the guard still fires — so a half-started,
/// never-resolved entry is also cleared rather than leaking. Either way the
/// map only ever holds genuinely in-flight rotations, and a later rotation for the
/// same anchor re-rotates instead of being handed a stale resolved wrapper
/// forever.
#[derive(Debug)]
pub struct EntryGuard {
    anchor_id: Uuid,
}

impl EntryGuard {
    /// Create a guard that will remove `anchor_id` from the per-thread
    /// single-flight map on drop.
    #[must_use]
    pub fn new(anchor_id: Uuid) -> Self {
        Self { anchor_id }
    }
}

impl Drop for EntryGuard {
    fn drop(&mut self) {
        with_single_flight(|sf| sf.remove(self.anchor_id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchor_cookie_prod_is_host_strict_httponly_secure() {
        let id = Uuid::new_v4();
        let c = set_anchor_cookie(&id, false);
        assert!(c.starts_with("__Host-zs_app_anchor="), "{c}");
        assert!(c.contains(&id.to_string()));
        assert!(c.contains("Path=/"));
        assert!(c.contains("HttpOnly"));
        // The anchor is Strict (NOT Lax) — §8.3 round-2.
        assert!(c.contains("SameSite=Strict"), "{c}");
        assert!(!c.contains("SameSite=Lax"), "{c}");
        assert!(c.contains("Secure"));
        assert!(c.contains("Max-Age=2592000"), "30d: {c}"); // 30 * 24 * 3600
    }

    #[test]
    fn anchor_cookie_dev_drops_secure_and_host_prefix() {
        let id = Uuid::new_v4();
        let c = set_anchor_cookie(&id, true);
        assert!(!c.starts_with("__Host-"), "dev must drop __Host-: {c}");
        assert!(c.starts_with("zs_app_anchor="), "{c}");
        assert!(!c.contains("Secure"), "{c}");
        assert!(c.contains("HttpOnly"));
        assert!(c.contains("SameSite=Strict"));
    }

    #[test]
    fn anchor_cookie_clear_zeroes_max_age() {
        let c = clear_anchor_cookie(false);
        assert!(c.contains("Max-Age=0"));
        assert!(c.contains("Secure"));
        let dev = clear_anchor_cookie(true);
        assert!(!dev.contains("Secure"));
        assert!(!dev.starts_with("__Host-"));
    }

    #[test]
    fn anchor_cookie_parse_roundtrips() {
        let id = Uuid::new_v4();
        let header = format!("foo=bar; __Host-zs_app_anchor={id}; baz=qux");
        assert_eq!(parse_anchor_cookie(&header, false), Some(id));
        assert_eq!(parse_anchor_cookie("nothing", false), None);
        assert_eq!(
            parse_anchor_cookie("__Host-zs_app_anchor=not-a-uuid", false),
            None
        );
        let dev = format!("zs_app_anchor={id}");
        assert_eq!(parse_anchor_cookie(&dev, true), Some(id));
        // Prod-named cookie does not match in dev mode.
        assert_eq!(parse_anchor_cookie(&header, true), None);
        // CRITICAL (MAJOR fix): the interactive OIDC cookie name must NOT be
        // parsed as an anchor — distinct stores, distinct names.
        let interactive = format!("__Host-zs_app_session={id}");
        assert_eq!(
            parse_anchor_cookie(&interactive, false),
            None,
            "the interactive __Host-zs_app_session must NOT resolve as an anchor"
        );
    }

    #[test]
    fn breadcrumb_is_non_httponly_lax_host_keyed() {
        let c = set_breadcrumb_cookie("myapp.zeroship.ai", false);
        assert!(c.starts_with("zs.myapp.zeroship.ai.is.authenticated=true"), "{c}");
        // Breadcrumb is readable by JS — NOT HttpOnly.
        assert!(!c.contains("HttpOnly"), "breadcrumb must be JS-readable: {c}");
        assert!(c.contains("SameSite=Lax"), "{c}");
        assert!(c.contains("Secure"));
        assert!(c.contains("Max-Age=2592000"), "matches anchor 30d: {c}");

        let dev = set_breadcrumb_cookie("myapp.localhost", true);
        assert!(!dev.contains("Secure"), "{dev}");
    }

    #[test]
    fn breadcrumb_clear_zeroes_max_age() {
        let c = clear_breadcrumb_cookie("h", false);
        assert!(c.contains("Max-Age=0"));
        assert!(!c.contains("HttpOnly"));
    }

    #[test]
    fn single_flight_coalesces_then_clears() {
        use futures::FutureExt as _;
        let sf = RotationSingleFlight::new();
        let id = Uuid::new_v4();
        assert_eq!(sf.in_flight(), 0);
        assert!(sf.get(id).is_none());

        let fut: super::SharedRotationFuture = (Box::pin(async {
            Ok(RotationOk {
                global_user_id: Uuid::new_v4(),
                granted_scopes: vec!["openid".into()],
                email_verified: Some(true),
                name: None,
                avatar_url: None,
                auth_time: None,
                amr: vec![],
            })
        }) as std::pin::Pin<Box<dyn std::future::Future<Output = RotationResult>>>)
            .shared();
        let _leader = sf.insert(id, fut);
        assert_eq!(sf.in_flight(), 1);
        // A follower for the same id gets a clone of the SAME future.
        assert!(sf.get(id).is_some());

        sf.remove(id);
        assert_eq!(sf.in_flight(), 0);
        assert!(sf.get(id).is_none());
    }
}
