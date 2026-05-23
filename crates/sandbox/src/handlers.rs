//! HTTP handlers for the sandbox service.
//!
//! Backend-agnostic — every op routes through
//! [`crate::backend::Backend`], so the same handlers serve docker
//! containers and k8s+libkrun Pods.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ntex::http::StatusCode;
use ntex::util::Bytes;
use ntex::web::{self, HttpRequest, HttpResponse};
use serde::Deserialize;
use uuid::Uuid;

use crate::error_envelope::error_response;
use crate::{auth, AppState};

type State = web::types::State<Arc<AppState>>;

// ─── helpers ─────────────────────────────────────────────────────

fn unauthorized() -> HttpResponse {
    error_response(
        StatusCode::UNAUTHORIZED,
        "unauthorized",
        "authentication required",
    )
}

/// Render a §10.0-compliant error envelope.
///
/// `status` is the wire HTTP code; `code` is a snake_case kind (e.g.
/// `"invalid_input"`, `"sandbox_not_found"`); `msg` is the human prose
/// that ends up in the `message` field. Pre-A4 this emitted
/// `{"error":<human prose>}` with no `message`; the new shape is
/// `{"error":<code>,"message":<msg>}` per proposal § 10.0.
fn err(status: u16, code: &'static str, msg: impl Into<String>) -> HttpResponse {
    let s = msg.into();
    // FM-B: every 5xx returned to a client gets a structured
    // operator-visible log line.
    if status >= 500 {
        tracing::error!(status, code, error = %s, "sandbox/handlers");
    }
    let sc = StatusCode::from_u16(status)
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    error_response(sc, code, s)
}

/// Parse a sandbox id from an HTTP path segment into the embedded
/// UUID. : the API returns `sbx_<base62>`
/// from `POST /sandboxes`; every follow-up call (GET / DELETE / exec
/// / files / preview / share) MUST accept that same string back. The
/// pre-fix `s.parse::<Uuid>()` rejected it and 400ed every follow-up
/// — the entire HTTP API was non-functional after create.
///
/// Accepts:
///   - Typed-id form `sbx_<base62>` (the canonical wire shape since
///     the round-1 typed-id rollout) — embedded UUID is decoded via
///     `typed_id::parse_with_prefix(s, "sbx")`.
///   - Bare hyphenated UUID — back-compat for internal/test-only
///     callers (the pg/db layer is moving to typed-id-only; this
///     fallback exists so the round-1 e2e fixtures that inject raw
///     UUIDs into the in-memory registry continue to work).
///
/// Path-traversal posture: `parse_with_prefix` rejects any embedded
/// `/`, `..`, or non-base62 byte BEFORE the value reaches the
/// registry / pg / sealed-record paths. The bare-UUID fallback is
/// equally safe — `Uuid::parse_str` only accepts the canonical
/// hyphenated shape.
fn parse_sandbox_id_to_uuid(s: &str) -> Result<Uuid, HttpResponse> {
    if let Ok(uuid) = zeroship_core::typed_id::parse_with_prefix(s, "sbx") {
        return Ok(uuid);
    }
    s.parse::<Uuid>().map_err(|_| {
        err(
            400,
            "invalid_sandbox_id",
            "invalid sandbox id (expected sbx_<base62> or hyphenated uuid)",
        )
    })
}

/// Validate an HTTP-supplied typed-id at the boundary, asserting
/// the prefix matches `expected_prefix` (e.g. `"usr"`, `"prj"`,
/// `"sbx"`). : every id that flows into
/// pg, k8s label values, and sealed-record paths is now a typed-id
/// (`<prefix>_<base62-uuidv7>`); `parse_with_prefix` is the
/// path-traversal-hardening boundary check (Invariant 2 in the
/// design doc).
///
/// Returns `Ok(())` on success; `Err(())` on any malformed input
/// (callers map to a 400 with a generic message; the parse error is
/// not echoed back to keep boundary noise out of the wire).
fn is_typed_id(id: &str, expected_prefix: &str) -> bool {
    zeroship_core::typed_id::parse_with_prefix(id, expected_prefix).is_ok()
}

/// Render an internal `Uuid` to the canonical wire form
/// `sbx_<base62>`. : every HTTP response
/// + log line that surfaces a sandbox id MUST use this — pre-fix the
/// stop endpoint emitted a hyphenated UUID while create emitted a
/// typed-id, so audit tools that joined on payload-id broke.
pub(crate) fn typed_sandbox_id(id: &Uuid) -> String {
    format!("sbx_{}", zeroship_core::typed_id::uuid_to_base62(id))
}

fn infer_content_type(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("");
    match ext.to_ascii_lowercase().as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" | "ts" | "tsx" | "jsx" => "application/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "md" | "txt" => "text/plain; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        _ => "application/octet-stream",
    }
}

// ─── GET /readyz ──────────────────────────────────────────────────
//
// 200 when the backend is healthy (last probe succeeded), 503
// otherwise. Unauthenticated — kubelet probes don't have the
// bearer token, and the response carries no sensitive info beyond
// "backend reachable / not reachable" which can be inferred from
// 5xx response patterns anyway.

pub async fn readyz(state: State) -> HttpResponse {
    if state.backend.is_healthy() {
        HttpResponse::Ok().json(&serde_json::json!({"status": "ready"}))
    } else {
        HttpResponse::ServiceUnavailable()
            .json(&serde_json::json!({"status": "backend-unhealthy"}))
    }
}

// ─── POST /sandboxes ──────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateSandboxBody {
    /// Identifies the human creator. Drives per-user PVC mounting
    /// in the K8s backend (caches survive across every sandbox the
    /// user opens) and "one active sandbox per user" scheduling.
    /// Constrained to `[a-zA-Z0-9_-]{1,64}`.
    pub user_id: String,
    /// Stable per-project id. The sandbox is keyed on
    /// (`user_id`, `project_id`); re-opening with the same pair
    /// returns the existing sandbox if one is alive. A different
    /// project_id from the same user implies a different sandbox
    /// — the previous one will be stopped (per-user PVC is RWO).
    pub project_id: String,
}

