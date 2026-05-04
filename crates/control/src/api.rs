//! Admin API handlers — app CRUD, deploy, plan, usage.

use std::sync::Arc;

use ntex::web;
use ntex::web::types::{Json, Path, State};
use serde::Deserialize;
use uuid::Uuid;

use crate::deploy::{self, IngestError};
use crate::registry::RegistryError;
use crate::AppState;

// ---------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CreateAppBody {
    pub name: String,
    #[serde(default = "default_plan")]
    pub plan_id: String,
}

fn default_plan() -> String {
    "free".to_string()
}

#[derive(Deserialize)]
pub struct SetPlanBody {
    pub plan_id: String,
}

// ---------------------------------------------------------------------------
// Admin auth — require master key on all mutating endpoints
// ---------------------------------------------------------------------------

pub(crate) fn check_admin_auth(req: &web::HttpRequest, state: &AppState) -> Option<web::HttpResponse> {
    // Dev-insecure opt-in: skip auth entirely. Production startup
    // (main.rs) refuses to boot with empty master_key unless this flag
    // is on. No implicit bypass.
    if state.insecure_dev {
        return None;
    }

    // Authenticated dashboard sessions count as admin. Cookie is
    // httpOnly, signed JWT — set by /auth/login or /auth/google/callback.
    if crate::auth_handlers::validate_session(req, state).is_some() {
        return None;
    }

    // Fallback: master-key Bearer header — for tooling (CLI, agents).
    let header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let token = zeroship_core::auth::extract_bearer(header);
    match token {
        Some(key) if zeroship_core::auth::validate_control_key(key, state.master_key.expose_secret()) => None,
        _ => {
            eprintln!("[control] auth rejected on {} {}", req.method(), req.path());
            Some(
                web::HttpResponse::Unauthorized()
                    .json(&serde_json::json!({"error":"unauthorized"})),
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Error → HttpResponse
// ---------------------------------------------------------------------------

fn error_response(e: RegistryError) -> web::HttpResponse {
    match e {
        RegistryError::NotFound(msg) => {
            web::HttpResponse::NotFound().json(&serde_json::json!({ "error": msg }))
        }
        RegistryError::AlreadyExists(msg) => {
            web::HttpResponse::Conflict().json(&serde_json::json!({ "error": msg }))
        }
        RegistryError::InvalidInput(msg) => {
            web::HttpResponse::BadRequest().json(&serde_json::json!({ "error": msg }))
        }
        RegistryError::Database(msg) => web::HttpResponse::InternalServerError()
            .json(&serde_json::json!({ "error": msg })),
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

pub async fn create_app(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    body: Json<CreateAppBody>,
) -> web::HttpResponse {
    if let Some(resp) = check_admin_auth(&req, &state) { return resp; }
    match state.registry.create_app(&body.name, &body.plan_id).await {
        Ok(record) => {
            // Include api_key in the create response (it's skipped from normal serialization)
            let mut json = serde_json::to_value(&record).unwrap();
            json["api_key"] = serde_json::Value::String(record.api_key.clone());
            web::HttpResponse::Created().json(&json)
        }
        Err(e) => error_response(e),
    }
}

pub async fn list_apps(state: State<Arc<AppState>>) -> web::HttpResponse {
    match state.registry.list_apps().await {
        Ok(apps) => web::HttpResponse::Ok().json(&apps),
        Err(e) => error_response(e),
    }
}

pub async fn get_app(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    id: Path<String>,
) -> web::HttpResponse {
    let uid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({"error":"invalid uuid"}))
        }
    };
    // Admin auth surfaces the app's api_key in the response (the same
    // bearer-master-key gate that allows create/delete). Anonymous reads
    // get the public AppRecord without the secret.
    let is_admin = check_admin_auth(&req, &state).is_none();
    match state.registry.get_app(&uid).await {
        Ok(Some(record)) => {
            if is_admin {
                let mut json = serde_json::to_value(&record).unwrap();
                json["api_key"] = serde_json::Value::String(record.api_key.clone());
                web::HttpResponse::Ok().json(&json)
            } else {
                web::HttpResponse::Ok().json(&record)
            }
        }
        Ok(None) => {
            web::HttpResponse::NotFound().json(&serde_json::json!({"error":"app not found"}))
        }
        Err(e) => error_response(e),
    }
}

pub async fn delete_app(req: web::HttpRequest, state: State<Arc<AppState>>, id: Path<String>) -> web::HttpResponse {
    if let Some(resp) = check_admin_auth(&req, &state) { return resp; }
    let uid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({"error":"invalid uuid"}))
        }
    };
    // Delete from VFS first (ignore NotFound — bundle may not exist yet).
    let app_id_str = uid.to_string();
    if let Err(e) = state.vfs.delete(&app_id_str) {
        match e {
            zeroship_bundle::VfsError::NotFound(_) => { /* ok */ }
            other => {
                return web::HttpResponse::InternalServerError()
                    .json(&serde_json::json!({"error": other.to_string()}));
            }
        }
    }
    match state.registry.delete_app(&uid).await {
        Ok(true) => web::HttpResponse::Ok().json(&serde_json::json!({"deleted": true})),
        Ok(false) => {
            web::HttpResponse::NotFound().json(&serde_json::json!({"error":"app not found"}))
        }
        Err(e) => error_response(e),
    }
}

