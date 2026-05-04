//! Admin API handlers — app CRUD, deploy, plan, usage.

use std::path::Path as StdPath;
use std::sync::Arc;

use futures::Stream;
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
            tracing::warn!(method = %req.method(), path = %req.path(), "control: auth rejected");
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

/// Streaming `.zship` ingest. Replaces the legacy raw-bundle path —
/// deploy bundles now arrive as zstd-compressed tar archives carrying
/// `manifest.json` + `blobs/<sha256>` entries. See
/// `docs/reference/zship.md` for the wire format and ingestion
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

    // Hard cut: only `application/x-zship` is accepted. The legacy
    // raw `.appbundle` and `application/javascript` paths are gone.
    let content_type = req
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !is_zship_content_type(content_type) {
        return web::HttpResponse::UnsupportedMediaType().json(&serde_json::json!({
            "error": "unsupported content type",
            "detail": "expected application/x-zship",
        }));
    }

    // Stream the request body to a tmp file under the configured
    // deploy tmp dir. Tmp files live for the duration of the deploy
    // and are removed after ingest (success or error). Path includes
    // a uuid so concurrent deploys don't trample each other. The
    // helper itself enforces `MAX_COMPRESSED_BYTES` while writing —
    // see `stream_body_to_tmp_file`. ntex's `Payload` implements
    // `Stream<Item = Result<Bytes, PayloadError>>` directly, so the
    // generic helper accepts it without an adapter.
    let tmp_path = state
        .deploy_tmp_dir
        .join(format!("zeroship-deploy-{}.zship", uuid::Uuid::new_v4().simple()));

    match stream_body_to_tmp_file(
        &mut body,
        &tmp_path,
        zeroship_bundle::MAX_COMPRESSED_BYTES as u64,
    )
    .await
    {
        Ok(_written) => { /* fall through to mmap + ingest */ }
        Err(StreamToTmpError::TooLarge { cap, observed }) => {
            return web::HttpResponse::PayloadTooLarge().json(&serde_json::json!({
                "error": "deploy too large",
                "cap_bytes": cap,
                "observed_bytes": observed,
            }));
        }
        Err(StreamToTmpError::PayloadError(detail)) => {
            return web::HttpResponse::BadRequest().json(&serde_json::json!({
                "error": "payload error",
                "detail": detail,
            }));
        }
        Err(e) => {
            tracing::error!(error = %e, path = %tmp_path.display(), "deploy: streaming to tmp failed");
            return web::HttpResponse::InternalServerError().json(&serde_json::json!({
                "error": "deploy temp storage unavailable",
            }));
        }
    }

    // mmap + ingest. The std::fs::File::open is sync but cheap (no I/O
    // beyond opening a fd); Mmap::map sets up VM mappings without
    // reading bytes. tar/zstd then page-fault through the slice, which
    // the kernel services from page cache.
    let file = match std::fs::File::open(&tmp_path) {
        Ok(f) => f,
        Err(e) => {
            tracing::error!(error = %e, path = %tmp_path.display(), "deploy: tmp re-open failed");
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
            tracing::error!(error = %e, path = %tmp_path.display(), "deploy: mmap failed");
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
/// `application/x-zship` plus parameterised variants like
/// `application/x-zship; charset=utf-8` (some clients add charset
/// even on binary uploads).
fn is_zship_content_type(value: &str) -> bool {
    let primary = value.split(';').next().unwrap_or("").trim();
    primary.eq_ignore_ascii_case("application/x-zship")
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
                "detail": "expected application/x-zship",
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

// ---------------------------------------------------------------------------
// Streaming helper — used by `deploy()` to land the request body in a
// tmp file before mmap+ingest. Generic over the stream type so the
// helper is unit-testable with `futures::stream::iter`; production
// callers pass in `web::types::Payload` (which is `Stream<Item =
// Result<Bytes, PayloadError>>`).
// ---------------------------------------------------------------------------

/// Errors from `stream_body_to_tmp_file`. Maps cleanly onto HTTP
/// status codes — see `deploy()` for the response shape.
#[derive(Debug)]
pub(crate) enum StreamToTmpError {
    /// Couldn't open the tmp file for writing. Caller should return 500.
    OpenFailed(String),
    /// Body exceeded `max_bytes`. Tmp file has been removed.
    /// Caller should return 413.
    TooLarge { cap: u64, observed: u64 },
    /// Underlying payload error (client disconnected, decoding error,
    /// etc.). Tmp file has been removed. Caller should return 400.
    PayloadError(String),
    /// Disk write or sync failed. Tmp file has been removed (best
    /// effort). Caller should return 500.
    WriteFailed(String),
}

impl std::fmt::Display for StreamToTmpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OpenFailed(s) => write!(f, "tmp file open failed: {s}"),
            Self::TooLarge { cap, observed } => {
                write!(f, "body too large: {observed} > {cap}")
            }
            Self::PayloadError(s) => write!(f, "payload error: {s}"),
            Self::WriteFailed(s) => write!(f, "tmp write failed: {s}"),
        }
    }
}

