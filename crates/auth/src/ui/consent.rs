//! `/consent` GET plus consent decision POST handlers.

use askama::Template;
use ntex::http::header::{COOKIE, SET_COOKIE};
use ntex::web::{HttpRequest, HttpResponse};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;
use zeroship_authz::{self as authz, AuthzContext, Resource, Scope};
use zeroship_core::typed_id::app_id_from_oauth_client_id;

use crate::advisory_lock::{oauth_grant_lock_key, with_advisory_lock};
use crate::audit::{self, AuditEvent};
use crate::config::AuthConfig;
use crate::csrf;
use crate::error::AuthError;
use crate::oidc::auth_request::AuthRequest;
use crate::oidc::authorization_code::{
    authorization_error_redirect_location, persist_consent_grant,
    return_to_after_prompt_interaction,
};
use crate::oidc::Issuer;
use crate::return_to;
use crate::sessions::login as session_cookie;
use crate::store::sessions as session_store;
use crate::ui::{ConsentPage, ConsentScopeView, PublicErrorMessage};

const CANNOT_GRANT: &str = "you cannot grant this permission";

/// Reserved OIDC identity scopes — namespace (b), platform-defined, always
/// self-grantable by the authenticated end user. Sharing one's own
/// name/email/identity with an app needs no platform privilege.
const IDENTITY_SCOPES: &[&str] = &["openid", "profile", "email", "offline_access"];

/// Reserved future namespace prefixes that route into the namespace-(a)
/// platform/delegated authorization gate alongside the closed `Scope::parse`
/// vocabulary (spec §5.1).
const RESERVED_DELEGATED_PREFIXES: &[&str] = &["platform:", "org:"];

#[derive(Debug, Deserialize)]
pub struct ConsentQuery {
    pub return_to: Option<String>,
}

// ntex's per-thread service futures are intentionally `!Send`.
#[allow(clippy::future_not_send)]
pub async fn get_consent(
    req: HttpRequest,
    query: ntex::web::types::Query<ConsentQuery>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
    db: ntex::web::types::State<Arc<compio_postgres::Client>>,
    issuer: ntex::web::types::State<Arc<Issuer>>,
) -> HttpResponse {
    let query = query.into_inner();
    get_consent_native(
        req,
        query.return_to.as_deref(),
        cfg.as_ref(),
        db.as_ref(),
        issuer.as_ref(),
    )
    .await
}

#[allow(clippy::future_not_send)]
async fn get_consent_native(
    req: HttpRequest,
    raw_return_to: Option<&str>,
    cfg: &AuthConfig,
    db: &compio_postgres::Client,
    issuer: &Issuer,
) -> HttpResponse {
    let return_to = return_to::sanitize(raw_return_to, return_to::SAFE_DEFAULT);
    let session = match resolve_native_session(&req, cfg, db).await {
        Ok(Some(session)) => session,
        Ok(None) => {
            let location = return_to::login_location(&return_to::request_target(&req));
            return return_to::see_other(&location)
                .header("cache-control", "no-store")
                .finish();
        }
        Err(resp) => return resp,
    };

    let ctx = match load_native_consent_context(db, &return_to).await {
        Ok(ctx) => ctx,
        Err(err) => {
            tracing::warn!(error = %err, "native consent request invalid");
            return render_error(PublicErrorMessage::InvalidRequest);
        }
    };
    let info = native_consent_request(&ctx, session.user_id);
    let app_scope_defs = match load_app_scope_defs(db, &ctx.client.client_id).await {
        Ok(defs) => defs,
        Err(e) => {
            tracing::error!(error = %e, client_id = %ctx.client.client_id, "native consent app scope defs lookup failed");
            return render_error(PublicErrorMessage::ContactSupport);
        }
    };
    let classified = match classify_and_authorize(db, &info, &app_scope_defs).await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, client_id = %ctx.client.client_id, "native consent grant authorization failed");
            return render_error(PublicErrorMessage::ContactSupport);
        }
    };
    if classified.has_unknown {
        return oauth_error_redirect(&ctx, "invalid_scope", issuer.issuer());
    }

    render_consent_page(&return_to, &info, db, cfg, classified.can_grant, &app_scope_defs)
    .await
}

// ─── POST /consent/accept and /consent/deny ──────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ConsentDecisionForm {
    pub csrf: Option<String>,
    pub return_to: Option<String>,
    /// HTML form checkbox: `Some("on")` when ticked, `None` when not.
    #[serde(default)]
    pub remember: Option<String>,
}

/// `/consent/accept` POST — validates CSRF, verifies the grantor can delegate
/// every recognized Phase 10 scope, and persists the native OP grant.
#[allow(clippy::future_not_send)]
pub async fn post_consent_accept(
    req: HttpRequest,
    form: ntex::web::types::Form<ConsentDecisionForm>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
    db: ntex::web::types::State<Arc<compio_postgres::Client>>,
    issuer: ntex::web::types::State<Arc<Issuer>>,
) -> HttpResponse {
    let form = form.into_inner();
    if !csrf_valid(&req, &form, cfg.as_ref()) {
        return render_error_forbidden(PublicErrorMessage::InvalidRequest);
    }

    post_consent_accept_native(req, &form, cfg.as_ref(), db.as_ref(), issuer.as_ref()).await
}

