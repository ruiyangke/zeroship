use axum::body::Body;
use axum::extract::State;
use axum::http::{header, Method, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tower_http::cors::{Any, CorsLayer};

use crate::cpu_timer::CpuLimits;
use crate::v8::{create_v8_runtime, handle_rpc};

/// Message sent from HTTP handlers to the V8 thread.
struct RpcRequest {
    body: String,
    reply: oneshot::Sender<Result<RpcReply, String>>,
}

struct RpcReply {
    json: String,
    cpu_ms: f64,
    total_cpu_ms: f64,
    request_count: u64,
}

/// Shared state for axum handlers (Send + Sync).
#[derive(Clone)]
struct AppState {
    rpc_tx: mpsc::Sender<RpcRequest>,
    static_html: Arc<Option<Vec<u8>>>,
}

pub async fn serve(
    script_path: &str,
    db_path: &str,
    port: u16,
    static_file: Option<&str>,
) -> Result<(), deno_error::JsErrorBox> {
    fn err(e: impl std::fmt::Display) -> deno_error::JsErrorBox {
        deno_error::JsErrorBox::generic(e.to_string())
    }

    let static_html: Option<Vec<u8>> = static_file.map(|path| {
        std::fs::read(path).unwrap_or_else(|e| {
            eprintln!("[appbase-rt] Warning: could not read {path}: {e}");
            b"<html><body>appbase</body></html>".to_vec()
        })
    });

    let script_path = script_path.to_string();
    let db_path = db_path.to_string();

    // Channel for HTTP → V8 communication
    let (rpc_tx, rpc_rx) = mpsc::channel::<RpcRequest>(256);

    // Spawn V8 on a dedicated thread (JsRuntime is !Send, needs its own thread)
    let v8_handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(v8_worker(&script_path, &db_path, rpc_rx))
    });

    let state = AppState {
        rpc_tx,
        static_html: Arc::new(static_html),
    };

    let cors = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers(Any)
        .allow_origin(Any);

    let app = Router::new()
        .route("/rpc", post(handle_rpc_endpoint))
        .fallback(get(handle_static))
        .layer(cors)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .map_err(err)?;
    eprintln!("[appbase-rt] http://localhost:{port}");

    axum::serve(listener, app).await.map_err(err)?;

    let _ = v8_handle.join();
    Ok(())
}

/// V8 worker loop — runs on its own thread with a single-threaded tokio runtime.
async fn v8_worker(
    script_path: &str,
    db_path: &str,
    mut rpc_rx: mpsc::Receiver<RpcRequest>,
) {
    let (mut runtime, rpc_result) = match create_v8_runtime(db_path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[appbase-rt] Failed to create V8 runtime: {e}");
            return;
        }
    };

    let user_code = match std::fs::read_to_string(script_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[appbase-rt] Failed to read {script_path}: {e}");
            return;
        }
    };

    if let Err(e) = runtime.execute_script("<user>", user_code) {
        eprintln!("[appbase-rt] Failed to execute script: {e}");
        return;
    }
    if let Err(e) = runtime.run_event_loop(Default::default()).await {
        eprintln!("[appbase-rt] Event loop error: {e}");
        return;
    }

    eprintln!("[appbase-rt] Loaded: {script_path}");

    let cpu_limits = CpuLimits::default();
    let mut cpu_usage = crate::cpu_timer::CpuUsage::default();

    while let Some(req) = rpc_rx.recv().await {
        let result = handle_rpc(&mut runtime, &rpc_result, &req.body, &cpu_limits).await;

        let reply = match result {
            Ok(rpc_resp) => {
                cpu_usage.record(rpc_resp.cpu_time);
                Ok(RpcReply {
                    json: rpc_resp.json,
                    cpu_ms: rpc_resp.cpu_time.as_secs_f64() * 1000.0,
                    total_cpu_ms: cpu_usage.total.as_secs_f64() * 1000.0,
                    request_count: cpu_usage.request_count,
                })
            }
            Err(e) => Err(e.to_string()),
        };

        let _ = req.reply.send(reply);
    }
}

async fn handle_rpc_endpoint(
    State(state): State<AppState>,
    body: String,
) -> Response {
    let (reply_tx, reply_rx) = oneshot::channel();

    if state
        .rpc_tx
        .send(RpcRequest {
            body,
            reply: reply_tx,
        })
        .await
        .is_err()
    {
        return json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            r#"{"jsonrpc":"2.0","error":{"code":-32603,"message":"V8 worker unavailable"},"id":null}"#,
        );
    }

    match reply_rx.await {
        Ok(Ok(reply)) => {
            eprintln!(
                "[appbase-rt] POST /rpc cpu={:.2}ms total={:.2}ms reqs={}",
                reply.cpu_ms, reply.total_cpu_ms, reply.request_count
            );
            json_response(StatusCode::OK, &reply.json)
        }
        Ok(Err(e)) => {
            let safe = sanitize_error(&e);
            let err_body = format!(
                r#"{{"jsonrpc":"2.0","error":{{"code":-32603,"message":"{safe}"}},"id":null}}"#
            );
            json_response(StatusCode::INTERNAL_SERVER_ERROR, &err_body)
        }
        Err(_) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"jsonrpc":"2.0","error":{"code":-32603,"message":"V8 worker crashed"},"id":null}"#,
        ),
    }
}

async fn handle_static(State(state): State<AppState>) -> Response {
    if let Some(ref html) = *state.static_html {
        Html(html.clone()).into_response()
    } else {
        json_response(StatusCode::OK, r#"{"status":"appbase-rt running"}"#)
    }
}

fn json_response(status: StatusCode, body: &str) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn sanitize_error(msg: &str) -> String {
    let sanitized = msg.lines().next().unwrap_or("Internal error");
    if sanitized.contains('/') {
        sanitized.split('/').last().unwrap_or(sanitized).to_string()
    } else {
        sanitized.to_string()
    }
}