pub async fn create_sandbox(
    req: HttpRequest,
    state: State,
    body: web::types::Json<CreateSandboxBody>,
) -> HttpResponse {
    if !auth::check(&req, &state) { return unauthorized(); }

    let user_id = body.user_id.trim().to_string();
    if !is_typed_id(&user_id, "usr") {
        return err(
            400,
            "invalid_user_id",
            "invalid user_id: must be a typed-id of the form usr_<base62>",
        );
    }

    let project_id = body.project_id.trim().to_string();
    if !is_typed_id(&project_id, "prj") {
        return err(
            400,
            "invalid_project_id",
            "invalid project_id: must be a typed-id of the form prj_<base62>",
        );
    }

    // Re-attach existing sandbox for THIS USER on this project.
    // (Same project_id from a different user = a different
    // sandbox; they each have their own clone of the project.)
    if let Some(id) = state.sandboxes.find_by_user_project(&user_id, &project_id) {
        if let Some(info) = state.sandboxes.get(&id) {
            return HttpResponse::Ok().json(&info);
        }
    }

    // FM-E: retry on stale-tenant or /livez-timeout errors. Each
    // retry mints a fresh sandbox UUID — the NomadCHBackend's
    // create() flow allocates a new vm_index inside, so the next
    // attempt naturally lands on a different IP.
    //
    // Retry contract:
    //   - Backend's contract stays "create returns Result<_, String>";
    //     the recovery decision lives at the handler layer.
    //   - Retry budget is bounded both by attempt-count
    //     (config.create_retry_max) AND total wall-time
    //     (config.create_retry_total_timeout_secs) — a pathological
    //     backend that consumes the full per-attempt timeout
    //     shouldn't tie up an ntex worker indefinitely.
    //   - On exhaustion we return 503 (transient — please retry
    //     later) rather than 500. With FM-F's host-side fence in
    //     place this path should be rare; the retry is one layer of
    //     defense for a truly bad host state.
    let total_budget =
        Duration::from_secs(state.config.create_retry_total_timeout_secs);
    let max_attempts = state.config.create_retry_max.saturating_add(1);
    let outcome = run_create_with_retry(
        max_attempts,
        total_budget,
        || async {
            // : mint a UUIDv7 (typed-id
            // backbone — the same UUIDv7 is what the typed-id wraps)
            // rather than v4. The retry path mints a fresh id per
            // attempt so a stale-tenant retry lands on a different
            // vm_index for nomad-ch (FM-E).
            let sandbox_id = Uuid::now_v7();
            let res = state.backend.create(sandbox_id, &user_id, &project_id).await;
            (sandbox_id, res)
        },
    )
    .await;
    match outcome {
        CreateOutcome::Ok { sandbox_id, mut info } => {
            // : SandboxInfo's `sandbox_id`
            // is the typed-id `sbx_<base62>` form everywhere the wire
            // sees it (registry → handlers → pg → preview-token
            // claims). The backends still take `Uuid` as their
            // internal key (cheap to look up; sealed-record filename
            // is `sha256(uuid_bytes).sealed`); but the public-facing
            // string carries the typed prefix so pg's CHECK passes
            // and `restore::process_pg_row::parse_with_prefix("sbx")`
            // round-trips.
            info.sandbox_id = format!(
                "sbx_{}",
                zeroship_core::typed_id::uuid_to_base62(&sandbox_id)
            );
            let stored = state.sandboxes.insert(sandbox_id, info.clone());
            // Pg is the system of record for non-secret state. Write
            // the sandbox row synchronously after create
            // succeeds; on Err, log + continue (sandbox is live in
            // memory; pg will reconcile on next boot).
            if let Some(db) = state.database.as_ref() {
                // : hold session_auth's result
                // once and destructure both fields. Pre-fix called
                // session_auth twice — wasted RTT and inconsistent
                // failure handling between the two calls.
                let (agent_url, key_fp) = match state.backend.session_auth(sandbox_id).await {
                    Ok(a) => (Some(a.agent_url), a.pubkey_fp),
                    Err(_) => (None, String::new()),
                };
                let vm_index = parse_vm_index_hint(&info.backend_hint);
                if let Err(e) = db
                    .insert_sandbox(
                        &info,
                        db.host_id(),
                        &key_fp,
                        agent_url.as_deref(),
                        vm_index,
                    )
                    .await
                {
                    tracing::warn!(
                        sandbox_id = %sandbox_id,
                        error = %e,
                        "sandbox/handlers: pg insert_sandbox failed (non-fatal)"
                    );
                }
                // Audit event — best-effort.
                let evt_data = serde_json::json!({
                    "backend": info.backend,
                    "vm_index": vm_index,
                    "agent_url": agent_url,
                });
                let event = crate::db::Database::new_event(
                    &info.sandbox_id,
                    &info.user_id,
                    "created",
                    evt_data.to_string(),
                );
                if let Err(e) = db.insert_event(&event).await {
                    tracing::warn!(
                        sandbox_id = %sandbox_id,
                        error = %e,
                        "sandbox/handlers: pg insert_event(created) failed (non-fatal)"
                    );
                }
            }
            HttpResponse::Created().json(&stored)
        }
        CreateOutcome::Failed { status, code, message } => err(status, code, message),
    }
}

