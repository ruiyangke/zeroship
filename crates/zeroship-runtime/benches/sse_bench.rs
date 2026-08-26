//! SSE Streaming Benchmark Tool
//!
//! Measures real-time SSE performance with nanosecond precision:
//! - Time to first byte (TTFB)
//! - Per-chunk latency (server enqueue → client receive)
//! - Chunk throughput (chunks/sec)
//! - Concurrent stream performance
//!
//! Usage:
//!   sse-bench http://localhost:5100/sse?chunks=100&delay=0
//!   sse-bench http://localhost:5100/sse?chunks=1000&delay=0 --connections 10
//!   sse-bench http://localhost:5100/sse?chunks=100&delay=1 --connections 50 --runs 3

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: sse-bench <url> [--connections N] [--runs N]");
        eprintln!("  url: http://host:port/path?query");
        eprintln!("  --connections N: concurrent SSE streams (default: 1)");
        eprintln!("  --runs N: repeat N times, report min/avg/max (default: 3)");
        std::process::exit(1);
    }

    let url = &args[1];
    let connections = parse_arg(&args, "--connections", 1);
    let runs = parse_arg(&args, "--runs", 3);

    let parsed = parse_url(url);

    println!("SSE Benchmark");
    println!("  URL:         {url}");
    println!("  Connections: {connections}");
    println!("  Runs:        {runs}");
    println!();

    if connections == 1 {
        // Single stream: detailed per-chunk analysis
        let mut all_results = Vec::new();
        for run in 1..=runs {
            let result = bench_single_stream(&parsed);
            println!("  Run {run}: TTFB {:.2}ms | {} chunks in {:.2}ms | {:.0} chunks/s | p50 {:.2}ms p99 {:.2}ms",
                result.ttfb_ms, result.chunk_count, result.total_ms,
                result.chunks_per_sec, result.p50_ms, result.p99_ms);
            all_results.push(result);
        }
        println!();
        print_summary(&all_results);
    } else {
        // Concurrent streams: throughput analysis
        for run in 1..=runs {
            let result = bench_concurrent(&parsed, connections);
            println!("  Run {run}: {} streams × {} chunks in {:.2}ms | {:.0} total chunks/s | TTFB min {:.2}ms max {:.2}ms",
                connections, result.chunks_per_stream, result.total_ms,
                result.total_chunks_per_sec, result.min_ttfb_ms, result.max_ttfb_ms);
        }
    }
}

// ---------------------------------------------------------------------------
// Single stream benchmark
// ---------------------------------------------------------------------------

struct SingleResult {
    ttfb_ms: f64,
    total_ms: f64,
    chunk_count: usize,
    chunks_per_sec: f64,
    chunk_latencies_us: Vec<f64>,
    p50_ms: f64,
    p99_ms: f64,
}

fn bench_single_stream(url: &ParsedUrl) -> SingleResult {
    let start = Instant::now();
    let mut reader = connect_and_send(url);

    let mut first_byte_time: Option<Instant> = None;
    let mut chunk_count = 0usize;
    let mut chunk_latencies_us = Vec::new();
    let mut last_chunk_time = start;
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).unwrap_or(0);
        if n == 0 { break; }

        if first_byte_time.is_none() {
            first_byte_time = Some(Instant::now());
        }

        let trimmed = line.trim();
        if trimmed.starts_with("data:") {
            let now = Instant::now();
            let latency = now.duration_since(last_chunk_time);
            chunk_latencies_us.push(latency.as_secs_f64() * 1_000_000.0);
            last_chunk_time = now;
            chunk_count += 1;

            if trimmed == "data: [DONE]" {
                break;
            }
        }
    }

    let total = start.elapsed();
    let ttfb = first_byte_time.map(|t| t.duration_since(start)).unwrap_or(total);

    // Sort latencies for percentiles
    chunk_latencies_us.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = percentile(&chunk_latencies_us, 50.0);
    let p99 = percentile(&chunk_latencies_us, 99.0);

    let total_ms = total.as_secs_f64() * 1000.0;
    let cps = if total_ms > 0.0 { chunk_count as f64 / (total_ms / 1000.0) } else { 0.0 };

    SingleResult {
        ttfb_ms: ttfb.as_secs_f64() * 1000.0,
        total_ms,
        chunk_count,
        chunks_per_sec: cps,
        chunk_latencies_us,
        p50_ms: p50 / 1000.0,
        p99_ms: p99 / 1000.0,
    }
}