/// Streaming `.zsapp` ingest. Replaces the legacy raw-bundle path —
/// deploy bundles now arrive as zstd-compressed tar archives carrying
/// `manifest.json` + `blobs/<sha256>` entries. See
/// `docs/reference/zsapp.md` for the wire format and ingestion
/// algorithm.
pub async fn deploy(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    id: Path<String>,
    mut body: web::types::Payload,
) -> web::HttpResponse {
    // Auth + uuid + content-type rejections happen BEFORE any body byte
    // is consumed — so an unauthenticated/malformed caller can't tie up
    // tmp file slots without first satisfying these gates.
    if let Some(resp) = check_admin_auth(&req, &state) { return resp; }
    let uid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({"error":"invalid uuid"}))
        }
    };

    // Hard cut: only `application/x-zsapp` is accepted. The legacy
    // raw `.appbundle` and `application/javascript` paths are gone.
    let content_type = req
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !is_zsapp_content_type(content_type) {
        return web::HttpResponse::UnsupportedMediaType().json(&serde_json::json!({
            "error": "unsupported content type",
            "detail": "expected application/x-zsapp",
        }));
    }

    // Stream the request body to a tmp file under the system temp dir.
    // Tmp files live for the duration of the deploy and are removed
    // after ingest (success or error). Path includes a uuid so
    // concurrent deploys don't trample each other.
    let tmp_path = std::env::temp_dir()
        .join(format!("zeroship-deploy-{}.zsapp", uuid::Uuid::new_v4().simple()));

    // Write chunks via compio. Track compressed size; abort if it would
    // exceed `MAX_COMPRESSED_BYTES`. The append happens via repeated
    // `write_all_at(buf, offset).await`.
    let write_result: Result<(), web::HttpResponse> = async {
        use compio::io::AsyncWriteAtExt;

        // create_new fails if a file already exists at that path —
        // defensive against tmp uuid collisions (vanishingly rare).
        let file = match compio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .await
        {
            Ok(f) => f,
            Err(e) => {
                eprintln!("[deploy] tmp create failed for {tmp_path:?}: {e}");
                return Err(web::HttpResponse::InternalServerError()
                    .json(&serde_json::json!({"error":"deploy temp storage unavailable"})));
            }
        };

        let mut written: u64 = 0;
        loop {
            match body.recv().await {
                Some(Ok(chunk)) => {
                    let chunk_len = chunk.len();
                    let new_total = written + chunk_len as u64;
                    if new_total > zeroship_bundle::MAX_COMPRESSED_BYTES as u64 {
                        return Err(web::HttpResponse::PayloadTooLarge().json(
                            &serde_json::json!({
                                "error": "deploy too large",
                                "cap_bytes": zeroship_bundle::MAX_COMPRESSED_BYTES,
                                "observed_bytes": new_total,
                            }),
                        ));
                    }
                    // compio File::write_all_at takes ownership of the buffer.
                    // ntex Bytes is a refcounted slice; copy into an owned Vec
                    // so we can hand it to write_all_at. The to_vec() costs a
                    // single chunk-sized alloc per chunk (typically 16-256 KiB).
                    let owned: Vec<u8> = chunk.to_vec();
                    let compio::BufResult(res, _returned) =
                        (&file).write_all_at(owned, written).await;
                    if let Err(e) = res {
                        eprintln!("[deploy] tmp write failed at offset {written}: {e}");
                        return Err(web::HttpResponse::InternalServerError().json(
                            &serde_json::json!({"error":"deploy temp write failed"}),
                        ));
                    }
                    written = new_total;
                }
                Some(Err(e)) => {
                    eprintln!("[deploy] payload read error: {e}");
                    return Err(web::HttpResponse::BadRequest().json(
                        &serde_json::json!({"error":"payload error","detail":format!("{e}")}),
                    ));
                }
                None => break,
            }
        }
        // fsync so the bytes are durable before we mmap them. For tmp
        // ingest this isn't strictly necessary (we'll unlink soon), but
        // it ensures the read after this sees the writes completely.
        if let Err(e) = file.sync_all().await {
            eprintln!("[deploy] tmp sync_all failed: {e}");
            return Err(web::HttpResponse::InternalServerError()
                .json(&serde_json::json!({"error":"deploy temp sync failed"})));
        }
        Ok(())
    }.await;

    if let Err(resp) = write_result {
        let _ = compio::fs::remove_file(&tmp_path).await;
        return resp;
    }

    // mmap + ingest. The std::fs::File::open is sync but cheap (no I/O
    // beyond opening a fd); Mmap::map sets up VM mappings without
    // reading bytes. tar/zstd then page-fault through the slice, which
    // the kernel services from page cache.
    let file = match std::fs::File::open(&tmp_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[deploy] tmp re-open failed: {e}");
            let _ = compio::fs::remove_file(&tmp_path).await;
            return web::HttpResponse::InternalServerError()
                .json(&serde_json::json!({"error":"deploy temp readback failed"}));
        }
    };
    // SAFETY: tmp file is owned by this handler, written exclusively by
    // us (create_new), fsynced before mapping, and not modified by any
    // other process for the lifetime of `mmap`.
    #[allow(unsafe_code)]
    let mmap = match unsafe { memmap2::Mmap::map(&file) } {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[deploy] mmap failed: {e}");
            drop(file);
            let _ = compio::fs::remove_file(&tmp_path).await;
            return web::HttpResponse::InternalServerError()
                .json(&serde_json::json!({"error":"deploy temp mmap failed"}));
        }
    };

    let result = deploy::ingest(&state.blob_store, &uid, &mmap[..]).await;

    // Drop mmap + file before unlinking. On Linux unlink-while-mapped
    // is fine, but explicit drop avoids edge cases on other platforms.
    drop(mmap);
    drop(file);
    let _ = compio::fs::remove_file(&tmp_path).await;

    match result {
        Ok(success) => {
            // Atomic UPDATE: deploy_hash + manifest_json land together
            // so the gateway never sees half-applied state.
            match state
                .registry
                .set_deploy_with_manifest(&uid, &success.deploy_hash, &success.manifest_json)
                .await
            {
                Ok(true) => web::HttpResponse::Ok().json(&serde_json::json!({
                    "deploy_hash": success.deploy_hash,
                    "blobs_uploaded": success.blobs_uploaded,
                    "blobs_deduped": success.blobs_deduped,
                })),
                Ok(false) => web::HttpResponse::NotFound()
                    .json(&serde_json::json!({"error": "app not found"})),
                Err(e) => error_response(e),
            }
        }
        Err(e) => ingest_error_to_response(e),
    }
}