/// Parse `vm_index=<int>` out of a `backend_hint` string. The
/// nomad-ch backend embeds vm_index in its hint; the
/// docker / k8s backends don't. Returns `None` for any miss.
fn parse_vm_index_hint(hint: &str) -> Option<i32> {
    hint.split_whitespace()
        .find_map(|tok| tok.strip_prefix("vm_index="))
        .and_then(|s| s.parse::<i32>().ok())
}

/// FM-E: retry result.
pub(crate) enum CreateOutcome {
    Ok {
        sandbox_id: Uuid,
        info: crate::backend::SandboxInfo,
    },
    /// `status` is the HTTP code the handler should emit (500 for
    /// non-retriable, 503 for retry-budget exhaustion); `code` is the
    /// §10.0 machine-readable kind that ends up in the response's
    /// `error` field.
    Failed { status: u16, code: &'static str, message: String },
}

/// FM-E: drives the create + retry loop. Extracted so unit tests
/// can verify the retry contract without spinning up the full ntex
/// app or a real Backend.
///
/// The closure `mint_and_create` is called once per attempt — it
/// must mint a fresh `Uuid` and call `Backend::create` with it.
/// Returning a `(Uuid, Result)` (rather than just a `Result`) lets
/// us record which UUID was tried so the success path can register
/// it in the state map.
pub(crate) async fn run_create_with_retry<F, Fut>(
    max_attempts: u32,
    total_budget: Duration,
    mut mint_and_create: F,
) -> CreateOutcome
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<
            Output = (Uuid, Result<crate::backend::SandboxInfo, String>),
        >,
{
    let started = Instant::now();
    let mut last_err: Option<String> = None;
    for attempt in 1..=max_attempts {
        let elapsed = started.elapsed();
        if elapsed >= total_budget {
            tracing::warn!(
                elapsed_ms = %elapsed.as_millis(),
                budget_ms = %total_budget.as_millis(),
                attempts = attempt - 1,
                "sandbox/handlers create: retry budget exhausted"
            );
            return CreateOutcome::Failed {
                status: 503,
                code: "create_retry_budget_exhausted",
                message: format!(
                    "backend.create: retry budget exhausted after {} \
                     attempt(s); last error: {}",
                    attempt - 1,
                    last_err.unwrap_or_else(|| "<no error captured>".to_string()),
                ),
            };
        }

        let (sandbox_id, res) = mint_and_create().await;
        match res {
            Ok(info) => {
                if attempt > 1 {
                    tracing::info!(
                        attempt,
                        sandbox_id = %sandbox_id,
                        elapsed_ms = %started.elapsed().as_millis(),
                        "sandbox/handlers create: succeeded after retries"
                    );
                }
                return CreateOutcome::Ok { sandbox_id, info };
            }
            Err(e) => {
                let retriable = is_retriable_create_error(&e);
                tracing::warn!(
                    attempt,
                    max_attempts,
                    sandbox_id = %sandbox_id,
                    retriable,
                    error = %e,
                    "sandbox/handlers create: error"
                );
                last_err = Some(e);
                if !retriable {
                    return CreateOutcome::Failed {
                        status: 500,
                        code: "backend_create_failed",
                        message: format!(
                            "backend.create: {}",
                            last_err.unwrap_or_default()
                        ),
                    };
                }
            }
        }
    }
    CreateOutcome::Failed {
        status: 503,
        code: "create_retry_budget_exhausted",
        message: format!(
            "backend.create: {max_attempts} attempts failed; last error: {last}",
            last = last_err.unwrap_or_else(|| "<no error captured>".to_string()),
        ),
    }
}

/// FM-E: classify a `backend.create` error as retriable.
///
/// Retriable cases (caused by a stale previous tenant whose VM is
/// still draining on the same IP):
///   - "stale agent" — FM-A fingerprint check fired
///   - "never returned 200 on /livez" — agent never came up at all
///     (could be a wedged previous tenant or genuinely bad host;
///     either way a fresh vm_index is the cheapest recovery)
///
/// Non-retriable cases (configuration / serialization / pool):
///   - typed-id validation failures (`user_id: expected prefix 'usr'…`)
///   - "concurrent sandbox create" (per-user gate)
///   - "no free vm_index" (pool exhausted)
///   - "nomad-ch backend unhealthy" (probe loop sets the bit)
///
/// We match on substrings rather than typed errors because the
/// Backend trait is `Result<_, String>` — keeping that contract
/// surface narrow lets the recovery policy live entirely in the
/// handler. If retry classification grows past this it should be
/// promoted to a typed error.
fn is_retriable_create_error(msg: &str) -> bool {
    msg.contains("stale agent") || msg.contains("never returned 200 on /livez")
}

// ─── GET /sandboxes ───────────────────────────────────────────────
//
// Cross-tenant scope: the bearer token gates "who can call the
// API," but the API was designed to be called by ONE control
// plane on behalf of MANY end-users. So the per-request scope is
// determined by `?user_id=<id>` — without it we refuse rather
// than dump every user's PVC names + pod names + IDs to whoever
// holds the token. (The previous behavior was a silent cross-
// tenant info disclosure for any token holder.)

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// Required. Filters the response to sandboxes owned by this
    /// user. Returning all sandboxes globally would leak PVC names,
    /// project ids, and pod names of unrelated users.
    pub user_id: Option<String>,
}

pub async fn list_sandboxes(
    req: HttpRequest,
    state: State,
    query: web::types::Query<ListQuery>,
) -> HttpResponse {
    if !auth::check(&req, &state) { return unauthorized(); }
    let Some(user_id) = query.into_inner().user_id else {
        return err(
            400,
            "missing_user_id",
            "list requires ?user_id=<id> — cross-user listing is not exposed",
        );
    };
    if !is_typed_id(&user_id, "usr") {
        return err(400, "invalid_user_id", "invalid user_id");
    }
    let filtered: Vec<_> = state
        .sandboxes
        .list()
        .into_iter()
        .filter(|s| s.user_id == user_id)
        .collect();
    HttpResponse::Ok().json(&filtered)
}

