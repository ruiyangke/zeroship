mod handler;
mod sync;
mod cache;

use std::sync::Arc;
use ntex::web;
use zeroship_runtime::init::init_v8;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[allow(missing_debug_implementations)]
pub struct WorkerConfig {
    pub control_url: String,
    pub control_key: String,
    pub db_url: Option<String>,
    pub max_isolates: usize,
    pub poll_interval_secs: u64,
}

#[ntex::main]
async fn main() -> std::io::Result<()> {
    init_v8();

    let args: Vec<String> = std::env::args().collect();
    let port = arg_or_env(&args, "--port", "WORKER_PORT", "8080");
    let workers = arg_or_env(
        &args,
        "--workers",
        "WORKER_THREADS",
        &std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .to_string(),
    );
    let control_url = arg_or_env(&args, "--control", "CONTROL_URL", "http://localhost:9090");
    let control_key = arg_or_env(&args, "--control-key", "CONTROL_KEY", "");
    let max_isolates = arg_or_env(&args, "--max-isolates", "MAX_ISOLATES", "200");
    let poll_interval = arg_or_env(&args, "--poll-interval", "POLL_INTERVAL", "5");
    let db_url = arg_or_env(&args, "--db", "DATABASE_URL", "");

    let config = Arc::new(WorkerConfig {
        control_url,
        control_key,
        db_url: if db_url.is_empty() { None } else { Some(db_url) },
        max_isolates: max_isolates.parse().unwrap_or(200),
        poll_interval_secs: poll_interval.parse().unwrap_or(5),
    });

    let socket_path = arg_or_env(&args, "--socket", "WORKER_SOCKET", "");
    let workers_count: usize = workers.parse().unwrap_or(1);
    let bind_addr = format!("0.0.0.0:{port}");

    eprintln!("[zeroship-worker] http://{bind_addr} ({workers_count} threads)");
    if !socket_path.is_empty() {
        eprintln!("[zeroship-worker] unix://{socket_path}");
        // Remove stale socket file
        let _ = std::fs::remove_file(&socket_path);
    }

    let mut server = web::server(async move || {
        let config = config.clone();
        cache::init_cache(config.max_isolates, config.db_url.clone());
        sync::start_sync(config.clone());

        web::App::new()
            .state(config)
            .service(web::resource("/dispatch/{app_id}").route(web::post().to(handler::dispatch)))
            .service(web::resource("/health").route(web::get().to(|| async {
                web::HttpResponse::Ok().body(r#"{"status":"ok"}"#)
            })))
    })
    .workers(workers_count)
    .bind(&bind_addr)?;

    // Also listen on Unix domain socket if configured
    if !socket_path.is_empty() {
        server = server.bind_uds(&socket_path)?;
    }

    server.run().await
}

fn arg_or_env(args: &[String], flag: &str, env_key: &str, default: &str) -> String {
    for pair in args.windows(2) {
        if pair[0] == flag {
            return pair[1].clone();
        }
    }
    std::env::var(env_key).unwrap_or_else(|_| default.to_string())
}
