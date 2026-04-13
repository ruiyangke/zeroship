mod auth;
mod enforce;
mod proxy;
mod router;
mod sync;

use std::sync::Arc;

use ntex::web;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[allow(missing_debug_implementations)]
pub struct GateConfig {
    pub control_url: String,
    pub control_key: String,
    pub worker_urls: Vec<String>,
    pub poll_interval_secs: u64,
}

#[allow(missing_debug_implementations)]
pub struct GateState {
    pub config: GateConfig,
    pub routes: sync::RouteCache,
    pub hash_ring: proxy::HashRing,
    pub rate_limiters: enforce::RateLimitRegistry,
    pub concurrency: enforce::ConcurrencyRegistry,
}

#[ntex::main]
async fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let port = arg_or_env(&args, "--port", "GATE_PORT", "80");
    let control_url = arg_or_env(&args, "--control", "CONTROL_URL", "http://localhost:9090");
    let control_key = arg_or_env(&args, "--control-key", "CONTROL_KEY", "");
    let workers_str = arg_or_env(&args, "--workers", "WORKER_URLS", "http://localhost:8080");
    let poll_interval = arg_or_env(&args, "--poll-interval", "POLL_INTERVAL", "5");

    let worker_urls: Vec<String> = workers_str
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();

    let num_workers = worker_urls.len();
    // Bounded load: each worker handles at most 125% of average load
    // With 10K apps and 10 workers, avg = 1K apps → max = 1.25K
    // For request concurrency, use a generous static bound
    let max_per_worker = 500u32;

    eprintln!(
        "[zeroship-gate] {} workers, CHWBL with 150 vnodes, max {max_per_worker} req/worker",
        num_workers
    );

    let hash_ring = proxy::HashRing::new(worker_urls.clone(), max_per_worker);

    let state = Arc::new(GateState {
        config: GateConfig {
            control_url,
            control_key,
            worker_urls,
            poll_interval_secs: poll_interval.parse().unwrap_or(5),
        },
        routes: sync::RouteCache::new(),
        hash_ring,
        rate_limiters: enforce::RateLimitRegistry::new(1000, 2000),
        concurrency: enforce::ConcurrencyRegistry::new(100),
    });

    sync::start_sync(state.clone());

    let bind_addr = format!("0.0.0.0:{port}");
    eprintln!("[zeroship-gate] http://{bind_addr}");

    web::server(async move || {
        web::App::new()
            .state(state.clone())
            .service(
                web::resource("/apps/{app_name}/{tail:.*}")
                    .route(web::route().to(router::handle)),
            )
            .service(web::resource("/health").route(web::get().to(|| async {
                web::HttpResponse::Ok().body(r#"{"status":"ok"}"#)
            })))
            // Subdomain catch-all — must be last (lowest priority)
            .service(
                web::resource("/{tail:.*}")
                    .route(web::route().to(router::handle_subdomain)),
            )
    })
    .bind(&bind_addr)?
    .run()
    .await
}

fn arg_or_env(args: &[String], flag: &str, env_key: &str, default: &str) -> String {
    for pair in args.windows(2) {
        if pair[0] == flag {
            return pair[1].clone();
        }
    }
    std::env::var(env_key).unwrap_or_else(|_| default.to_string())
}