// ─── GET /sandboxes/:id ───────────────────────────────────────────
//
// Same model as the list endpoint — the caller must assert which
// user they're acting on behalf of via `?user_id=`. A wrong
// user_id gets 404 (not 403) so the API doesn't become a
// "does sandbox X exist?" oracle.

#[derive(Debug, Deserialize)]
pub struct GetQuery {
    pub user_id: Option<String>,
}

pub async fn get_sandbox(
    req: HttpRequest,
    state: State,
    path: web::types::Path<String>,
    query: web::types::Query<GetQuery>,
) -> HttpResponse {
    if !auth::check(&req, &state) { return unauthorized(); }
    let id = match parse_sandbox_id_to_uuid(&path) { Ok(u) => u, Err(r) => return r };
    let Some(user_id) = query.into_inner().user_id else {
        return err(400, "missing_user_id", "get requires ?user_id=<id>");
    };
    if !is_typed_id(&user_id, "usr") {
        return err(400, "invalid_user_id", "invalid user_id");
    }
    match state.sandboxes.get(&id) {
        Some(info) if info.user_id == user_id => HttpResponse::Ok().json(&info),
        // 404 for both "no such sandbox" and "wrong owner" — the
        // API must not reveal the difference.
        _ => err(404, "sandbox_not_found", "sandbox not found"),
    }
}

/// Verify the request's `?user_id=<id>` matches the sandbox's
/// owner. Returns the parsed sandbox id on success. On any
/// failure (bad uuid, missing/bad user_id, sandbox not found,
/// owner mismatch) returns 404 — same response regardless, so
/// the API doesn't become an existence oracle.
fn require_owner(
    req: &HttpRequest,
    state: &AppState,
    raw_id: &str,
) -> Result<Uuid, HttpResponse> {
    let id = parse_sandbox_id_to_uuid(raw_id)?;
    // Pull user_id from the query string. Hand-parse to avoid
    // pulling another extractor through every signature; the
    // string is short and the format is fixed.
    let user_id = req
        .uri()
        .query()
        .and_then(|q| {
            q.split('&').find_map(|kv| {
                let (k, v) = kv.split_once('=')?;
                if k == "user_id" {
                    Some(v.to_string())
                } else {
                    None
                }
            })
        })
        .unwrap_or_default();
    if !is_typed_id(&user_id, "usr") {
        return Err(err(404, "sandbox_not_found", "sandbox not found"));
    }
    match state.sandboxes.get(&id) {
        Some(info) if info.user_id == user_id => Ok(id),
        _ => Err(err(404, "sandbox_not_found", "sandbox not found")),
    }
}

// ─── DELETE /sandboxes/:id ────────────────────────────────────────