// ---------------------------------------------------------------------------
// Concurrent streams benchmark
// ---------------------------------------------------------------------------

struct ConcurrentResult {
    total_ms: f64,
    chunks_per_stream: usize,
    total_chunks_per_sec: f64,
    min_ttfb_ms: f64,
    max_ttfb_ms: f64,
}

fn bench_concurrent(url: &ParsedUrl, n: usize) -> ConcurrentResult {
    let start = Instant::now();

    let handles: Vec<_> = (0..n)
        .map(|_| {
            let url = url.clone();
            std::thread::spawn(move || bench_single_stream(&url))
        })
        .collect();

    let results: Vec<SingleResult> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    let total = start.elapsed();
    let total_chunks: usize = results.iter().map(|r| r.chunk_count).sum();
    let chunks_per_stream = results.first().map(|r| r.chunk_count).unwrap_or(0);
    let total_ms = total.as_secs_f64() * 1000.0;
    let total_cps = if total_ms > 0.0 { total_chunks as f64 / (total_ms / 1000.0) } else { 0.0 };
    let min_ttfb = results.iter().map(|r| r.ttfb_ms).fold(f64::MAX, f64::min);
    let max_ttfb = results.iter().map(|r| r.ttfb_ms).fold(0.0f64, f64::max);

    ConcurrentResult {
        total_ms,
        chunks_per_stream,
        total_chunks_per_sec: total_cps,
        min_ttfb_ms: min_ttfb,
        max_ttfb_ms: max_ttfb,
    }
}

// ---------------------------------------------------------------------------
// HTTP connection (raw TCP — no dependencies)
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct ParsedUrl {
    host: String,
    port: u16,
    path: String,
}

fn parse_url(url: &str) -> ParsedUrl {
    let without_scheme = url.strip_prefix("http://").unwrap_or(url);
    let (host_port, path) = without_scheme.split_once('/').unwrap_or((without_scheme, ""));
    let (host, port) = if host_port.contains(':') {
        let (h, p) = host_port.split_once(':').unwrap();
        (h.to_string(), p.parse().unwrap_or(80))
    } else {
        (host_port.to_string(), 80)
    };
    ParsedUrl { host, port, path: format!("/{path}") }
}

fn connect_and_send(url: &ParsedUrl) -> BufReader<TcpStream> {
    let addr = format!("{}:{}", url.host, url.port);
    let mut stream = TcpStream::connect(&addr).expect("connect failed");
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok();

    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}:{}\r\nAccept: text/event-stream\r\nConnection: close\r\n\r\n",
        url.path, url.host, url.port
    );
    stream.write_all(request.as_bytes()).expect("write failed");
    stream.flush().expect("flush failed");

    let mut reader = BufReader::new(stream);

    // Skip HTTP headers
    let mut line = String::new();
    loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        if line.trim().is_empty() { break; }
    }

    reader
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn percentile(sorted: &[f64], pct: f64) -> f64 {
    if sorted.is_empty() { return 0.0; }
    let idx = ((pct / 100.0) * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn print_summary(results: &[SingleResult]) {
    let avg_ttfb: f64 = results.iter().map(|r| r.ttfb_ms).sum::<f64>() / results.len() as f64;
    let avg_cps: f64 = results.iter().map(|r| r.chunks_per_sec).sum::<f64>() / results.len() as f64;
    let avg_p50: f64 = results.iter().map(|r| r.p50_ms).sum::<f64>() / results.len() as f64;
    let avg_p99: f64 = results.iter().map(|r| r.p99_ms).sum::<f64>() / results.len() as f64;

    println!("  Summary ({} runs):", results.len());
    println!("    Avg TTFB:       {:.2}ms", avg_ttfb);
    println!("    Avg throughput: {:.0} chunks/s", avg_cps);
    println!("    Avg p50:        {:.2}ms", avg_p50);
    println!("    Avg p99:        {:.2}ms", avg_p99);
}

fn parse_arg(args: &[String], flag: &str, default: usize) -> usize {
    for pair in args.windows(2) {
        if pair[0] == flag {
            return pair[1].parse().unwrap_or(default);
        }
    }
    default
}
