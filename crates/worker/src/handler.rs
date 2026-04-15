use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use ntex::web::{self, HttpRequest, HttpResponse};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use zeroship_runtime::runtime::DispatchOutcome;

use crate::{cache, WorkerConfig};

pub async fn dispatch(
    req: HttpRequest,
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

    // Decode authenticated user from ZeroShip-User header (base64 JSON from gateway)
    let user_json = req
        .headers()
        .get("zeroship-user")
        .and_then(|v| v.to_str().ok())
        .and_then(|b64| B64.decode(b64).ok())
        .and_then(|bytes| String::from_utf8(bytes).ok());

    // Set auth user in thread-local before dispatch, clear after
    zeroship_runtime::auth::set_auth_user(user_json);

    // Phase 1: Enter isolate, start dispatch (may return sync or async)
    let outcome = {
        let mut rt = runtime.borrow_mut();
        rt.enter_isolate();
        let o = rt.dispatch_start(&body);
        rt.exit_isolate();
        o
    };

    // Phase 2: Handle the outcome
    let response = match outcome {
        DispatchOutcome::Complete(Ok(result)) => {
            make_response(&result.json, result.cpu_time.as_secs_f64() * 1000.0)
        }
        DispatchOutcome::Complete(Err(e)) => make_error(&e),

        DispatchOutcome::Pending(rx) => {
            // Async — the pump task will resolve the promise.
            // We yield to compio until the result arrives.
            let wall_limit = runtime.borrow().wall_timeout()
                .unwrap_or(std::time::Duration::from_secs(30));
            let deadline = std::time::Instant::now() + wall_limit;

            loop {
                if let Some(result) = rx.try_recv() {
                    break match result {
                        Ok(r) => make_response(&r.json, r.cpu_time.as_secs_f64() * 1000.0),
                        Err(e) => make_error(&e),
                    };
                }
                if std::time::Instant::now() >= deadline {
                    break make_error("request timed out");
                }
                // Yield to compio — let the pump task run
                yield_now().await;
            }
        }

        // HTTP handler responses (for onRequest exports)
        DispatchOutcome::HttpComplete { status: _, headers: _, body, logs: _ } => {
            make_response(&body, 0.0)
        }
        DispatchOutcome::HttpPending(rx) => {
            let wall_limit = runtime.borrow().wall_timeout()
                .unwrap_or(std::time::Duration::from_secs(30));
            let deadline = std::time::Instant::now() + wall_limit;

            loop {
                if let Some(result) = rx.try_recv() {
                    break match result {
                        Ok(zeroship_runtime::HttpDispatchResult::Complete { body, .. }) => {
                            make_response(&body, 0.0)
                        }
                        Ok(_) => make_error("unsupported HTTP result type"),
                        Err(e) => make_error(&e),
                    };
                }
                if std::time::Instant::now() >= deadline {
                    break make_error("request timed out");
                }
                yield_now().await;
            }
        }

        _ => make_error("unsupported dispatch outcome"),
    };

    // Clear auth user after dispatch (prevent leaking to next request)
    zeroship_runtime::auth::clear_auth_user();

    response
}

fn make_response(json: &str, cpu_ms: f64) -> HttpResponse {
    let mut builder = HttpResponse::Ok();
    builder.content_type("application/json");
    if cpu_ms > 0.0 {
        builder.header("x-cpu-time-ms", format!("{cpu_ms:.2}"));
    }
    builder.body(json.to_string())
}

fn make_error(msg: &str) -> HttpResponse {
    let error = serde_json::json!({
        "jsonrpc": "2.0",
        "error": { "code": -32000, "message": msg },
        "id": null
    });
    HttpResponse::Ok()
        .content_type("application/json")
        .body(serde_json::to_string(&error).unwrap())
}

/// Yield control to the compio event loop.
fn yield_now() -> impl std::future::Future<Output = ()> {
    let mut yielded = false;
    std::future::poll_fn(move |cx| {
        if yielded {
            std::task::Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    })
}

/// Pull bundle from control plane and load into cache (cold start path).
async fn load_on_demand(config: &WorkerConfig, app_id: &Uuid) -> Result<(), String> {
    let bundle_url = format!("{}/internal/bundles/{}", config.control_url, app_id);
    let bytes = crate::sync::http_get_bytes(&bundle_url, &config.control_key).await?;

    if bytes.is_empty() {
        return Err("empty bundle".into());
    }

    let hash = hex::encode(Sha256::digest(&bytes));

    if cache::load_app(*app_id, &bytes) {
        cache::set_hash(*app_id, hash);
        eprintln!("[worker] on-demand loaded {app_id}");
        Ok(())
    } else {
        Err("failed to parse bundle".into())
    }
}