pub async fn stop_sandbox(
    req: HttpRequest,
    state: State,
    path: web::types::Path<String>,
) -> HttpResponse {
    if !auth::check(&req, &state) { return unauthorized(); }
    let id = match require_owner(&req, &state, &path) { Ok(u) => u, Err(r) => return r };

    let info_for_audit = state.sandboxes.get(&id);
    // Snapshot the in-memory `generation` before any
    // pg or backend write so the CAS-guarded UPDATEs below carry
    // the value this controller still believes it owns. A peer
    // takeover that already happened will have bumped the pg row
    // past this value; our UPDATE returns CasLost and we abandon
    // the destructive ops on this side (§ 11.2).
    let expected_generation = state.sandboxes.generation_for(&id);
    let owner_user_id = info_for_audit.as_ref().map(|i| i.user_id.clone());

    // : pre-flight CAS Stopping fence.
    //
    // Pre-fix, we ran `backend.stop(id)` BEFORE asking pg whether
    // we still own the row. If a peer had just taken over via
    // lease expiration, the agent at the recycled vm_index now
    // belongs to the new owner — and our `backend.stop` would have
    // killed their runtime out from under them. The new owner's
    // probe-and-register-one had succeeded; we just yanked it.
    //
    // The fix is to flip the row to `Stopping` under (host_id,
    // generation) BEFORE touching the backend. CasLost ⇒ a peer
    // owns it; we skip backend.stop AND skip the pg DELETE; the
    // only thing we do is in-memory + sealed-record cleanup, which
    // are local to this controller and don't affect the new owner.
    //
    // When pg is disabled (database=None) we skip the fence — that
    // matches the dev / single-node deploy where there's no peer to
    // race with anyway.
    let mut cas_lost = false;
    let mut new_generation: Option<i64> = None;
    if let (Some(db), Some(gen)) = (state.database.as_ref(), expected_generation) {
        match db
            .update_sandbox_status_with_host(
                id,
                crate::db::SandboxStatus::Stopping,
                gen,
                db.host_id(),
                owner_user_id.as_deref(),
            )
            .await
        {
            Ok(new_gen) => {
                new_generation = Some(new_gen);
                state.sandboxes.set_generation(&id, new_gen);
                tracing::debug!(
                    sandbox_id = %typed_sandbox_id(&id),
                    old_generation = gen,
                    new_generation = new_gen,
                    "sandbox/handlers: pg pre-flight CAS Stopping ok"
                );
            }
            Err(crate::db::DatabaseError::CasLost {
                sandbox_id: sid,
                expected_generation: eg,
                observed_generation: og,
                current_host_id: chi,
            }) => {
                tracing::warn!(
                    sandbox_id = %sid,
                    expected_generation = eg,
                    observed_generation = og,
                    current_host_id = ?chi,
                    "sandbox/handlers: lost-leadership on stop pre-flight; skipping backend.stop AND pg-delete (peer owns it now)"
                );
                crate::metrics::inc_lost_leadership("update_sandbox_status_stopping");
                cas_lost = true;
            }
            Err(crate::db::DatabaseError::NotFound { sandbox_id: sid }) => {
                tracing::info!(
                    sandbox_id = %sid,
                    "sandbox/handlers: stop pre-flight saw row already gone; skipping backend.stop"
                );
                cas_lost = true;
            }
            Err(e) => {
                // Pg failure on pre-flight: log, but treat as if
                // CAS was OK so we don't strand the runtime. This
                // is a deliberate availability-over-consistency
                // choice — a flaky pg shouldn't lock-in stale
                // sandboxes.
                tracing::warn!(
                    sandbox_id = %typed_sandbox_id(&id),
                    error = %e,
                    "sandbox/handlers: pg pre-flight CAS failed; proceeding with backend.stop"
                );
            }
        }
    }

    // CAS-LOST PATH: skip backend.stop and pg-delete entirely.
    // Local cleanup only — registry remove is safe (in-memory,
    // local to this controller). Sealed record was sealed on the
    // ORIGINAL owner's disk; the new owner has its own copy (or
    // not, given the current cross-host limitation). Leaving our
    // local copy alone is the conservative choice; the orphan
    // sweep at next boot would clean it up anyway.
    if cas_lost {
        state.sandboxes.remove(&id);
        return HttpResponse::Ok().json(&serde_json::json!({
            "stopped": true,
            "sandbox_id": typed_sandbox_id(&id),
            "lost_leadership": true,
        }));
    }

    if let Err(e) = state.backend.stop(id).await {
        // **Don't** swallow: surface so the operator sees the
        // failure. We still remove from the registry — leaving a
        // stale entry would never resolve, and the runtime
        // (Pod/container) is the controller's responsibility to
        // chase down via cluster-side cleanup.
        state.sandboxes.remove(&id);
        return err(500, "backend_stop_failed", format!("backend.stop: {e}"));
    }
    state.sandboxes.remove(&id);

    // Best-effort pg writes. Move the row to the
    // tombstone (deleted_sandboxes) and emit a stopped event.
    if let Some(db) = state.database.as_ref() {
        // Final flip from `Stopping` → `Stopped` (records the
        // stopped_at timestamp). We carry the generation that came
        // back from the pre-flight UPDATE; if pre-flight wasn't run
        // (no expected_generation), best-effort skip.
        if let Some(gen) = new_generation {
            match db
                .update_sandbox_status_with_host(
                    id,
                    crate::db::SandboxStatus::Stopped,
                    gen,
                    db.host_id(),
                    owner_user_id.as_deref(),
                )
                .await
            {
                Ok(final_gen) => {
                    state.sandboxes.set_generation(&id, final_gen);
                    tracing::debug!(
                        sandbox_id = %typed_sandbox_id(&id),
                        new_generation = final_gen,
                        "sandbox/handlers: pg update_sandbox_status(stopped) ok"
                    );
                }
                Err(crate::db::DatabaseError::CasLost {
                    sandbox_id: sid,
                    expected_generation: eg,
                    observed_generation: og,
                    current_host_id: chi,
                }) => {
                    // Extremely rare: another controller squeezed in
                    // between our pre-flight Stopping flip and this
                    // final Stopped flip. We've already done the
                    // backend.stop on what we owned at pre-flight
                    // time; the new owner gets to clean up its own
                    // view.
                    tracing::warn!(
                        sandbox_id = %sid,
                        expected_generation = eg,
                        observed_generation = og,
                        current_host_id = ?chi,
                        "sandbox/handlers: lost-leadership between Stopping and Stopped; skipping pg-delete"
                    );
                    crate::metrics::inc_lost_leadership("update_sandbox_status_stopped");
                    cas_lost = true;
                }
                Err(e) => {
                    tracing::warn!(
                        sandbox_id = %typed_sandbox_id(&id),
                        error = %e,
                        "sandbox/handlers: pg update_sandbox_status(stopped) failed (non-fatal)"
                    );
                }
            }
        }

        // + :
        // tombstone DELETE only when we still own the row. The row
        // is `Stopping` at this point (in our view); host_id fence
        // ensures no one else moves it.
        if !cas_lost {
            match db
                .delete_sandbox(
                    id,
                    Some(db.host_id()),
                    owner_user_id.as_deref(),
                )
                .await
            {
                Ok(()) => {}
                Err(crate::db::DatabaseError::NotFound { sandbox_id: sid }) => {
                    // Either the fence rejected (someone else owns it)
                    // or the row is genuinely gone. Either way, no
                    // further action needed.
                    tracing::info!(
                        sandbox_id = %sid,
                        "sandbox/handlers: pg delete_sandbox saw 0 rows (fence or already gone)"
                    );
                    crate::metrics::inc_lost_leadership("delete_sandbox_fence");
                }
                Err(e) => {
                    tracing::warn!(
                        sandbox_id = %typed_sandbox_id(&id),
                        error = %e,
                        "sandbox/handlers: pg delete_sandbox failed (non-fatal)"
                    );
                }
            }
        }

        if let Some(info) = info_for_audit {
            let event = crate::db::Database::new_event(
                &info.sandbox_id,
                &info.user_id,
                "stopped",
                "{}".to_string(),
            );
            if let Err(e) = db.insert_event(&event).await {
                tracing::warn!(
                    sandbox_id = %id,
                    error = %e,
                    "sandbox/handlers: pg insert_event(stopped) failed (non-fatal)"
                );
            }
        }
    }

    // : emit the SAME id form `POST
    // /sandboxes` returned (`sbx_<base62>`). Pre-fix the stop
    // response leaked a hyphenated UUID, so audit tools that joined
    // on payload-id broke at every stop.
    HttpResponse::Ok().json(&serde_json::json!({
        "stopped": true,
        "sandbox_id": typed_sandbox_id(&id),
    }))
}

