use std::sync::Arc;

use ntex::web::{self, HttpResponse};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{cache, WorkerConfig};

pub async fn dispatch(
    config: web::types::State<Arc<WorkerConfig>>,
    path: web::types::Path<String>,
    body: String,
) -> HttpResponse {
    let app_id = match path.parse::<Uuid>() {
        Ok(id) => id,
        Err(_) => return HttpResponse::BadRequest().body(r#"{"error":"invalid app_id"}"#),
    };

    // On-demand loading: if app is not cached, pull from control plane
    if cache::get_runtime(&app_id).is_none() {
        if let Err(e) = load_on_demand(&config, &app_id).await {
            return HttpResponse::ServiceUnavailable()
                .json(&serde_json::json!({"error": format!("failed to load app: {e}")}));
        }
    }

    let runtime = match cache::get_runtime(&app_id) {
        Some(rt) => rt,
        None => {
            return HttpResponse::NotFound()
                .body(format!(r#"{{"error":"app {app_id} not loaded"}}"#));
        }
    };

    // Dispatch to V8 on this thread — no channel needed since V8 is thread-local.
    let result = {
        let mut rt = runtime.borrow_mut();
        rt.dispatch_rpc(&body)
    };

    match result {
        Ok(rr) => HttpResponse::Ok()
            .content_type("application/json")
            .header(
                "x-cpu-time-ms",
                format!("{:.2}", rr.cpu_time.as_secs_f64() * 1000.0),
            )
            .body(rr.json),
        Err(e) => {
            let error = serde_json::json!({
                "jsonrpc": "2.0",
                "error": { "code": -32000, "message": e },
                "id": null
            });
            HttpResponse::Ok()
                .content_type("application/json")
                .body(serde_json::to_string(&error).unwrap())
        }
    }
}

/// Pull bundle from control plane and load into cache (cold start path).
async fn load_on_demand(config: &WorkerConfig, app_id: &Uuid) -> Result<(), String> {
    let bundle_url = format!("{}/internal/bundles/{}", config.control_url, app_id);
    let bytes = crate::sync::http_get_bytes(&bundle_url, &config.control_key).await?;

    if bytes.is_empty() {
        return Err("empty bundle".into());
    }

    // Compute hash for cache tracking
    let hash = hex::encode(Sha256::digest(&bytes));

    if cache::load_app(*app_id, &bytes) {
        cache::set_hash(*app_id, hash);
        eprintln!("[worker] on-demand loaded {app_id}");
        Ok(())
    } else {
        Err("failed to parse bundle".into())
    }
}
