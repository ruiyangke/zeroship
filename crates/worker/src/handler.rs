use std::sync::Arc;

use ntex::web::{self, HttpResponse};
use uuid::Uuid;

use crate::{cache, WorkerConfig};

pub async fn dispatch(
    _config: web::types::State<Arc<WorkerConfig>>,
    path: web::types::Path<String>,
    body: String,
) -> HttpResponse {
    let app_id = match path.parse::<Uuid>() {
        Ok(id) => id,
        Err(_) => return HttpResponse::BadRequest().body(r#"{"error":"invalid app_id"}"#),
    };

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