// ─── POST /sandboxes/:id/exec ─────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ExecBody {
    pub cmd: String,
    pub cwd: Option<String>,
    pub timeout_ms: Option<u64>,
}

pub async fn exec(
    req: HttpRequest,
    state: State,
    path: web::types::Path<String>,
    body: web::types::Json<ExecBody>,
) -> HttpResponse {
    if !auth::check(&req, &state) { return unauthorized(); }
    let id = match require_owner(&req, &state, &path) { Ok(u) => u, Err(r) => return r };

    let timeout_ms = body.timeout_ms.unwrap_or(60_000).min(600_000);
    let cwd = body.cwd.as_deref();

    match state.backend.exec(id, &body.cmd, cwd, Some(timeout_ms)).await {
        Ok(out) => HttpResponse::Ok().json(&serde_json::json!({
            "status": out.status,
            "stdout": out.stdout,
            "stderr": out.stderr,
            "timed_out": out.timed_out,
        })),
        Err(e) => err(500, "backend_exec_failed", format!("backend.exec: {e}")),
    }
}

// ─── GET /sandboxes/:id/file-tree ─────────────────────────────────

pub async fn file_tree(
    req: HttpRequest,
    state: State,
    path: web::types::Path<String>,
) -> HttpResponse {
    if !auth::check(&req, &state) { return unauthorized(); }
    let id = match require_owner(&req, &state, &path) { Ok(u) => u, Err(r) => return r };

    match state.backend.file_tree(id).await {
        Ok(entries) => HttpResponse::Ok().json(&serde_json::json!({"entries": entries})),
        Err(e) => err(500, "backend_file_tree_failed", format!("backend.file_tree: {e}")),
    }
}

// ─── GET /sandboxes/:id/files/{path} ──────────────────────────────

pub async fn read_file(
    req: HttpRequest,
    state: State,
    path: web::types::Path<(String, String)>,
) -> HttpResponse {
    if !auth::check(&req, &state) { return unauthorized(); }
    let (id_s, file_path) = path.into_inner();
    let id = match require_owner(&req, &state, &id_s) { Ok(u) => u, Err(r) => return r };

    match state.backend.read_file(id, &file_path).await {
        Ok(bytes) => HttpResponse::Ok()
            .content_type(infer_content_type(&file_path))
            .body(bytes),
        Err(e) if e.contains("No such file") || e.contains("file not found") || e.starts_with("read") => {
            err(404, "file_not_found", e)
        }
        Err(e) => err(400, "read_file_failed", e),
    }
}

// ─── PUT /sandboxes/:id/files/{path} ──────────────────────────────

pub async fn write_file(
    req: HttpRequest,
    state: State,
    path: web::types::Path<(String, String)>,
    body: Bytes,
) -> HttpResponse {
    if !auth::check(&req, &state) { return unauthorized(); }
    let (id_s, file_path) = path.into_inner();
    let id = match require_owner(&req, &state, &id_s) { Ok(u) => u, Err(r) => return r };

    match state.backend.write_file(id, &file_path, &body).await {
        Ok(()) => HttpResponse::Ok().json(&serde_json::json!({
            "written": file_path,
            "size": body.len(),
        })),
        Err(e) => err(400, "write_file_failed", e),
    }
}

// ─── DELETE /sandboxes/:id/files/{path} ───────────────────────────

