use std::sync::Arc;

use ntex::util::Bytes;
use ntex::web::{self, HttpRequest, HttpResponse};
use uuid::Uuid;

use crate::{auth, enforce, proxy, GateState};

pub async fn handle(
    req: HttpRequest,
    state: web::types::State<Arc<GateState>>,
    path: web::types::Path<(String, String)>,
    body: Bytes,
) -> HttpResponse {
    let (app_name, _tail) = path.into_inner();
    let wall_start = std::time::Instant::now();

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

    // 4. Concurrency guard (RAII — released on drop)
    let _guard = match enforce::acquire_concurrency(&state.concurrency, &app_id) {
        Ok(guard) => guard,
        Err(resp) => return resp,
    };

    // 5. Proxy to worker via CHWBL hash ring
    let request_id = Uuid::new_v4();
    let mut response = match proxy::forward(
        &state.hash_ring,
        &app_id,
        &route.plan_id,
        &request_id,
        &body,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            return HttpResponse::BadGateway()
                .json(&serde_json::json!({"error": format!("worker error: {e}")}));
        }
    };

    // 6. Add response headers
    let wall_ms = wall_start.elapsed().as_secs_f64() * 1000.0;
    response.headers_mut().insert(
        ntex::http::header::HeaderName::from_static("x-wall-time-ms"),
        ntex::http::header::HeaderValue::from_str(&format!("{wall_ms:.2}")).unwrap(),
    );
    response.headers_mut().insert(
        ntex::http::header::HeaderName::from_static("x-request-id"),
        ntex::http::header::HeaderValue::from_str(&request_id.to_string()).unwrap(),
    );

    response
}