#[allow(clippy::future_not_send)]
async fn post_consent_accept_native(
    req: HttpRequest,
    form: &ConsentDecisionForm,
    cfg: &AuthConfig,
    db: &compio_postgres::Client,
    issuer: &Issuer,
) -> HttpResponse {
    let return_to = return_to::sanitize(form.return_to.as_deref(), return_to::SAFE_DEFAULT);
    let session = match resolve_native_session(&req, cfg, db).await {
        Ok(Some(session)) => session,
        Ok(None) => {
            let consent_path = return_to::consent_location(&return_to);
            let location = return_to::login_location(&consent_path);
            return return_to::see_other(&location)
                .header("cache-control", "no-store")
                .finish();
        }
        Err(resp) => return resp,
    };
    let ctx = match load_native_consent_context(db, &return_to).await {
        Ok(ctx) => ctx,
        Err(err) => {
            tracing::warn!(error = %err, "POST /consent/accept native request invalid");
            return render_error(PublicErrorMessage::InvalidRequest);
        }
    };
    let info = native_consent_request(&ctx, session.user_id);
    let app_scope_defs = match load_app_scope_defs(db, &ctx.client.client_id).await {
        Ok(defs) => defs,
        Err(e) => {
            tracing::error!(error = %e, client_id = %ctx.client.client_id, "POST /consent/accept native app scope defs lookup failed");
            return render_error(PublicErrorMessage::ContactSupport);
        }
    };
    match classify_and_authorize(db, &info, &app_scope_defs).await {
        Ok(c) if c.has_unknown => return oauth_error_redirect(&ctx, "invalid_scope", issuer.issuer()),
        Ok(c) if c.can_grant => {}
        Ok(_) => {
            return render_consent_page(&return_to, &info, db, cfg, false, &app_scope_defs)
            .await;
        }
        Err(e) => {
            tracing::error!(error = %e, client_id = %ctx.client.client_id, "POST /consent/accept native authorization failed");
            return render_error(PublicErrorMessage::ContactSupport);
        }
    }

    let requested_scopes = sort_dedup_scopes(&ctx.request.scopes);
    let lock_conn = match open_dedicated_auth_pg(&cfg.db_url).await {
        Ok(conn) => conn,
        Err(e) => {
            tracing::error!(error = %e, client_id = %ctx.client.client_id, "native oauth grant lock connection failed");
            return render_error(PublicErrorMessage::ContactSupport);
        }
    };
    let lock_key = oauth_grant_lock_key(&session.user_id, &ctx.client.client_id);
    let cumulative_scopes = match with_advisory_lock(&lock_conn, lock_key, || async {
        let cumulative = persist_consent_grant(
            &lock_conn,
            session.user_id,
            &ctx.client.client_id,
            &requested_scopes,
        )
        .await
        .map_err(AuthError::Db)?;

        if cumulative.iter().any(|s| s == "email")
            && app_id_from_client_id(&ctx.client.client_id).is_some()
        {
            match crate::store::relay::mint_alias_at_consent(
                &lock_conn,
                &ctx.client.client_id,
                session.user_id,
                &cfg.relay_domain,
            )
            .await
            {
                Ok(Some(alias)) => {
                    tracing::debug!(client_id = %ctx.client.client_id, alias = %alias, "relay alias minted at native consent");
                }
                Ok(None) => {
                    tracing::debug!(client_id = %ctx.client.client_id, "relay alias deferred to gateway lazy-mint (identity row not yet projected)");
                }
                Err(e) => {
                    tracing::warn!(error = %e, client_id = %ctx.client.client_id, "relay alias mint failed at native consent (non-fatal)");
                }
            }
        }

        Ok(cumulative)
    })
    .await
    {
        Ok(scopes) => scopes,
        Err(e) => {
            tracing::error!(error = %e, client_id = %ctx.client.client_id, "POST /consent/accept native locked grant mutation failed");
            return render_error(PublicErrorMessage::ContactSupport);
        }
    };

    audit::emit(
        db,
        &AuditEvent {
            event_type: "consent_accept",
            outcome: "success",
            user_id: Some(&session.user_id),
            client_id: Some(&ctx.client.client_id),
            auth_method: Some("consent"),
            detail: json!({
                "requested_scopes": requested_scopes,
                "granted_scopes": cumulative_scopes,
            }),
            ..AuditEvent::from_request(&req)
        },
    )
    .await;

    let return_to = return_to_after_prompt_interaction(&return_to, &["consent"]);
    return_to::see_other(&return_to)
        .header("cache-control", "no-store")
        .finish()
}

