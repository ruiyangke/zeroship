use std::sync::Arc;

use ntex::web::{self, HttpRequest, HttpResponse};

use crate::{auth, enforce, proxy, GateState};

pub async fn handle(
    req: HttpRequest,
    state: web::types::State<Arc<GateState>>,
    path: web::types::Path<(String, String)>,
    body: String,
) -> HttpResponse {
    let (app_name, _tail) = path.into_inner();

    // 1. Route resolution
    let (app_id, route) = match state.routes.lookup_by_name(&app_name) {
        Some(r) => r,
        None => {
            return HttpResponse::NotFound()
                .json(&serde_json::json!({"error": format!("app '{app_name}' not found")}));
        }
    };

    // 2. Auth — check X-Api-Key header
    if let Err(resp) = auth::check_api_key(&req, &route) {
        return resp;
    }

    // 3. Rate limit
    if let Err(resp) = enforce::check_rate_limit(&state.rate_limiters, &app_id) {
        return resp;
    }

    // 4. Concurrency guard
    let _guard = match enforce::acquire_concurrency(&state.concurrency, &app_id) {
        Ok(guard) => guard,
        Err(resp) => return resp,
    };

    // 5. Proxy to worker
    match proxy::forward(&state.config.worker_urls, &app_id, &body).await {
        Ok(response) => response,
        Err(e) => HttpResponse::BadGateway()
            .json(&serde_json::json!({"error": format!("worker error: {e}")})),
    }
}