/// Permissive content-type check. We accept the canonical
/// `application/x-zsapp` plus parameterised variants like
/// `application/x-zsapp; charset=utf-8` (some clients add charset
/// even on binary uploads).
fn is_zsapp_content_type(value: &str) -> bool {
    let primary = value.split(';').next().unwrap_or("").trim();
    primary.eq_ignore_ascii_case("application/x-zsapp")
}

/// Map the structured ingest error to an HTTP response.
fn ingest_error_to_response(e: IngestError) -> web::HttpResponse {
    match e {
        IngestError::BadRequest { error, detail } => web::HttpResponse::BadRequest()
            .json(&serde_json::json!({"error": error, "detail": detail})),
        IngestError::TooLarge { cap_bytes, observed_bytes } => {
            web::HttpResponse::PayloadTooLarge().json(&serde_json::json!({
                "error": "deploy too large",
                "cap_bytes": cap_bytes,
                "observed_bytes": observed_bytes,
            }))
        }
        IngestError::UnsupportedMediaType => web::HttpResponse::UnsupportedMediaType()
            .json(&serde_json::json!({
                "error": "unsupported content type",
                "detail": "expected application/x-zsapp",
            })),
        IngestError::BlobStoreUnavailable(detail) => web::HttpResponse::ServiceUnavailable()
            .json(&serde_json::json!({
                "error": "blob store unavailable",
                "detail": detail,
            })),
        IngestError::Internal(detail) => web::HttpResponse::InternalServerError()
            .json(&serde_json::json!({"error": "internal", "detail": detail})),
    }
}

pub async fn set_plan(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    id: Path<String>,
    body: Json<SetPlanBody>,
) -> web::HttpResponse {
    if let Some(resp) = check_admin_auth(&req, &state) { return resp; }
    let uid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({"error":"invalid uuid"}))
        }
    };
    match state.registry.set_plan(&uid, &body.plan_id).await {
        Ok(true) => web::HttpResponse::Ok().json(&serde_json::json!({"updated": true})),
        Ok(false) => {
            web::HttpResponse::NotFound().json(&serde_json::json!({"error":"app not found"}))
        }
        Err(e) => error_response(e),
    }
}

pub async fn get_usage(state: State<Arc<AppState>>, id: Path<String>) -> web::HttpResponse {
    let uid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({"error":"invalid uuid"}))
        }
    };
    match state.registry.get_usage(&uid).await {
        Ok(usage) => web::HttpResponse::Ok().json(&usage),
        Err(e) => error_response(e),
    }
}