/// `/consent/deny` POST — validates CSRF and returns the native OP error
/// redirect.
#[allow(clippy::future_not_send)]
pub async fn post_consent_deny(
    req: HttpRequest,
    form: ntex::web::types::Form<ConsentDecisionForm>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
    db: ntex::web::types::State<Arc<compio_postgres::Client>>,
    issuer: ntex::web::types::State<Arc<Issuer>>,
) -> HttpResponse {
    let form = form.into_inner();
    if !csrf_valid(&req, &form, cfg.as_ref()) {
        return render_error_forbidden(PublicErrorMessage::InvalidRequest);
    }

    post_consent_deny_native(req, &form, cfg.as_ref(), db.as_ref(), issuer.as_ref()).await
}

#[allow(clippy::future_not_send)]
async fn post_consent_deny_native(
    req: HttpRequest,
    form: &ConsentDecisionForm,
    cfg: &AuthConfig,
    db: &compio_postgres::Client,
    issuer: &Issuer,
) -> HttpResponse {
    let return_to = return_to::sanitize(form.return_to.as_deref(), return_to::SAFE_DEFAULT);
    let ctx = match load_native_consent_context(db, &return_to).await {
        Ok(ctx) => ctx,
        Err(err) => {
            tracing::warn!(error = %err, "POST /consent/deny native request invalid");
            return render_error(PublicErrorMessage::InvalidRequest);
        }
    };
    let subject = resolve_native_session(&req, cfg, db)
        .await
        .ok()
        .flatten()
        .map(|session| session.user_id);

    audit::emit(
        db,
        &AuditEvent {
            event_type: "consent_deny",
            outcome: "success",
            user_id: subject.as_ref(),
            client_id: Some(&ctx.client.client_id),
            auth_method: Some("consent"),
            detail: json!({
                "requested_scopes": sort_dedup_scopes(&ctx.request.scopes),
            }),
            ..AuditEvent::from_request(&req)
        },
    )
    .await;

    oauth_error_redirect(&ctx, "access_denied", issuer.issuer())
}

// ─── helpers ─────────────────────────────────────────────────────────────

async fn open_dedicated_auth_pg(db_url: &str) -> crate::error::Result<compio_postgres::Client> {
    let (client, connection) = compio_postgres::connect(db_url, compio_postgres::NoTls)
        .await
        .map_err(|e| AuthError::Db(format!("consent grant lock connect: {e}")))?;
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            tracing::error!(error = %e, "consent grant lock connection ended");
        }
    })
    .detach();
    Ok(client)
}

#[derive(Clone, Debug)]
struct NativeOAuthClient {
    client_id: String,
    client_name: String,
    redirect_uris: Vec<String>,
    scopes: Vec<String>,
}

#[derive(Clone, Debug)]
struct NativeConsentContext {
    request: AuthRequest,
    client: NativeOAuthClient,
}

#[derive(Clone, Debug)]
struct NativeConsentClient {
    client_id: String,
    client_name: Option<String>,
}

#[derive(Clone, Debug)]
struct NativeConsentRequest {
    subject: String,
    client: NativeConsentClient,
    requested_scope: Vec<String>,
}

async fn load_native_consent_context(
    db: &compio_postgres::Client,
    return_to: &str,
) -> Result<NativeConsentContext, String> {
    let request = AuthRequest::parse_return_to(return_to).map_err(|err| err.to_string())?;
    let client = load_native_oauth_client(db, &request.client_id).await?;
    if !client
        .redirect_uris
        .iter()
        .any(|registered| registered == &request.redirect_uri)
    {
        return Err("redirect_uri is not registered".into());
    }
    if !scopes_are_subset(&request.scopes, &client.scopes) {
        return Err("scope is not allowed for client".into());
    }
    Ok(NativeConsentContext { request, client })
}

async fn load_native_oauth_client(
    db: &compio_postgres::Client,
    client_id: &str,
) -> Result<NativeOAuthClient, String> {
    let rows = db
        .query(
            "SELECT client_id, client_name, redirect_uris, scopes \
             FROM zeroship.oauth_clients \
             WHERE client_id = $1",
            &[&client_id],
        )
        .await
        .map_err(|err| format!("oauth client lookup failed: {err}"))?;
    let Some(row) = rows.first() else {
        return Err("unknown client".into());
    };
    Ok(NativeOAuthClient {
        client_id: row.get("client_id"),
        client_name: row.get("client_name"),
        redirect_uris: row.get("redirect_uris"),
        scopes: sort_dedup_scopes(&row.get::<_, Vec<String>>("scopes")),
    })
}

fn native_consent_request(ctx: &NativeConsentContext, subject: Uuid) -> NativeConsentRequest {
    NativeConsentRequest {
        subject: subject.to_string(),
        client: NativeConsentClient {
            client_id: ctx.client.client_id.clone(),
            client_name: Some(ctx.client.client_name.clone()),
        },
        requested_scope: ctx.request.scopes.clone(),
    }
}