/// Stream a body to a tmp file, enforcing `max_bytes` while writing.
/// On any error (incl. cap exceeded), the partial tmp file is removed.
/// On success, the file is fsynced and the total byte count returned.
///
/// Generic over the chunk type (`B: AsRef<[u8]>`) so this compiles
/// against both `ntex::util::Bytes` (production: `web::types::Payload`
/// yields ntex's bytes type) and stock `bytes::Bytes` (used by tests
/// constructing `futures::stream::iter`).
pub(crate) async fn stream_body_to_tmp_file<S, B, E>(
    stream: &mut S,
    tmp_path: &StdPath,
    max_bytes: u64,
) -> Result<u64, StreamToTmpError>
where
    S: Stream<Item = Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
    E: std::fmt::Display,
{
    use compio::io::AsyncWriteAtExt;
    use futures::StreamExt;

    let file = compio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(tmp_path)
        .await
        .map_err(|e| StreamToTmpError::OpenFailed(e.to_string()))?;

    let mut written: u64 = 0;
    while let Some(item) = stream.next().await {
        let chunk = match item {
            Ok(c) => c,
            Err(e) => {
                drop(file);
                let _ = compio::fs::remove_file(tmp_path).await;
                return Err(StreamToTmpError::PayloadError(e.to_string()));
            }
        };
        let chunk_slice: &[u8] = chunk.as_ref();
        let chunk_len = chunk_slice.len() as u64;
        let new_total = written + chunk_len;
        if new_total > max_bytes {
            drop(file);
            let _ = compio::fs::remove_file(tmp_path).await;
            return Err(StreamToTmpError::TooLarge {
                cap: max_bytes,
                observed: new_total,
            });
        }
        // compio File::write_all_at takes ownership of the buffer.
        // The chunk is a refcounted slice; copy into an owned Vec so
        // we can hand it to write_all_at. The to_vec() costs a single
        // chunk-sized alloc per chunk (typically 16-256 KiB).
        let owned: Vec<u8> = chunk_slice.to_vec();
        let compio::BufResult(res, _returned) =
            (&file).write_all_at(owned, written).await;
        if let Err(e) = res {
            drop(file);
            let _ = compio::fs::remove_file(tmp_path).await;
            return Err(StreamToTmpError::WriteFailed(format!(
                "write at offset {written}: {e}"
            )));
        }
        written = new_total;
    }
    if let Err(e) = file.sync_all().await {
        drop(file);
        let _ = compio::fs::remove_file(tmp_path).await;
        return Err(StreamToTmpError::WriteFailed(format!("sync_all: {e}")));
    }
    drop(file);
    Ok(written)
}

