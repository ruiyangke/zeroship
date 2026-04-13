//! ntex + compio + V8 via flume channels (v8pool model).
//!
//! HTTP handled by ntex worker threads, V8 dispatched to a dedicated
//! compio thread via flume channels. Simulates the current platform v8pool.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use ntex::web::{self, App, HttpRequest, HttpResponse};

use zeroship_runtime::bundle::{AppBundle, ModuleType};
use zeroship_runtime::init::init_v8;
use zeroship_runtime::modules::ModuleEntry;
use zeroship_runtime::runtime::{AsyncWork, Runtime};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const SERVER_JS: &str = include_str!("../scenarios.js");

fn server_modules() -> Vec<ModuleEntry> {
    let bundle = AppBundle::new(
        "index.js",
        vec![("index.js".into(), ModuleType::EsModule, SERVER_JS.into())],
    );
    let bytes = bundle.to_bytes();
    let mut loaded = AppBundle::from_bytes(&bytes).expect("Failed to parse .appbundle");
    loaded.to_module_entries()
}

// ---------------------------------------------------------------------------
// V8 worker thread (runs on its own compio runtime)
// ---------------------------------------------------------------------------

struct WorkRequest {
    body: String,
    reply: flume::Sender<Result<String, String>>,
}

fn spawn_v8_worker(
    modules: Vec<ModuleEntry>,
) -> flume::Sender<WorkRequest> {
    let (tx, rx) = flume::bounded::<WorkRequest>(256);

    std::thread::Builder::new()
        .name("v8-worker".into())
        .spawn(move || {
            compio::runtime::RuntimeBuilder::new()
                .build()
                .unwrap()
                .block_on(async {
                    let runtime = Rc::new(RefCell::new(
                        Runtime::new_direct(modules, HashMap::new(), None, None),
                    ));

                    // Warmup
                    {
                        let _ = runtime.borrow_mut().dispatch_rpc(
                            r#"{"jsonrpc":"2.0","method":"ping","params":[],"id":0}"#,
                        );
                    }

                    // Pump task
                    let mut async_work = AsyncWork::new();
                    let (notify_tx, notify_rx) = futures::channel::mpsc::channel::<()>(1);
                    runtime.borrow_mut().set_pump_notify(notify_tx);
                    runtime.borrow_mut().drain_new_tasks_into(&mut async_work);

                    let rt_pump = runtime.clone();
                    compio::runtime::spawn(async move {
                        pump_task(rt_pump, async_work, notify_rx).await;
                    })
                    .detach();

                    // Request loop
                    while let Ok(req) = rx.recv_async().await {
                        let result = {
                            let mut rt = runtime.borrow_mut();
                            rt.dispatch_rpc(&req.body)
                        };
                        let reply = match result {
                            Ok(rr) => Ok(rr.json),
                            Err(e) => Err(e),
                        };
                        let _ = req.reply.send(reply);
                    }
                });
        })
        .unwrap();

    tx
}

async fn pump_task(
    runtime: Rc<RefCell<Runtime>>,
    mut work: AsyncWork,
    mut notify_rx: futures::channel::mpsc::Receiver<()>,
) {
    use futures::StreamExt;
    loop {
        {
            let mut rt = runtime.borrow_mut();
            rt.drain_new_tasks_into(&mut work);
        }

        let event = {
            let has_ops = !work.pending_ops.is_empty();
            let has_timers = !work.pending_timers.is_empty();

            match (has_ops, has_timers) {
                (true, true) => {
                    futures::select! {
                        r = work.pending_ops.select_next_some() => Some(zeroship_runtime::runtime::AsyncEvent::Op(r)),
                        r = work.pending_timers.select_next_some() => Some(zeroship_runtime::runtime::AsyncEvent::Timer(r)),
                        _ = notify_rx.next() => None,
                    }
                }
                (true, false) => {
                    futures::select! {
                        r = work.pending_ops.select_next_some() => Some(zeroship_runtime::runtime::AsyncEvent::Op(r)),
                        _ = notify_rx.next() => None,
                    }
                }
                (false, true) => {
                    futures::select! {
                        r = work.pending_timers.select_next_some() => Some(zeroship_runtime::runtime::AsyncEvent::Timer(r)),
                        _ = notify_rx.next() => None,
                    }
                }
                (false, false) => {
                    let _ = notify_rx.next().await;
                    None
                }
            }
        };

        if let Some(event) = event {
            let mut rt = runtime.borrow_mut();
            rt.handle_async_event(event, &mut work);
        }
    }
}

// ---------------------------------------------------------------------------
// ntex HTTP handlers — dispatch via flume to V8 worker
// ---------------------------------------------------------------------------

struct AppState {
    v8_tx: flume::Sender<WorkRequest>,
}

async fn health() -> HttpResponse {
    HttpResponse::Ok()
        .content_type("application/json")
        .body(r#"{"status":"ok"}"#)
}

async fn rpc(body: String, state: web::types::State<Arc<AppState>>) -> HttpResponse {
    let (reply_tx, reply_rx) = flume::bounded(1);
    let req = WorkRequest {
        body,
        reply: reply_tx,
    };

    if state.v8_tx.send_async(req).await.is_err() {
        return HttpResponse::InternalServerError().body("V8 worker unavailable");
    }

    match reply_rx.recv_async().await {
        Ok(Ok(json)) => HttpResponse::Ok()
            .content_type("application/json")
            .body(json),
        Ok(Err(e)) => {
            let error_json = serde_json::json!({
                "jsonrpc": "2.0",
                "error": { "code": -32000, "message": e },
                "id": null
            });
            HttpResponse::Ok()
                .content_type("application/json")
                .body(serde_json::to_string(&error_json).unwrap())
        }
        Err(_) => HttpResponse::InternalServerError().body("V8 worker crashed"),
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
        .unwrap_or(5300);

    let workers: usize = std::env::args()
        .find(|a| a.starts_with("--workers="))
        .and_then(|a| a.strip_prefix("--workers=").unwrap().parse().ok())
        .unwrap_or(1);

    eprintln!("[ntex-flume-v8] starting on port {port} with {workers} HTTP workers + 1 V8 worker");

    let modules = server_modules();
    let v8_tx = spawn_v8_worker(modules);
    let state = Arc::new(AppState { v8_tx });

    web::server(async move || {
        let state = state.clone();
        App::new()
            .state(state)
            .service(web::resource("/health").route(web::get().to(health)))
            .service(web::resource("/rpc").route(web::post().to(rpc)))
            .default_service(web::route().to(http_handler))
    })
    .workers(workers)
    .bind(format!("0.0.0.0:{port}"))?
    .run()
    .await
}