fn oauth_error_redirect(ctx: &NativeConsentContext, error: &str, issuer: &str) -> HttpResponse {
    let url = match authorization_error_redirect_location(
        &ctx.request.redirect_uri,
        error,
        ctx.request.state.as_deref(),
        issuer,
    ) {
        Ok(url) => url,
        Err(err) => {
            tracing::warn!(error = %err, redirect_uri = %ctx.request.redirect_uri, "native consent redirect_uri parse failed after registry validation");
            return render_error(PublicErrorMessage::InvalidRequest);
        }
    };
    return_to::see_other(&url)
        .header("cache-control", "no-store")
        .finish()
}

#[allow(clippy::future_not_send)]
async fn resolve_native_session(
    req: &HttpRequest,
    cfg: &AuthConfig,
    db: &compio_postgres::Client,
) -> Result<Option<session_store::Session>, HttpResponse> {
    let cookie_header = req
        .headers()
        .get(COOKIE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let Some(session_id) = session_cookie::parse_cookie(cookie_header, cfg.insecure_dev) else {
        return Ok(None);
    };
    session_store::validate(db, session_id).await.map_err(|err| {
        tracing::error!(error = %err, "native consent session validation failed");
        render_error(PublicErrorMessage::ContactSupport)
    })
}

fn sort_dedup_scopes(scopes: &[String]) -> Vec<String> {
    let mut sorted = scopes.to_vec();
    sorted.sort();
    sorted.dedup();
    sorted
}

fn scopes_are_subset(requested: &[String], previously_granted: &[String]) -> bool {
    requested
        .iter()
        .all(|scope| previously_granted.binary_search(scope).is_ok())
}

#[allow(clippy::future_not_send)]
async fn render_consent_page(
    return_to: &str,
    info: &NativeConsentRequest,
    db: &compio_postgres::Client,
    cfg: &AuthConfig,
    can_grant: bool,
    app_scope_defs: &HashMap<String, ScopeDef>,
) -> HttpResponse {
    let csrf_token = csrf::generate_token();
    let client = load_client_display(db, info).await;
    let page = ConsentPage {
        return_to,
        csrf: &csrf_token,
        client_id: &info.client.client_id,
        client_name: &client.name,
        client_logo_uri: client.logo_uri.as_deref(),
        scopes: scope_views(&info.requested_scope, app_scope_defs),
        can_grant,
        grant_error: (!can_grant).then_some(CANNOT_GRANT),
    };
    let body = match page.render() {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = %e, "render consent.html failed");
            return render_error(PublicErrorMessage::ContactSupport);
        }
    };

    let mut resp = HttpResponse::Ok();
    resp.content_type("text/html; charset=utf-8");
    resp.header(SET_COOKIE, csrf::set_cookie(&csrf_token, cfg.insecure_dev));
    resp.body(body)
}

// ─── two-namespace scope classifier (spec §5.1 / §5.2) ───────────────────
//
// Each requested scope falls into exactly one of three buckets. The classifier
// is the *single* authority on whether a scope is grantable — it replaces the
// old `filter_map(Scope::parse(raw).ok())` silent-drop, which inverted the
// authorization for app-declared end-user scopes (a normal user could not grant
// an app its own declared `read:billing`) AND silently swallowed genuinely
// unknown scopes instead of rejecting them.

/// The grant namespace a requested scope belongs to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScopeClass {
    /// Namespace (b): reserved OIDC identity scope or an app-declared scope
    /// found in `zeroship.app_scope_defs`. Self-grantable by the authenticated
    /// end user — bypasses `is_authorized_anywhere` entirely.
    SelfGrant,
    /// Namespace (a): the closed `Scope::parse` platform vocabulary or a
    /// reserved `platform:` / `org:` prefix. Requires platform-policy
    /// delegation via `is_authorized_anywhere`.
    Delegated,
    /// Neither identity, nor app-declared, nor platform vocabulary — a
    /// genuinely undefined scope. Rejects the whole consent with
    /// `invalid_scope`.
    Unknown,
}

/// Classify one requested scope against this consent's app-declared scope set.
///
/// The **platform vocabulary wins on collision**: a scope that `Scope::parse`
/// accepts or that uses a reserved `platform:`/`org:` prefix is classified
/// `Delegated` even if it ALSO appears in `app_scope_defs`. The deploy-time
/// validator (`control::app_oauth_client::validate_app_scopes`) already
/// hard-fails any app scope id that collides with the platform vocabulary, so
/// the `app_scope_defs` registry cannot legitimately contain such an id — but
/// this re-check is defense-in-depth: a future second writer to
/// `app_scope_defs` that bypassed validation can never let the classifier
/// self-grant a real platform scope (the platform gate always runs first).
fn classify_scope(scope: &str, app_scope_defs: &HashMap<String, ScopeDef>) -> ScopeClass {
    // Platform vocabulary / reserved prefixes take precedence over a colliding
    // app-declared entry — these always route through the delegation gate.
    if Scope::parse(scope).is_ok()
        || RESERVED_DELEGATED_PREFIXES
            .iter()
            .any(|p| scope.starts_with(p))
    {
        ScopeClass::Delegated
    } else if IDENTITY_SCOPES.contains(&scope) || app_scope_defs.contains_key(scope) {
        ScopeClass::SelfGrant
    } else {
        ScopeClass::Unknown
    }
}

