//! ntex + compio + V8 benchmark server.
//!
//! Same as v8-server-compio but uses ntex for HTTP instead of raw httparse.
//! Each ntex worker thread gets its own V8 Runtime (same as serve.rs).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use ntex::web::{self, App, HttpRequest, HttpResponse};
use serde::Deserialize;

use zeroship_runtime::init::init_v8;
use zeroship_runtime::modules::ModuleEntry;
use zeroship_runtime::runtime::Runtime;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const SERVER_JS: &str = include_str!("../scenarios.js");

fn server_modules() -> Vec<ModuleEntry> {
    vec![ModuleEntry {
        specifier: "index.js".into(),
        source: SERVER_JS.into(),
    }]
}

// Thread-local V8 runtime — each ntex worker gets its own.
thread_local! {
    static V8_RUNTIME: RefCell<Option<Rc<RefCell<Runtime>>>> = const { RefCell::new(None) };
}

/// Initialize the V8 runtime on this worker thread.
fn ensure_v8_initialized(modules: &[ModuleEntry]) {
    V8_RUNTIME.with(|cell| {
        if cell.borrow().is_none() {
            let rt = Runtime::new_direct(modules.to_vec(), HashMap::new(), None, None);
            let rt = Rc::new(RefCell::new(rt));

            // Warmup
            {
                let result = rt.borrow_mut().dispatch_rpc(
                    r#"{"jsonrpc":"2.0","method":"ping","params":[],"id":0}"#,
                );
                if let Err(e) = result {
                    eprintln!("[ntex-v8] warmup failed: {e}");
                }
            }

            // Start pump task for async V8 ops (timers, fetch, streams).
            Runtime::start_pump(rt.clone());

            *cell.borrow_mut() = Some(rt);
        }
    });
}

#[derive(Deserialize)]
struct RpcRequest {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    method: String,
    #[allow(dead_code)]
    params: Option<serde_json::Value>,
    #[allow(dead_code)]
    id: Option<serde_json::Value>,
}

async fn health() -> HttpResponse {
    HttpResponse::Ok()
        .content_type("application/json")
        .body(r#"{"status":"ok"}"#)
}

async fn rpc(body: String) -> HttpResponse {
    let result = V8_RUNTIME.with(|cell| {
        let binding = cell.borrow();
        let rt = binding.as_ref().unwrap();
        let mut rt = rt.borrow_mut();
        rt.dispatch_rpc(&body)
    });

    match result {
        Ok(rr) => {
            let mut response = rr.json;
            // Ensure it's valid JSON response
            if response.is_empty() {
                response = r#"{"jsonrpc":"2.0","result":null,"id":null}"#.to_string();
            }
            HttpResponse::Ok()
                .content_type("application/json")
                .body(response)
        }
        Err(e) => {
            let error_json = serde_json::json!({
                "jsonrpc": "2.0",
                "error": { "code": -32000, "message": e },
                "id": null
            });
            HttpResponse::Ok()
                .content_type("application/json")
                .body(serde_json::to_string(&error_json).unwrap())
        }
    }
}

async fn http_handler(req: HttpRequest) -> HttpResponse {
    let body = serde_json::json!({
        "method": req.method().to_string(),
        "url": req.uri().to_string(),
    });
    HttpResponse::Ok()
        .content_type("application/json")
        .body(serde_json::to_string(&body).unwrap())
}

#[ntex::main]
async fn main() -> std::io::Result<()> {
    init_v8();

    let port: u16 = std::env::args()
        .find(|a| a.starts_with("--port="))
        .and_then(|a| a.strip_prefix("--port=").unwrap().parse().ok())
        .unwrap_or(5200);

    let workers: usize = std::env::args()
        .find(|a| a.starts_with("--workers="))
        .and_then(|a| a.strip_prefix("--workers=").unwrap().parse().ok())
        .unwrap_or(1);

    eprintln!("[ntex-v8-compio] starting on port {port} with {workers} workers");

    let modules = server_modules();
    eprintln!(
        "[ntex-v8-compio] appbundle loaded: {} modules",
        modules.len()
    );

    // Leak modules so they're 'static and can be shared across threads
    let modules: &'static [ModuleEntry] = Box::leak(modules.into_boxed_slice());

    web::server(async move || {
        // Initialize V8 on this worker thread
        ensure_v8_initialized(modules);

        App::new()
            .service(web::resource("/health").route(web::get().to(health)))
            .service(web::resource("/rpc").route(web::post().to(rpc)))
            .default_service(web::route().to(http_handler))
    })
    .workers(workers)
    .bind(format!("0.0.0.0:{port}"))?
    .run()
    .await
}
