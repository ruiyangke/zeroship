//! Benchmark HTTP server for V8 runtime using compio (io_uring).
//!
//! This is the v8-server-compio binary. It loads the embedded `scenarios.js`
//! into an .appbundle and starts the compio HTTP server via the shared
//! `zeroship_runtime::serve` module.

use std::time::Duration;

use zeroship_runtime::bundle::{AppBundle, ModuleType};
use zeroship_runtime::modules::ModuleEntry;
use zeroship_runtime::serve::{start_server, ServerOptions};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Default JS loaded when no --js flag is provided.
const SERVER_JS: &str = include_str!("../benches/scenarios.js");

/// Build an .appbundle at startup, then load modules from it.
/// This exercises the real production path: bundle -> parse -> decompress -> V8.
fn server_modules() -> Vec<ModuleEntry> {
    let bundle = AppBundle::new("index.js", vec![
        ("index.js".into(), ModuleType::EsModule, SERVER_JS.into()),
    ]);
    let bytes = bundle.to_bytes();

    eprintln!(
        "[v8-server-compio] appbundle: {} -> {} bytes ({:.0}% ratio)",
        SERVER_JS.len(),
        bytes.len(),
        bytes.len() as f64 / SERVER_JS.len() as f64 * 100.0,
    );

    let mut loaded = AppBundle::from_bytes(&bytes)
        .expect("Failed to parse .appbundle");
    loaded.to_module_entries()
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