/// Label + description for one app-declared scope, loaded from
/// `zeroship.app_scope_defs`.
#[derive(Clone, Debug)]
struct ScopeDef {
    label: String,
    description: Option<String>,
}

/// Resolve the per-app `client_id` (`oac_<base62-app-id>`) back to its app UUID.
/// Returns `None` for any client that is not a per-app end-user client (the
/// builder/console/admin clients, e.g. `zeroship-builder-…`), which have no
/// `app_scope_defs` and only ever request identity + platform scopes.
///
/// Delegates to the shared `zeroship_core::typed_id` decoder — the exact
/// inverse of control's `client_id_for_app`, so the prefix can never drift
/// between the minter (control) and this decoder (auth).
fn app_id_from_client_id(client_id: &str) -> Option<Uuid> {
    app_id_from_oauth_client_id(client_id)
}

/// Load the app's declared end-user scopes from `zeroship.app_scope_defs`,
/// keyed by `scope_id`. Empty for non-per-app clients (no `oac_` prefix) or an
/// app that declared none. The auth PG client shares the database with the
/// control schema, exactly like the existing `zeroship.oauth_grants` /
/// `zeroship.oauth_clients` reads in this file.
async fn load_app_scope_defs(
    db: &compio_postgres::Client,
    client_id: &str,
) -> Result<HashMap<String, ScopeDef>, String> {
    let Some(app_id) = app_id_from_client_id(client_id) else {
        return Ok(HashMap::new());
    };
    let rows = match db
        .query(
            "SELECT scope_id, label, description \
             FROM zeroship.app_scope_defs \
             WHERE app_id = $1",
            &[&app_id],
        )
        .await
    {
        Ok(rows) => rows,
        // A control schema that hasn't been migrated yet (or a test DB without
        // the table) must not brick consent for identity/platform scopes — the
        // classifier simply sees no app-declared scopes.
        Err(err) if missing_relation_or_column(&err) => return Ok(HashMap::new()),
        Err(err) => return Err(format!("select zeroship.app_scope_defs: {err}")),
    };

    let mut defs = HashMap::with_capacity(rows.len());
    for row in &rows {
        let scope_id: String = row.get("scope_id");
        defs.insert(
            scope_id,
            ScopeDef {
                label: row.get("label"),
                description: row.try_get("description").ok().flatten(),
            },
        );
    }
    Ok(defs)
}

/// Outcome of classifying every requested scope for a consent challenge.
struct ClassifiedScopes {
    /// `true` when no scope is `Unknown` and the authenticated subject may
    /// grant every `Delegated` scope (the `SelfGrant` subset is always OK).
    can_grant: bool,
    /// `true` when any requested scope is `Unknown` — the consent must be
    /// rejected with `invalid_scope` rather than rendered/accepted.
    has_unknown: bool,
}

/// The pure (DB-free) outcome of partitioning a requested-scope list into the
/// two namespaces. Distinguishes the three early-decision cases from the
/// "needs the delegation gate" case so [`classify_and_authorize`] can short-
/// circuit without a DB and unit tests can assert the partition directly.
enum Partition {
    /// At least one `Unknown` scope — reject with `invalid_scope`.
    HasUnknown,
    /// A reserved `platform:`/`org:` scope that is not yet a real `Scope` — not
    /// delegatable by any policy, so the grant is refused (CANNOT_GRANT).
    UngrantableReserved,
    /// Every scope is `SelfGrant`, or the only delegated scopes still need the
    /// `is_authorized_anywhere` gate. `delegated` is the parseable platform
    /// subset to authorize (empty ⇒ all `SelfGrant`, immediately grantable).
    Delegated(Vec<Scope>),
}

/// Partition the requested scopes into the two namespaces — PURE, no I/O. The
/// `SelfGrant` subset (identity + app-declared) needs no policy; only the
/// returned `Delegated` subset is fed to the authz gate. `Unknown` takes
/// precedence over an ungrantable reserved-prefix scope so the `invalid_scope`
/// reject fires rather than a bare CANNOT_GRANT (spec §5.2). We classify EVERY
/// scope before deciding (no early-return) so a later `Unknown` is always
/// observed.
fn partition_scopes(
    requested: &[String],
    app_scope_defs: &HashMap<String, ScopeDef>,
) -> Partition {
    let mut delegated: Vec<Scope> = Vec::new();
    let mut has_unknown = false;
    let mut has_ungrantable_reserved = false;
    for raw in requested {
        match classify_scope(raw, app_scope_defs) {
            ScopeClass::SelfGrant => {}
            ScopeClass::Delegated => {
                // A `Delegated` classification means EITHER `Scope::parse`
                // succeeded OR a reserved `platform:`/`org:` prefix matched.
                // Only the parseable subset can be fed to the authz gate.
                match Scope::parse(raw) {
                    Ok(scope) => delegated.push(scope),
                    Err(_) => has_ungrantable_reserved = true,
                }
            }
            ScopeClass::Unknown => has_unknown = true,
        }
    }

    if has_unknown {
        Partition::HasUnknown
    } else if has_ungrantable_reserved {
        Partition::UngrantableReserved
    } else {
        Partition::Delegated(delegated)
    }
}

