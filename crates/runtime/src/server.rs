//! HTTP server for the appbase runtime.
//!
//! Uses axum for multi-threaded HTTP handling.
//! V8 isolates run on dedicated threads, one per app (via IsolatePool).
//! Communication between HTTP handlers and V8 is via mpsc channels.

use axum::body::Body;
use axum::http::{header, Method, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tower_http::cors::{Any, CorsLayer};

use crate::plugin::Plugin;
use crate::pool::IsolatePool;

/// An app's compiled artifacts.
#[derive(Clone)]
pub struct AppBundle {
    /// Compiled server JS (loaded into V8).
    pub server_js: String,
    /// Compiled client HTML (served to browser).
    pub client_html: Option<Bytes>,
}

/// Shared state for axum handlers.
#[derive(Clone)]
struct AppState {
    pool: Arc<IsolatePool>,
    /// App bundles keyed by app_id. For single-app mode, uses "default".
    apps: Arc<Mutex<HashMap<String, AppBundle>>>,
    /// Default app ID (for single-app mode).
    default_app: Option<String>,
}

/// Start the server in single-app mode (one app, backward compatible).
pub async fn serve(
    script_path: &str,
    port: u16,
    static_file: Option<&str>,
    plugins: Vec<Box<dyn Plugin>>,
) -> Result<(), deno_error::JsErrorBox> {
    fn err(e: impl std::fmt::Display) -> deno_error::JsErrorBox {
        deno_error::JsErrorBox::generic(e.to_string())
    }

    let server_js = std::fs::read_to_string(script_path)
        .map_err(|e| err(format!("Failed to read {script_path}: {e}")))?;

    let client_html = static_file.map(|path| {
        Bytes::from(std::fs::read(path).unwrap_or_else(|e| {
            eprintln!("[appbase-rt] Warning: could not read {path}: {e}");
            b"<html><body>appbase</body></html>".to_vec()
        }))
    });

    let mut apps = HashMap::new();
    apps.insert(
        "default".to_string(),
        AppBundle {
            server_js,
            client_html,
        },
    );

    // Single-app mode: direct V8 worker, no pool needed
    serve_single_app(port, apps, plugins).await.map_err(err)
}

/// Single-app server — spawns one V8 worker, serves one app.
async fn serve_single_app(
    port: u16,
    apps: HashMap<String, AppBundle>,
    plugins: Vec<Box<dyn Plugin>>,
) -> Result<(), String> {
    use crate::cpu_timer::{CpuLimits, CpuUsage};
    use crate::v8::{create_v8_runtime, handle_rpc};
    use tokio::sync::{mpsc, oneshot};

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

    let bundle = apps.get("default").ok_or("No default app")?;
    let server_js = bundle.server_js.clone();
    let client_html = bundle.client_html.clone();

    let (rpc_tx, mut rpc_rx) = mpsc::channel::<RpcRequest>(256);

    // V8 worker thread
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let (mut runtime, rpc_result) = match create_v8_runtime(&plugins) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("[appbase-rt] Failed to create V8 runtime: {e}");
                    return;
                }
            };

            if let Err(e) = runtime.execute_script("<user>", server_js) {
                eprintln!("[appbase-rt] Failed to execute script: {e}");
                return;
            }
            if let Err(e) = runtime.run_event_loop(Default::default()).await {
                eprintln!("[appbase-rt] Event loop error: {e}");
                return;
            }

            eprintln!("[appbase-rt] V8 isolate ready");

            let cpu_limits = CpuLimits::default();
            let mut cpu_usage = CpuUsage::default();

            while let Some(req) = rpc_rx.recv().await {
                let result = handle_rpc(&mut runtime, &rpc_result, &req.body, &cpu_limits).await;
                let reply = match result {
                    Ok(resp) => {
                        cpu_usage.record(resp.cpu_time);
                        Ok(RpcReply {
                            json: resp.json,
                            cpu_ms: resp.cpu_time.as_secs_f64() * 1000.0,
                            total_cpu_ms: cpu_usage.total.as_secs_f64() * 1000.0,
                            request_count: cpu_usage.request_count,
                        })
                    }
                    Err(e) => Err(e.to_string()),
                };
                let _ = req.reply.send(reply);
            }
        });
    });

    // Axum HTTP server
    #[derive(Clone)]
    struct SingleAppState {
        rpc_tx: mpsc::Sender<RpcRequest>,
        client_html: Option<Bytes>,
    }

    let state = SingleAppState {
        rpc_tx,
        client_html,
    };

    let cors = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers(Any)
        .allow_origin(Any);

    let app = Router::new()
        .route("/rpc", post({
            let state = state.clone();
            move |body: String| async move {
                let (reply_tx, reply_rx) = oneshot::channel();
                if state.rpc_tx.send(RpcRequest { body, reply: reply_tx }).await.is_err() {
                    return json_response(StatusCode::SERVICE_UNAVAILABLE,
                        r#"{"jsonrpc":"2.0","error":{"code":-32603,"message":"V8 unavailable"},"id":null}"#);
                }
                match reply_rx.await {
                    Ok(Ok(reply)) => {
                        eprintln!("[appbase-rt] POST /rpc cpu={:.2}ms total={:.2}ms reqs={}",
                            reply.cpu_ms, reply.total_cpu_ms, reply.request_count);
                        json_response(StatusCode::OK, &reply.json)
                    }
                    Ok(Err(e)) => json_response(StatusCode::INTERNAL_SERVER_ERROR,
                        &format!(r#"{{"jsonrpc":"2.0","error":{{"code":-32603,"message":"{}"}},"id":null}}"#,
                            sanitize_error(&e))),
                    Err(_) => json_response(StatusCode::INTERNAL_SERVER_ERROR,
                        r#"{"jsonrpc":"2.0","error":{"code":-32603,"message":"V8 crashed"},"id":null}"#),
                }
            }
        }))
        .fallback(get({
            let state = state.clone();
            move || async move {
                if let Some(ref html) = state.client_html {
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                        .body(Body::from(html.clone()))
                        .unwrap()
                } else {
                    json_response(StatusCode::OK, r#"{"status":"appbase-rt running"}"#)
                }
            }
        }))
        .layer(cors);

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .map_err(|e| e.to_string())?;
    eprintln!("[appbase-rt] http://localhost:{port}");

    axum::serve(listener, app)
        .await
        .map_err(|e| e.to_string())?;

    Ok(())
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
