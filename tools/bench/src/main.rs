mod config;
mod http;
mod lua;
mod numa;
mod report;
mod sse;
mod stats;
mod thread;
mod tls;
mod tui;
mod ws;

use std::sync::Arc;

use config::Config;
use stats::{LiveStats, Summary, SseSummary, WsSummary, ThreadStats, SseThreadStats, WsThreadStats};

fn main() {
    let config = Config::from_args();
    let cpus = numa::resolve_cpus(&config.cpu_affinity, &config.numa_node, config.threads);

    if config.sse {
        eprintln!("Running {:?} SSE test @ {}", config.duration, config.url);
    } else if config.ws {
        eprintln!("Running {:?} WebSocket test @ {}", config.duration, config.url);
    } else {
        eprintln!("Running {:?} test @ {}", config.duration, config.url);
    }
    eprintln!("  {} threads and {} connections", config.threads, config.connections);

    if config.tls {
        eprintln!("  TLS enabled");
    }
    if let Some(ref s) = config.script {
        eprintln!("  Lua script: {s}");
    }

    let start = std::time::Instant::now();

    if config.ws {
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
        // HTTP mode — optionally with TUI + Lua.
        // Shared atomic counters for TUI live updates.
        let live: Option<Arc<LiveStats>> = if config.tui {
            Some(LiveStats::new())
        } else {
            None
        };

        // Spawn worker threads.
        let handles: Vec<_> = (0..config.threads)
            .map(|i| {
                let cfg = Config::from_args();
                let cpu = cpus[i];
                let live_clone = live.clone();
                std::thread::spawn(move || thread::run_worker(&cfg, i, cpu, live_clone))
            })
            .collect();

        // If TUI is enabled, run it on the main thread while workers run.
        // The TUI loop blocks until the benchmark duration elapses or user presses 'q'.
        if let Some(ref l) = live {
            tui::run_tui(&config, Arc::clone(l));
        }

        let thread_stats: Vec<ThreadStats> = handles
            .into_iter()
            .map(|h| h.join().expect("worker thread panicked"))
            .collect();

        let duration = start.elapsed();
        let summary = Summary::merge(thread_stats, duration);

        // Lua done() callback — fires once after all threads complete.
        if let Some(ref script_path) = config.script {
            match lua::LuaScript::load(script_path, &config) {
                Ok(script) => script.call_done(&summary),
                Err(e) => eprintln!("[lua] failed to load script for done(): {e}"),
            }
        }

        report::print_wrk_format(&summary, &config);
    }
}