/// Classify all requested scopes and run the namespace-(a) delegation gate on
/// the `Delegated` subset only. The namespace-(b) `SelfGrant` subset (identity
/// scopes and app-declared scopes) is always grantable by the authenticated
/// subject and never touches `load_platform_policies`. Applied identically on
/// the GET render path and the POST accept path so a self-grantable app scope
/// reaches native consent issuance, not just the render.
async fn classify_and_authorize(
    db: &compio_postgres::Client,
    info: &NativeConsentRequest,
    app_scope_defs: &HashMap<String, ScopeDef>,
) -> Result<ClassifiedScopes, String> {
    let delegated = match partition_scopes(&info.requested_scope, app_scope_defs) {
        Partition::HasUnknown => {
            return Ok(ClassifiedScopes { can_grant: false, has_unknown: true });
        }
        Partition::UngrantableReserved => {
            return Ok(ClassifiedScopes { can_grant: false, has_unknown: false });
        }
        Partition::Delegated(delegated) => delegated,
    };

    if delegated.is_empty() {
        return Ok(ClassifiedScopes { can_grant: true, has_unknown: false });
    }

    let principal_id = Uuid::parse_str(&info.subject)
        .map_err(|e| format!("consent subject is not a UUID: {e}"))?;
    let policies = authz::load_platform_policies()
        .map_err(|e| format!("load platform policies: {e}"))?;
    let now = now_unix()?;

    for scope in delegated {
        let ctx = AuthzContext {
            principal_id,
            token_id: None,
            token_policy: None,
            action: scope.action(),
            resource: Resource::Any,
            now,
            request_ip: None,
            mfa_verified: false,
            mfa_age_seconds: None,
            request_id: None,
        };
        match authz::is_authorized_anywhere(db, &policies, &ctx).await {
            Ok(true) => {}
            Ok(false) => return Ok(ClassifiedScopes { can_grant: false, has_unknown: false }),
            Err(e) => return Err(format!("authorize {}: {e}", scope.as_str())),
        }
    }

    Ok(ClassifiedScopes { can_grant: true, has_unknown: false })
}

fn now_unix() -> Result<i64, String> {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|err| format!("clock: {err}"))?
            .as_secs(),
    )
    .map_err(|err| format!("clock overflow: {err}"))
}

struct ClientDisplay {
    name: String,
    logo_uri: Option<String>,
}

async fn load_client_display(
    db: &compio_postgres::Client,
    info: &NativeConsentRequest,
) -> ClientDisplay {
    let fallback_name = info
        .client
        .client_name
        .as_deref()
        .unwrap_or(&info.client.client_id);
    let mut display = ClientDisplay {
        name: fallback_name.to_owned(),
        logo_uri: None,
    };

    let rows = match db
        .query(
            "SELECT client_name, logo_uri \
             FROM zeroship.oauth_clients \
             WHERE client_id = $1",
            &[&info.client.client_id],
        )
        .await
    {
        Ok(rows) => rows,
        Err(err) if missing_relation_or_column(&err) => return display,
        Err(err) => {
            tracing::warn!(error = %err, client_id = %info.client.client_id, "consent client metadata lookup failed");
            return display;
        }
    };

    if let Some(row) = rows.first() {
        if let Ok(Some(name)) = row.try_get::<_, Option<String>>("client_name") {
            display.name = name;
        }
        if let Ok(logo_uri) = row.try_get::<_, Option<String>>("logo_uri") {
            display.logo_uri = logo_uri;
        }
    }

    display
}

fn missing_relation_or_column(err: &compio_postgres::Error) -> bool {
    let text = err.to_string();
    text.contains("does not exist")
        || text.contains("undefined_column")
        || text.contains("42P01")
        || text.contains("42703")
}

/// Render per-scope line items. Consults `app_scope_defs` so an app-declared
/// scope (`read:billing`) renders with its declared label and is **recognized**
/// — the classifier is the single authority, so the only scopes rendered
/// `unrecognized` are genuinely `Unknown` (the consent is then rejected with
/// `invalid_scope` by the caller, never accepted). App-declared labels take
/// precedence over the platform `Scope::parse` label: a `read:billing` an app
/// declared is the app's own scope, not the platform vocabulary.
fn scope_views(
    scopes: &[String],
    app_scope_defs: &HashMap<String, ScopeDef>,
) -> Vec<ConsentScopeView> {
    scopes
        .iter()
        .map(|scope| {
            if let Some(def) = app_scope_defs.get(scope) {
                ConsentScopeView {
                    label: def.label.clone(),
                    description: def.description.clone(),
                    unrecognized: false,
                }
            } else if let Some(label) = standard_scope_label(scope) {
                ConsentScopeView {
                    label: label.to_owned(),
                    description: None,
                    unrecognized: false,
                }
            } else if let Ok(parsed) = Scope::parse(scope) {
                ConsentScopeView {
                    label: parsed.human_label().to_owned(),
                    description: None,
                    unrecognized: false,
                }
            } else {
                ConsentScopeView {
                    label: scope.clone(),
                    description: None,
                    unrecognized: true,
                }
            }
        })
        .collect()
}

