mod config;
mod http;
mod numa;
mod report;
mod sse;
mod stats;
mod thread;
mod ws;

use config::Config;
use stats::{Summary, SseSummary, WsSummary, ThreadStats, SseThreadStats, WsThreadStats};

fn main() {
    let config = Config::from_args();
    let cpus = numa::resolve_cpus(&config.cpu_affinity, &config.numa_node, config.threads);

    if config.sse {
        eprintln!(
            "Running {:?} SSE test @ {}",
            config.duration, config.url
        );
    } else if config.ws {
        eprintln!(
            "Running {:?} WebSocket test @ {}",
            config.duration, config.url
        );
    } else {
        eprintln!(
            "Running {:?} test @ {}",
            config.duration, config.url
        );
    }
    eprintln!(
        "  {} threads and {} connections",
        config.threads, config.connections
    );

    let start = std::time::Instant::now();

    if config.ws {
        // WebSocket mode: each thread manages N persistent upgraded connections,
        // sending a message and recording RTT per round-trip.
        let handles: Vec<_> = (0..config.threads)
            .map(|i| {
                let cfg = Config::from_args();
                let cpu = cpus[i];
                std::thread::spawn(move || ws::run_ws_worker(&cfg, i, cpu))
            })
            .collect();

        let thread_stats: Vec<WsThreadStats> = handles
            .into_iter()
            .map(|h| h.join().expect("WS worker thread panicked"))
            .collect();

        let duration = start.elapsed();
        let summary = WsSummary::merge(thread_stats, duration);

        report::print_ws_format(&summary, &config);
    } else if config.sse {
        // SSE mode: each thread runs long-lived streaming connections.
        let handles: Vec<_> = (0..config.threads)
            .map(|i| {
                let cfg = Config::from_args();
                let cpu = cpus[i];
                std::thread::spawn(move || sse::run_sse_worker(&cfg, i, cpu))
            })
            .collect();

        let thread_stats: Vec<SseThreadStats> = handles
            .into_iter()
            .map(|h| h.join().expect("SSE worker thread panicked"))
            .collect();

        let duration = start.elapsed();
        let summary = SseSummary::merge(thread_stats, duration);

        report::print_sse_format(&summary, &config);
    } else {
        // HTTP mode: spawn N OS threads, each running a compio event loop.
        // Config is re-parsed per thread so each thread gets its own owned copy
        // (avoids Send requirements on the config reference).
        let handles: Vec<_> = (0..config.threads)
            .map(|i| {
                let cfg = Config::from_args();
                let cpu = cpus[i];
                std::thread::spawn(move || thread::run_worker(&cfg, i, cpu))
            })
            .collect();

        let thread_stats: Vec<ThreadStats> = handles
            .into_iter()
            .map(|h| h.join().expect("worker thread panicked"))
            .collect();

        let duration = start.elapsed();
        let summary = Summary::merge(thread_stats, duration);

        report::print_wrk_format(&summary, &config);
    }
}