#[cfg(test)]
mod stream_tmp_tests {
    use super::*;
    use bytes::Bytes;
    use futures::stream;

    fn temp_path(label: &str) -> std::path::PathBuf {
        let unique = uuid::Uuid::new_v4().simple().to_string();
        std::env::temp_dir().join(format!("zs-stream-test-{label}-{unique}"))
    }

    #[compio::test]
    async fn happy_path_writes_concatenated_bytes() {
        let path = temp_path("happy");
        let chunks: Vec<Result<Bytes, &str>> = vec![
            Ok(Bytes::from_static(b"hello, ")),
            Ok(Bytes::from_static(b"streaming ")),
            Ok(Bytes::from_static(b"world!")),
        ];
        let mut s = stream::iter(chunks);
        let written = stream_body_to_tmp_file(&mut s, &path, 1024)
            .await
            .expect("stream ok");
        assert_eq!(written, b"hello, streaming world!".len() as u64);
        let contents = std::fs::read(&path).unwrap();
        assert_eq!(contents, b"hello, streaming world!");
        let _ = std::fs::remove_file(&path);
    }

    #[compio::test]
    async fn cap_exceeded_removes_tmp_file() {
        let path = temp_path("cap");
        let chunks: Vec<Result<Bytes, &str>> = vec![
            Ok(Bytes::from_static(b"AAAAAAAAAA")), // 10 bytes
            Ok(Bytes::from_static(b"BBBBBBBBBB")), // would push to 20, > 15
        ];
        let mut s = stream::iter(chunks);
        let err = stream_body_to_tmp_file(&mut s, &path, 15)
            .await
            .unwrap_err();
        match err {
            StreamToTmpError::TooLarge { cap: 15, observed: 20 } => {}
            other => panic!("expected TooLarge, got {other:?}"),
        }
        assert!(!path.exists(), "tmp file should be removed on cap-exceeded");
    }

    #[compio::test]
    async fn stream_error_removes_tmp_file() {
        let path = temp_path("err");
        let chunks: Vec<Result<Bytes, &str>> = vec![
            Ok(Bytes::from_static(b"some bytes")),
            Err("network blew up"),
        ];
        let mut s = stream::iter(chunks);
        let err = stream_body_to_tmp_file(&mut s, &path, 1024)
            .await
            .unwrap_err();
        match err {
            StreamToTmpError::PayloadError(detail) => {
                assert!(detail.contains("network blew up"), "got {detail}");
            }
            other => panic!("expected PayloadError, got {other:?}"),
        }
        assert!(!path.exists(), "tmp file should be removed on payload error");
    }

    #[compio::test]
    async fn empty_stream_writes_zero_bytes() {
        let path = temp_path("empty");
        let chunks: Vec<Result<Bytes, &str>> = vec![];
        let mut s = stream::iter(chunks);
        let written = stream_body_to_tmp_file(&mut s, &path, 1024)
            .await
            .expect("stream ok");
        assert_eq!(written, 0);
        // Empty file should exist (we created it before the loop).
        assert!(path.exists(), "tmp file should exist even when empty");
        let contents = std::fs::read(&path).unwrap();
        assert!(contents.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[compio::test]
    async fn create_new_fails_when_path_exists() {
        let path = temp_path("collide");
        std::fs::write(&path, b"pre-existing").unwrap();
        let chunks: Vec<Result<Bytes, &str>> = vec![Ok(Bytes::from_static(b"x"))];
        let mut s = stream::iter(chunks);
        let err = stream_body_to_tmp_file(&mut s, &path, 1024)
            .await
            .unwrap_err();
        match err {
            StreamToTmpError::OpenFailed(_) => {}
            other => panic!("expected OpenFailed, got {other:?}"),
        }
        // Pre-existing file must not be overwritten.
        let contents = std::fs::read(&path).unwrap();
        assert_eq!(contents, b"pre-existing");
        let _ = std::fs::remove_file(&path);
    }
}