fn standard_scope_label(scope: &str) -> Option<&'static str> {
    Some(match scope {
        "openid" => "Verify your identity",
        "email" => "See your email address",
        "profile" => "See your name and profile picture",
        "offline_access" => "Stay signed in to this app even when you're not using it",
        _ => return None,
    })
}

fn csrf_valid(
    req: &HttpRequest,
    form: &ConsentDecisionForm,
    cfg: &AuthConfig,
) -> bool {
    let cookie_header = req
        .headers()
        .get(COOKIE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let cookie_token = csrf::parse_cookie(cookie_header, cfg.insecure_dev);
    let Some(form_token) = form.csrf.as_deref() else {
        return false;
    };
    cookie_token
        .as_deref()
        .is_some_and(|cookie| csrf::matches(form_token, cookie))
}

fn render_error(message: PublicErrorMessage) -> HttpResponse {
    use crate::ui::ErrorPage;
    let page = ErrorPage {
        message,
        error_code: message.error_code(),
    };
    let body = page
        .render()
        .unwrap_or_else(|_| format!("<h1>{}</h1>", message.as_str()));
    let mut response = HttpResponse::Ok();
    response.content_type("text/html; charset=utf-8");
    response.body(body)
}

fn render_error_forbidden(message: PublicErrorMessage) -> HttpResponse {
    use crate::ui::ErrorPage;
    let page = ErrorPage {
        message,
        error_code: message.error_code(),
    };
    let body = page
        .render()
        .unwrap_or_else(|_| format!("<h1>{}</h1>", message.as_str()));
    let mut response = HttpResponse::Forbidden();
    response.content_type("text/html; charset=utf-8");
    response.body(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_scope_labels() {
        let mut defs = HashMap::new();
        defs.insert(
            "read:billing".to_owned(),
            ScopeDef {
                label: "View billing".to_owned(),
                description: Some("See invoices and plan.".to_owned()),
            },
        );
        let scopes = scope_views(
            &[
                "openid".to_owned(),
                "apps:deploy".to_owned(),
                "read:billing".to_owned(),
                "custom-scope".to_owned(),
            ],
            &defs,
        );

        // Identity scope — standard label, recognized.
        assert_eq!(scopes[0].label, "Verify your identity");
        assert!(!scopes[0].unrecognized);
        // Platform vocabulary — Scope::human_label, recognized.
        assert_eq!(scopes[1].label, "Deploy code to your apps");
        assert!(!scopes[1].unrecognized);
        // App-declared — declared label + description from app_scope_defs,
        // RECOGNIZED (the round-3 reconciliation: app scopes are no longer
        // rendered unrecognized).
        assert_eq!(scopes[2].label, "View billing");
        assert_eq!(scopes[2].description.as_deref(), Some("See invoices and plan."));
        assert!(!scopes[2].unrecognized);
        // Genuinely unknown — rendered unrecognized (and the consent gate
        // rejects it with invalid_scope on the live path).
        assert_eq!(scopes[3].label, "custom-scope");
        assert!(scopes[3].unrecognized);
    }

    #[test]
    fn classifies_identity_app_platform_and_unknown() {
        let mut defs = HashMap::new();
        defs.insert(
            "read:billing".to_owned(),
            ScopeDef { label: "View billing".to_owned(), description: None },
        );

        // (b) identity — self-grantable.
        assert_eq!(classify_scope("openid", &defs), ScopeClass::SelfGrant);
        assert_eq!(classify_scope("offline_access", &defs), ScopeClass::SelfGrant);
        // (b) app-declared — self-grantable, even though it is not in the
        // platform vocabulary.
        assert_eq!(classify_scope("read:billing", &defs), ScopeClass::SelfGrant);
        // (a) platform vocabulary — delegated.
        assert_eq!(classify_scope("apps:deploy", &defs), ScopeClass::Delegated);
        assert_eq!(classify_scope("billing:read", &defs), ScopeClass::Delegated);
        // (a) reserved prefixes — delegated.
        assert_eq!(classify_scope("platform:admin", &defs), ScopeClass::Delegated);
        assert_eq!(classify_scope("org:manage", &defs), ScopeClass::Delegated);
        // (d) unknown — neither identity, app-declared, nor platform vocab.
        assert_eq!(classify_scope("write:projects", &defs), ScopeClass::Unknown);
        assert_eq!(classify_scope("custom-scope", &defs), ScopeClass::Unknown);
    }

    /// Defense-in-depth: even if a colliding platform-vocabulary id sneaks into
    /// `app_scope_defs` (a future second writer bypassing the deploy-time
    /// validator), the platform vocabulary WINS — the scope classifies
    /// `Delegated` and must run through the policy gate, never self-grantable.
    #[test]
    fn platform_vocab_wins_over_colliding_app_scope_def() {
        let mut defs = HashMap::new();
        // A planted registry row that collides with the closed platform vocab.
        defs.insert(
            "billing:read".to_owned(),
            ScopeDef { label: "evil".to_owned(), description: None },
        );
        // The classifier must NOT self-grant it — platform vocabulary first.
        assert_eq!(classify_scope("billing:read", &defs), ScopeClass::Delegated);
        // A planted reserved-prefix row is likewise forced through delegation.
        defs.insert(
            "platform:admin".to_owned(),
            ScopeDef { label: "evil".to_owned(), description: None },
        );
        assert_eq!(classify_scope("platform:admin", &defs), ScopeClass::Delegated);
    }

    /// The DB-free core of the authorization-inversion fix: a request made up
    /// ONLY of `SelfGrant` scopes (identity + app-declared) partitions to an
    /// EMPTY `Delegated` set — `classify_and_authorize` short-circuits to
    /// `can_grant = true` WITHOUT loading any platform policy, so a normal end
    /// user with an empty policy set self-grants. This gates the inversion fix
    /// without a live DB (the integration test gates the full HTTP path).
    #[test]
    fn self_grant_only_partitions_to_empty_delegated() {
        let mut defs = HashMap::new();
        defs.insert(
            "read:billing".to_owned(),
            ScopeDef { label: "View billing".to_owned(), description: None },
        );
        let requested = vec!["openid".to_owned(), "read:billing".to_owned()];
        match partition_scopes(&requested, &defs) {
            Partition::Delegated(delegated) => assert!(
                delegated.is_empty(),
                "self-grant-only must yield no delegated scopes (no policy needed)"
            ),
            Partition::HasUnknown => panic!("self-grant set must not be Unknown"),
            Partition::UngrantableReserved => panic!("self-grant set is not reserved"),
        }
    }

    /// A platform scope still partitions into the `Delegated` set (the policy
    /// gate must run) even when it rides alongside a self-grantable app scope.
    #[test]
    fn platform_scope_partitions_into_delegated() {
        let mut defs = HashMap::new();
        defs.insert(
            "read:billing".to_owned(),
            ScopeDef { label: "View billing".to_owned(), description: None },
        );
        let requested = vec!["read:billing".to_owned(), "apps:deploy".to_owned()];
        match partition_scopes(&requested, &defs) {
            Partition::Delegated(delegated) => {
                assert_eq!(delegated.len(), 1, "only the platform scope is delegated");
                assert_eq!(delegated[0].as_str(), "apps:deploy");
            }
            other => panic!("expected Delegated, got {}", match other {
                Partition::HasUnknown => "HasUnknown",
                Partition::UngrantableReserved => "UngrantableReserved",
                Partition::Delegated(_) => unreachable!(),
            }),
        }
    }

    /// Unknown takes precedence over an ungrantable reserved-prefix scope: a
    /// request like `["platform:foo", "genuinely-unknown"]` (where
    /// `platform:foo` does not `Scope::parse`) partitions to `HasUnknown` so the
    /// handler rejects with `invalid_scope`, not a bare CANNOT_GRANT (spec §5.2).
    #[test]
    fn unknown_wins_over_ungrantable_reserved() {
        let defs = HashMap::new();
        // `platform:foo` matches a reserved prefix but does NOT Scope::parse;
        // `genuinely-unknown` is neither identity/app/platform — Unknown.
        let requested = vec!["platform:foo".to_owned(), "genuinely-unknown".to_owned()];
        assert!(
            matches!(partition_scopes(&requested, &defs), Partition::HasUnknown),
            "Unknown must win so the consent rejects with invalid_scope"
        );
        // With no Unknown alongside it, the reserved-prefix scope alone blocks
        // the grant (CANNOT_GRANT), without ever loading a policy.
        let reserved_only = vec!["platform:foo".to_owned()];
        assert!(matches!(
            partition_scopes(&reserved_only, &defs),
            Partition::UngrantableReserved
        ));
    }

    #[test]
    fn app_id_round_trips_through_oac_client_id() {
        let app = Uuid::new_v4();
        // Mint via the shared core helper (the SAME path control uses) and decode
        // via the consent classifier — they must round-trip, pinning the no-drift
        // contract across the control (minter) / auth (decoder) crate boundary.
        let client_id = zeroship_core::typed_id::app_oauth_client_id(&app);
        assert!(client_id.starts_with("oac_"), "got {client_id}");
        assert_eq!(app_id_from_client_id(&client_id), Some(app));
        // Non-per-app clients (builder/console) resolve to None.
        assert_eq!(app_id_from_client_id("zeroship-builder-abc"), None);
        assert_eq!(app_id_from_client_id("oac_not-base62"), None);
    }
}