pub async fn delete_file(
    req: HttpRequest,
    state: State,
    path: web::types::Path<(String, String)>,
) -> HttpResponse {
    if !auth::check(&req, &state) { return unauthorized(); }
    let (id_s, file_path) = path.into_inner();
    let id = match require_owner(&req, &state, &id_s) { Ok(u) => u, Err(r) => return r };

    match state.backend.delete_file(id, &file_path).await {
        Ok(true) => HttpResponse::Ok().json(&serde_json::json!({"deleted": file_path})),
        Ok(false) => err(404, "file_not_found", "file not found"),
        Err(e) => err(400, "delete_file_failed", e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::SandboxInfo;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn fake_info(id: Uuid) -> SandboxInfo {
        SandboxInfo {
            sandbox_id: id.to_string(),
            user_id: "u".into(),
            project_id: "p".into(),
            backend: "nomad-ch".into(),
            backend_hint: "test".into(),
            created_at_secs: 0,
            last_used_at_secs: 0,
        }
    }

    // ─── : parse_sandbox_id_to_uuid ──

    #[test]
    fn parse_sandbox_id_accepts_typed_id() {
        let uuid = Uuid::now_v7();
        let typed = format!(
            "sbx_{}",
            zeroship_core::typed_id::uuid_to_base62(&uuid)
        );
        let parsed = parse_sandbox_id_to_uuid(&typed)
            .expect("typed-id form must parse");
        assert_eq!(parsed, uuid, "typed-id round-trip must yield the same UUID");
    }

    #[test]
    fn parse_sandbox_id_accepts_hyphenated_uuid_back_compat() {
        let uuid = Uuid::now_v7();
        let s = uuid.to_string();
        let parsed = parse_sandbox_id_to_uuid(&s)
            .expect("hyphenated uuid must parse for back-compat with internal callers");
        assert_eq!(parsed, uuid);
    }

    #[test]
    fn parse_sandbox_id_rejects_wrong_prefix() {
        // Right shape, wrong prefix — must NOT silently fall through
        // to the bare-UUID branch (the embedded base62 is NOT a valid
        // hyphenated UUID, so `s.parse::<Uuid>()` will fail and we
        // return 400, which is the right answer).
        let usr = zeroship_core::typed_id::generate("usr");
        assert!(parse_sandbox_id_to_uuid(&usr).is_err());
    }

    #[test]
    fn parse_sandbox_id_rejects_garbage() {
        for s in ["", "garbage", "sbx_", "sbx_!!!", "alice"] {
            assert!(parse_sandbox_id_to_uuid(s).is_err(), "{s:?} must reject");
        }
    }

    #[test]
    fn typed_sandbox_id_has_sbx_prefix() {
        let uuid = Uuid::now_v7();
        let s = typed_sandbox_id(&uuid);
        assert!(s.starts_with("sbx_"), "got {s:?}");
        // Round-trip via the parser.
        let parsed = parse_sandbox_id_to_uuid(&s).unwrap();
        assert_eq!(parsed, uuid);
    }

    // ─── : typed-id validation ───────

    #[test]
    fn typed_id_validator_accepts_well_formed_typed_id() {
        let usr = zeroship_core::typed_id::generate("usr");
        assert!(is_typed_id(&usr, "usr"));
        let prj = zeroship_core::typed_id::generate("prj");
        assert!(is_typed_id(&prj, "prj"));
    }

    #[test]
    fn typed_id_validator_refuses_human_style_ids() {
        // Pre-CRITICAL-#1 the handler accepted these and the
        // downstream Database::insert_sandbox silently failed because
        // its own parse_with_prefix rejected them. Now we refuse at
        // the handler boundary so a 400 surfaces immediately.
        for id in ["alice", "bob", "myproj", "p", "u-1", ""] {
            assert!(!is_typed_id(id, "usr"), "{id:?} must NOT pass usr typed-id check");
        }
    }

    #[test]
    fn typed_id_validator_refuses_wrong_prefix() {
        // Right shape, wrong prefix — mirrors the path-traversal
        // hardening posture from typed_id::ParseError::WrongPrefix.
        let prj = zeroship_core::typed_id::generate("prj");
        assert!(!is_typed_id(&prj, "usr"));
        let usr = zeroship_core::typed_id::generate("usr");
        assert!(!is_typed_id(&usr, "prj"));
    }

    // ─── FM-E: classifier ───────────────────────────────────────

    #[test]
    fn classifier_retries_stale_agent() {
        assert!(is_retriable_create_error(
            "stale agent at http://10.99.101.2:7777: expected pubkey_fingerprint=aa, got bb"
        ));
        assert!(is_retriable_create_error(
            "stale agent at http://10.99.101.2:7777: /version returned 401"
        ));
    }

    #[test]
    fn classifier_retries_livez_timeout() {
        assert!(is_retriable_create_error(
            "agent at http://10.99.101.2:7777 never returned 200 on /livez (expected fp=aa)"
        ));
    }

    #[test]
    fn classifier_does_not_retry_pool_exhausted() {
        // FM-E: must NOT loop forever on a structural failure (pool
        // empty, validation, concurrent-create gate). 500-immediate
        // is the right answer.
        assert!(!is_retriable_create_error("no free vm_index in pool"));
        assert!(!is_retriable_create_error(
            "concurrent sandbox create in progress for user \"alice\"; retry"
        ));
        assert!(!is_retriable_create_error(
            "nomad-ch backend unhealthy; refusing new sandboxes"
        ));
        assert!(!is_retriable_create_error(
            "user_id: expected prefix 'usr', got 'alice'"
        ));
    }

    // ─── FM-E: retry loop ───────────────────────────────────────

    #[compio::test]
    async fn retry_loop_returns_ok_on_first_attempt() {
        let outcome = run_create_with_retry(3, Duration::from_secs(10), || async {
            let id = Uuid::new_v4();
            (id, Ok(fake_info(id)))
        })
        .await;
        match outcome {
            CreateOutcome::Ok { .. } => {}
            CreateOutcome::Failed { status, code: _, message } => {
                panic!("expected Ok, got {status}: {message}")
            }
        }
    }

    #[compio::test]
    async fn retry_loop_recovers_from_stale_agent_on_second_attempt() {
        // The pilot's exact scenario: cycle-2 first attempt sees a
        // stale-agent fp mismatch on the recycled IP; the retry
        // mints a fresh UUID, the backend's create flow allocates a
        // different vm_index, and the second attempt succeeds.
        let attempts = Rc::new(RefCell::new(0u32));
        let attempts_c = attempts.clone();
        let outcome = run_create_with_retry(3, Duration::from_secs(10), move || {
            let attempts = attempts_c.clone();
            async move {
                let n = {
                    let mut x = attempts.borrow_mut();
                    *x += 1;
                    *x
                };
                let id = Uuid::new_v4();
                if n == 1 {
                    (
                        id,
                        Err(
                            "stale agent at http://10.99.101.2:7777: \
                             expected pubkey_fingerprint=aa, got bb"
                                .to_string(),
                        ),
                    )
                } else {
                    (id, Ok(fake_info(id)))
                }
            }
        })
        .await;
        assert_eq!(*attempts.borrow(), 2, "must retry exactly once");
        assert!(matches!(outcome, CreateOutcome::Ok { .. }));
    }

    #[compio::test]
    async fn retry_loop_returns_503_after_exhausting_retries() {
        let attempts = Rc::new(RefCell::new(0u32));
        let attempts_c = attempts.clone();
        let outcome = run_create_with_retry(3, Duration::from_secs(10), move || {
            let attempts = attempts_c.clone();
            async move {
                *attempts.borrow_mut() += 1;
                let id = Uuid::new_v4();
                (
                    id,
                    Err(
                        "stale agent at http://10.99.101.2:7777: expected fp=aa, got bb"
                            .to_string(),
                    ),
                )
            }
        })
        .await;
        assert_eq!(
            *attempts.borrow(),
            3,
            "must try max_attempts (1 initial + 2 retries) times"
        );
        match outcome {
            CreateOutcome::Failed { status, code: _, message } => {
                assert_eq!(status, 503, "exhausted retries → 503");
                assert!(
                    message.contains("attempts failed") || message.contains("attempt"),
                    "503 body must include attempt count for triage; got {message:?}"
                );
            }
            CreateOutcome::Ok { .. } => panic!("expected failure"),
        }
    }

    #[compio::test]
    async fn retry_loop_does_not_retry_non_retriable_error() {
        let attempts = Rc::new(RefCell::new(0u32));
        let attempts_c = attempts.clone();
        let outcome = run_create_with_retry(3, Duration::from_secs(10), move || {
            let attempts = attempts_c.clone();
            async move {
                *attempts.borrow_mut() += 1;
                let id = Uuid::new_v4();
                (id, Err("no free vm_index in pool".to_string()))
            }
        })
        .await;
        assert_eq!(
            *attempts.borrow(),
            1,
            "non-retriable error must NOT loop"
        );
        match outcome {
            CreateOutcome::Failed { status, .. } => {
                assert_eq!(status, 500, "non-retriable → 500, not 503")
            }
            CreateOutcome::Ok { .. } => panic!("expected failure"),
        }
    }

    #[compio::test]
    async fn retry_loop_uses_fresh_uuid_per_attempt() {
        // FM-E invariant: each attempt mints a fresh UUID. The
        // registry's entry().or_insert_with would refuse a duplicate
        // and the same vm_index would be re-allocated on the same IP.
        let seen: Rc<RefCell<Vec<Uuid>>> = Rc::new(RefCell::new(Vec::new()));
        let seen_c = seen.clone();
        let _outcome = run_create_with_retry(3, Duration::from_secs(10), move || {
            let seen = seen_c.clone();
            async move {
                let id = Uuid::new_v4();
                seen.borrow_mut().push(id);
                (
                    id,
                    Err("stale agent at x: expected fp=a got b".to_string()),
                )
            }
        })
        .await;
        let s = seen.borrow();
        assert_eq!(s.len(), 3);
        assert_ne!(s[0], s[1], "attempt 1 and 2 must use distinct UUIDs");
        assert_ne!(s[1], s[2], "attempt 2 and 3 must use distinct UUIDs");
        assert_ne!(s[0], s[2], "attempt 1 and 3 must use distinct UUIDs");
    }

    #[compio::test]
    async fn retry_loop_bails_when_total_budget_exceeded() {
        // Tight budget + slow attempts → second attempt should hit
        // the wall-time check before invoking the closure.
        let attempts = Rc::new(RefCell::new(0u32));
        let attempts_c = attempts.clone();
        let outcome = run_create_with_retry(5, Duration::from_millis(100), move || {
            let attempts = attempts_c.clone();
            async move {
                *attempts.borrow_mut() += 1;
                // Sleep past the budget so the next iteration hits
                // the elapsed check.
                compio::time::sleep(Duration::from_millis(120)).await;
                let id = Uuid::new_v4();
                (id, Err("stale agent at x: a/b".to_string()))
            }
        })
        .await;
        let n = *attempts.borrow();
        assert!(
            n < 5,
            "budget cap must short-circuit before max_attempts; got {n}"
        );
        match outcome {
            CreateOutcome::Failed { status, code: _, message } => {
                assert_eq!(status, 503);
                assert!(
                    message.contains("budget") || message.contains("attempts failed"),
                    "exhaustion message must hint at the cause; got {message:?}"
                );
            }
            CreateOutcome::Ok { .. } => panic!("expected failure"),
        }
    }

    // ─── A4: §10.0 ErrorEnvelope wire-shape pins ──────────────────
    //
    // One test per group of error sites in this file. Each test
    // synthesises the helper and asserts the response carries BOTH
    // `error` (machine-readable kind) AND `message` (human prose) —
    // pre-A4 the response only had `error` (with human prose
    // inside), violating proposal § 10.0.

    use crate::error_envelope::test_helpers::body_json;

    #[compio::test]
    async fn a4_unauthorized_helper_has_error_and_message() {
        let resp = unauthorized();
        assert_eq!(resp.status().as_u16(), 401);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "unauthorized");
        assert!(body["message"].is_string(), "missing `message` field");
    }

    #[compio::test]
    async fn a4_err_400_emits_code_and_message() {
        let resp = err(400, "invalid_user_id", "invalid user_id");
        assert_eq!(resp.status().as_u16(), 400);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "invalid_user_id");
        assert_eq!(body["message"], "invalid user_id");
    }

    #[compio::test]
    async fn a4_err_404_emits_code_and_message() {
        let resp = err(404, "sandbox_not_found", "sandbox not found");
        assert_eq!(resp.status().as_u16(), 404);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "sandbox_not_found");
        assert_eq!(body["message"], "sandbox not found");
    }

    #[compio::test]
    async fn a4_err_500_emits_code_and_message() {
        let resp = err(500, "backend_exec_failed", "backend.exec: timed out");
        assert_eq!(resp.status().as_u16(), 500);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "backend_exec_failed");
        assert_eq!(body["message"], "backend.exec: timed out");
    }

    #[compio::test]
    async fn a4_err_503_emits_code_and_message() {
        let resp = err(
            503,
            "create_retry_budget_exhausted",
            "backend.create: retry budget exhausted after 3 attempt(s)",
        );
        assert_eq!(resp.status().as_u16(), 503);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "create_retry_budget_exhausted");
        assert!(
            body["message"].as_str().unwrap().contains("budget exhausted"),
            "human prose lives in `message`, not `error`",
        );
    }

    #[compio::test]
    async fn a4_parse_sandbox_id_failure_is_envelope_compliant() {
        let result = parse_sandbox_id_to_uuid("not-a-uuid");
        let resp = result.expect_err("malformed id must error");
        let body = body_json(resp).await;
        assert_eq!(body["error"], "invalid_sandbox_id");
        assert!(body["message"].is_string());
    }
}
