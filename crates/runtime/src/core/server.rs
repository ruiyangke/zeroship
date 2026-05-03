//! Benchmark HTTP server for V8 runtime using compio (io_uring).
//!
//! This is the zeroship-bench-server binary. It loads the embedded `scenarios.js`
//! as a single ES module and starts the compio HTTP server via the shared
//! `zeroship_runtime::serve` module.

use std::time::Duration;

use zeroship_runtime::modules::ModuleEntry;
use zeroship_runtime::serve::{start_server, ServerOptions};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Default JS loaded when no --js flag is provided.
const SERVER_JS: &str = include_str!("../../benches/scenarios.js");

fn server_modules() -> Vec<ModuleEntry> {
    vec![ModuleEntry {
        specifier: "index.js".into(),
        source: SERVER_JS.into(),
    }]
}

fn main() {
    let port: u16 = std::env::args()
        .find(|a| a.starts_with("--port="))
        .and_then(|a| a.strip_prefix("--port=").unwrap().parse().ok())
        .unwrap_or(5000);

    let num_workers: usize = std::env::args()
        .find(|a| a.starts_with("--workers="))
        .and_then(|a| a.strip_prefix("--workers=").unwrap().parse().ok())
        .unwrap_or(1);

    let cpu_limit: Option<Duration> = std::env::args()
        .find(|a| a.starts_with("--cpu-limit="))
        .and_then(|a| a.strip_prefix("--cpu-limit=").unwrap().parse::<u64>().ok())
        .map(Duration::from_millis);

    let wall_timeout: Option<Duration> = std::env::args()
        .find(|a| a.starts_with("--wall-timeout="))
        .and_then(|a| a.strip_prefix("--wall-timeout=").unwrap().parse::<u64>().ok())
        .map(Duration::from_millis);

    let modules = server_modules();

    start_server(modules, ServerOptions {
        port,
        workers: num_workers,
        cpu_limit,
        wall_timeout,
        ..Default::default()
    });
}
