# zerobench Phase 1: HTTP Mode Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a wrk-compatible HTTP benchmark tool on compio/io_uring with NUMA-aware CPU pinning.

**Architecture:** Thread-per-core model with compio event loops. Each thread owns N connections, collects stats into a local HDR histogram. Stats merged after the test. CLI matches wrk flags. Output matches wrk format.

**Tech Stack:** Rust, compio (io_uring), hdrhistogram, clap, libc (CPU affinity)

---

## File Structure

```
tools/bench/
  Cargo.toml                 — binary crate, workspace member
  src/
    main.rs                  — CLI parsing via clap, thread spawning, orchestration
    config.rs                — Config struct, defaults, validation
    stats.rs                 — HDR histogram wrapper, per-thread stats, merge, percentiles
    thread.rs                — Worker thread: compio loop, connection pool, request cycle
    http.rs                  — HTTP/1.1 request builder + response parser (httparse)
    numa.rs                  — NUMA detection, CPU affinity via sched_setaffinity
    report.rs                — wrk-compatible text output + JSON
```

---

### Task 1: Scaffold the crate

**Files:**
- Create: `tools/bench/Cargo.toml`
- Create: `tools/bench/src/main.rs`

- [ ] **Step 1: Create Cargo.toml**

```toml
[package]
name = "zerobench"
version = "0.1.0"
edition = "2021"
description = "Next-gen HTTP benchmark tool — wrk superset with SSE, WebSocket, NUMA, TUI"

[[bin]]
name = "zerobench"
path = "src/main.rs"

[dependencies]
compio = { version = "0.18", features = ["io", "net", "runtime", "macros", "time"] }
hdrhistogram = "7"
httparse = "1"
libc = "0.2"
```

- [ ] **Step 2: Create minimal main.rs**

```rust
fn main() {
    println!("zerobench v0.1.0");
}
```

- [ ] **Step 3: Add to workspace**

Edit `/home/ruiyang/Projects/appbase/Cargo.toml` — add `"tools/bench"` to the `[workspace] members` list. Since the workspace uses `members = ["crates/*"]`, add an explicit entry:

```toml
[workspace]
members = ["crates/*", "tools/bench"]
```

- [ ] **Step 4: Verify it builds**

```bash
cargo check -p zerobench
```
Expected: compiles with zero errors.

- [ ] **Step 5: Commit**

```bash
git add tools/bench/ Cargo.toml Cargo.lock
git commit -m "feat(zerobench): scaffold crate — binary skeleton"
```

---

### Task 2: Config and CLI parsing

**Files:**
- Create: `tools/bench/src/config.rs`
- Modify: `tools/bench/src/main.rs`

- [ ] **Step 1: Create config.rs**

```rust
/// Benchmark configuration parsed from CLI arguments.
pub struct Config {
    pub url: String,
    pub host: String,
    pub port: u16,
    pub path: String,
    pub method: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    pub threads: usize,
    pub connections: usize,
    pub duration: std::time::Duration,
    pub timeout: std::time::Duration,
    pub print_latency: bool,
    pub json_output: bool,
    pub numa_node: Option<usize>,
    pub cpu_affinity: Option<Vec<usize>>,
}

impl Config {
    pub fn from_args() -> Self {
        let args: Vec<String> = std::env::args().collect();
        let mut config = Config {
            url: String::new(),
            host: String::new(),
            port: 80,
            path: "/".to_string(),
            method: "GET".to_string(),
            headers: Vec::new(),
            body: None,
            threads: num_cpus(),
            connections: 100,
            duration: std::time::Duration::from_secs(10),
            timeout: std::time::Duration::from_secs(2),
            print_latency: false,
            json_output: false,
            numa_node: None,
            cpu_affinity: None,
        };

        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "-t" | "--threads" => { i += 1; config.threads = args[i].parse().unwrap_or(config.threads); }
                "-c" | "--connections" => { i += 1; config.connections = args[i].parse().unwrap_or(config.connections); }
                "-d" | "--duration" => { i += 1; config.duration = parse_duration(&args[i]); }
                "-H" | "--header" => { i += 1; if let Some((k, v)) = args[i].split_once(": ") { config.headers.push((k.to_string(), v.to_string())); } }
                "--body" => { i += 1; config.body = Some(args[i].clone()); config.method = "POST".to_string(); }
                "--method" => { i += 1; config.method = args[i].clone(); }
                "--timeout" => { i += 1; config.timeout = parse_duration(&args[i]); }
                "--latency" => { config.print_latency = true; }
                "--json" => { config.json_output = true; }
                "--numa" => { i += 1; config.numa_node = args[i].parse().ok(); }
                "--cpu" => { i += 1; config.cpu_affinity = Some(parse_cpu_range(&args[i])); }
                arg if !arg.starts_with('-') => {
                    config.url = arg.to_string();
                    parse_url(&config.url, &mut config);
                }
                _ => {}
            }
            i += 1;
        }

        if config.url.is_empty() {
            eprintln!("Usage: zerobench [OPTIONS] <URL>");
            std::process::exit(1);
        }

        config
    }

    /// Connections per thread (evenly distributed).
    pub fn connections_per_thread(&self) -> usize {
        (self.connections + self.threads - 1) / self.threads
    }
}

fn parse_url(url: &str, config: &mut Config) {
    let without_scheme = url.strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url);
    let (host_port, path) = without_scheme.split_once('/').unwrap_or((without_scheme, ""));
    config.path = format!("/{path}");
    if let Some((h, p)) = host_port.split_once(':') {
        config.host = h.to_string();
        config.port = p.parse().unwrap_or(80);
    } else {
        config.host = host_port.to_string();
        config.port = if url.starts_with("https") { 443 } else { 80 };
    }
}

fn parse_duration(s: &str) -> std::time::Duration {
    if let Some(secs) = s.strip_suffix('s') {
        std::time::Duration::from_secs(secs.parse().unwrap_or(10))
    } else if let Some(mins) = s.strip_suffix('m') {
        std::time::Duration::from_secs(mins.parse::<u64>().unwrap_or(1) * 60)
    } else {
        std::time::Duration::from_secs(s.parse().unwrap_or(10))
    }
}

fn parse_cpu_range(s: &str) -> Vec<usize> {
    let mut cpus = Vec::new();
    for part in s.split(',') {
        if let Some((start, end)) = part.split_once('-') {
            let s: usize = start.parse().unwrap_or(0);
            let e: usize = end.parse().unwrap_or(s);
            cpus.extend(s..=e);
        } else if let Ok(n) = part.parse() {
            cpus.push(n);
        }
    }
    cpus
}

fn num_cpus() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}
```

- [ ] **Step 2: Update main.rs to use config**

```rust
mod config;

use config::Config;

fn main() {
    let config = Config::from_args();
    println!("zerobench v0.1.0");
    println!("  URL:         {}", config.url);
    println!("  Threads:     {}", config.threads);
    println!("  Connections: {}", config.connections);
    println!("  Duration:    {:?}", config.duration);
}
```

- [ ] **Step 3: Verify**

```bash
cargo run -p zerobench -- -t4 -c100 -d10s http://localhost:8080/rpc
```
Expected: prints config summary.

- [ ] **Step 4: Commit**

```bash
git add tools/bench/src/
git commit -m "feat(zerobench): CLI config parsing — wrk-compatible flags"
```

---

### Task 3: Stats collection (HDR histogram)

**Files:**
- Create: `tools/bench/src/stats.rs`

- [ ] **Step 1: Create stats.rs**

```rust
use hdrhistogram::Histogram;

/// Per-thread statistics. Lock-free — each thread owns its own instance.
pub struct ThreadStats {
    pub latency: Histogram<u64>,
    pub requests: u64,
    pub bytes: u64,
    pub errors_connect: u64,
    pub errors_read: u64,
    pub errors_write: u64,
    pub errors_timeout: u64,
    pub errors_status: u64,
}

impl ThreadStats {
    pub fn new() -> Self {
        Self {
            // 1µs to 60s range, 3 significant digits
            latency: Histogram::new_with_bounds(1, 60_000_000, 3).unwrap(),
            requests: 0,
            bytes: 0,
            errors_connect: 0,
            errors_read: 0,
            errors_write: 0,
            errors_timeout: 0,
            errors_status: 0,
        }
    }

    pub fn record_latency(&mut self, micros: u64) {
        let _ = self.latency.record(micros);
    }

    pub fn record_request(&mut self, bytes: u64) {
        self.requests += 1;
        self.bytes += bytes;
    }
}

/// Aggregated stats from all threads.
pub struct Summary {
    pub latency: Histogram<u64>,
    pub requests: u64,
    pub bytes: u64,
    pub duration: std::time::Duration,
    pub errors_connect: u64,
    pub errors_read: u64,
    pub errors_write: u64,
    pub errors_timeout: u64,
    pub errors_status: u64,
}

impl Summary {
    pub fn merge(thread_stats: Vec<ThreadStats>, duration: std::time::Duration) -> Self {
        let mut merged = Histogram::new_with_bounds(1, 60_000_000, 3).unwrap();
        let mut requests = 0u64;
        let mut bytes = 0u64;
        let mut ec = 0u64;
        let mut er = 0u64;
        let mut ew = 0u64;
        let mut et = 0u64;
        let mut es = 0u64;

        for ts in &thread_stats {
            merged.add(&ts.latency).ok();
            requests += ts.requests;
            bytes += ts.bytes;
            ec += ts.errors_connect;
            er += ts.errors_read;
            ew += ts.errors_write;
            et += ts.errors_timeout;
            es += ts.errors_status;
        }

        Summary {
            latency: merged,
            requests,
            bytes,
            duration,
            errors_connect: ec,
            errors_read: er,
            errors_write: ew,
            errors_timeout: et,
            errors_status: es,
        }
    }

    pub fn requests_per_sec(&self) -> f64 {
        self.requests as f64 / self.duration.as_secs_f64()
    }

    pub fn bytes_per_sec(&self) -> f64 {
        self.bytes as f64 / self.duration.as_secs_f64()
    }

    pub fn total_errors(&self) -> u64 {
        self.errors_connect + self.errors_read + self.errors_write + self.errors_timeout + self.errors_status
    }
}
```

- [ ] **Step 2: Verify**

```bash
cargo check -p zerobench
```

- [ ] **Step 3: Commit**

```bash
git add tools/bench/src/stats.rs
git commit -m "feat(zerobench): HDR histogram stats — per-thread, lock-free, mergeable"
```

---

### Task 4: HTTP request/response codec

**Files:**
- Create: `tools/bench/src/http.rs`

- [ ] **Step 1: Create http.rs**

```rust
use crate::config::Config;

/// Build an HTTP/1.1 request as bytes.
pub fn build_request(config: &Config) -> Vec<u8> {
    let mut req = format!("{} {} HTTP/1.1\r\n", config.method, config.path);
    req.push_str(&format!("Host: {}:{}\r\n", config.host, config.port));
    req.push_str("Connection: keep-alive\r\n");

    for (name, value) in &config.headers {
        req.push_str(&format!("{name}: {value}\r\n"));
    }

    if let Some(body) = &config.body {
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
        req.push_str("\r\n");
        req.push_str(body);
    } else {
        req.push_str("\r\n");
    }

    req.into_bytes()
}

/// Parse result from httparse.
pub struct ParsedResponse {
    pub status: u16,
    pub header_len: usize,
    pub content_length: usize,
    pub keep_alive: bool,
    pub chunked: bool,
}

/// Parse HTTP response headers. Returns None if incomplete.
pub fn parse_response(buf: &[u8]) -> Option<ParsedResponse> {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut resp = httparse::Response::new(&mut headers);

    match resp.parse(buf) {
        Ok(httparse::Status::Complete(header_len)) => {
            let status = resp.code.unwrap_or(0);
            let mut content_length = 0usize;
            let mut keep_alive = true;
            let mut chunked = false;

            for h in resp.headers.iter() {
                let name = h.name.to_lowercase();
                let val = std::str::from_utf8(h.value).unwrap_or("");
                match name.as_str() {
                    "content-length" => content_length = val.parse().unwrap_or(0),
                    "connection" => keep_alive = !val.eq_ignore_ascii_case("close"),
                    "transfer-encoding" => chunked = val.eq_ignore_ascii_case("chunked"),
                    _ => {}
                }
            }

            Some(ParsedResponse { status, header_len, content_length, keep_alive, chunked })
        }
        _ => None,
    }
}

/// Find the end of a chunked body. Returns total body length if complete.
pub fn find_chunked_end(body: &[u8]) -> Option<usize> {
    // Look for the terminating 0\r\n\r\n
    if body.len() >= 5 {
        for i in 0..body.len() - 4 {
            if &body[i..i + 5] == b"0\r\n\r\n" {
                return Some(i + 5);
            }
        }
    }
    None
}
```

- [ ] **Step 2: Verify**

```bash
cargo check -p zerobench
```

- [ ] **Step 3: Commit**

```bash
git add tools/bench/src/http.rs
git commit -m "feat(zerobench): HTTP/1.1 request builder + response parser (httparse)"
```

---

### Task 5: NUMA detection and CPU pinning

**Files:**
- Create: `tools/bench/src/numa.rs`

- [ ] **Step 1: Create numa.rs**

```rust
/// Pin the current thread to a specific CPU.
#[cfg(target_os = "linux")]
pub fn pin_to_cpu(cpu: usize) {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
    }
}

#[cfg(not(target_os = "linux"))]
pub fn pin_to_cpu(_cpu: usize) {
    // CPU pinning not available on this platform
}

/// Get the list of CPUs on a given NUMA node.
#[cfg(target_os = "linux")]
pub fn cpus_for_numa_node(node: usize) -> Vec<usize> {
    let path = format!("/sys/devices/system/node/node{node}/cpulist");
    match std::fs::read_to_string(&path) {
        Ok(content) => parse_cpu_list(content.trim()),
        Err(_) => Vec::new(),
    }
}

#[cfg(not(target_os = "linux"))]
pub fn cpus_for_numa_node(_node: usize) -> Vec<usize> {
    Vec::new()
}

/// Parse a CPU list like "0-7,16-23" into a Vec of CPU IDs.
fn parse_cpu_list(s: &str) -> Vec<usize> {
    let mut cpus = Vec::new();
    for part in s.split(',') {
        let part = part.trim();
        if let Some((start, end)) = part.split_once('-') {
            let s: usize = start.parse().unwrap_or(0);
            let e: usize = end.parse().unwrap_or(s);
            cpus.extend(s..=e);
        } else if let Ok(n) = part.parse() {
            cpus.push(n);
        }
    }
    cpus
}

/// Resolve CPU list for the benchmark threads.
/// Priority: --cpu > --numa > all CPUs.
pub fn resolve_cpus(cpu_affinity: &Option<Vec<usize>>, numa_node: &Option<usize>, threads: usize) -> Vec<usize> {
    let cpus = if let Some(cpus) = cpu_affinity {
        cpus.clone()
    } else if let Some(node) = numa_node {
        cpus_for_numa_node(*node)
    } else {
        return (0..threads).collect(); // no pinning, just assign sequentially
    };

    if cpus.is_empty() {
        (0..threads).collect()
    } else {
        // Distribute threads across available CPUs (round-robin)
        (0..threads).map(|i| cpus[i % cpus.len()]).collect()
    }
}
```

- [ ] **Step 2: Verify**

```bash
cargo check -p zerobench
```

- [ ] **Step 3: Commit**

```bash
git add tools/bench/src/numa.rs
git commit -m "feat(zerobench): NUMA detection + CPU pinning via sched_setaffinity"
```

---

### Task 6: Worker thread (compio event loop + connections)

**Files:**
- Create: `tools/bench/src/thread.rs`
- Modify: `tools/bench/src/main.rs`

- [ ] **Step 1: Create thread.rs**

```rust
use std::time::{Duration, Instant};

use compio::net::TcpStream;
use compio::BufResult;

use crate::config::Config;
use crate::http;
use crate::stats::ThreadStats;

/// Run one worker thread: connect N sockets, send requests in a loop until duration expires.
pub fn run_worker(config: &Config, thread_id: usize, cpu: usize) -> ThreadStats {
    crate::numa::pin_to_cpu(cpu);

    compio::runtime::RuntimeBuilder::new()
        .build()
        .unwrap()
        .block_on(async {
            worker_loop(config, thread_id).await
        })
}

async fn worker_loop(config: &Config, _thread_id: usize) -> ThreadStats {
    let mut stats = ThreadStats::new();
    let conns_per_thread = config.connections_per_thread();
    let request_bytes = http::build_request(config);
    let deadline = Instant::now() + config.duration;
    let addr = format!("{}:{}", config.host, config.port);

    // Open connections
    let mut connections: Vec<Option<TcpStream>> = Vec::with_capacity(conns_per_thread);
    for _ in 0..conns_per_thread {
        match TcpStream::connect(&addr).await {
            Ok(stream) => connections.push(Some(stream)),
            Err(_) => {
                stats.errors_connect += 1;
                connections.push(None);
            }
        }
    }

    // Request loop: round-robin across connections
    let mut buf = vec![0u8; 65536];
    let mut conn_idx = 0;

    while Instant::now() < deadline {
        if connections[conn_idx].is_none() {
            // Reconnect
            match TcpStream::connect(&addr).await {
                Ok(stream) => connections[conn_idx] = Some(stream),
                Err(_) => {
                    stats.errors_connect += 1;
                    conn_idx = (conn_idx + 1) % conns_per_thread;
                    continue;
                }
            }
        }

        let stream = connections[conn_idx].as_mut().unwrap();
        let start = Instant::now();

        // Send request
        let BufResult(write_result, _) = stream.write_all(request_bytes.clone()).await;
        if write_result.is_err() {
            stats.errors_write += 1;
            connections[conn_idx] = None;
            conn_idx = (conn_idx + 1) % conns_per_thread;
            continue;
        }

        // Read response
        let mut total_read = 0usize;
        let mut parsed: Option<http::ParsedResponse> = None;

        loop {
            let BufResult(read_result, returned_buf) = stream.read(buf.split_off(total_read)).await;
            buf = returned_buf;
            match read_result {
                Ok(0) => {
                    connections[conn_idx] = None;
                    break;
                }
                Ok(n) => {
                    total_read += n;

                    if parsed.is_none() {
                        parsed = http::parse_response(&buf[..total_read]);
                    }

                    if let Some(ref p) = parsed {
                        let body_start = p.header_len;
                        let body_received = total_read - body_start;

                        if p.chunked {
                            if http::find_chunked_end(&buf[body_start..total_read]).is_some() {
                                break;
                            }
                        } else if body_received >= p.content_length {
                            break;
                        }
                    }
                }
                Err(_) => {
                    stats.errors_read += 1;
                    connections[conn_idx] = None;
                    break;
                }
            }
        }

        let elapsed = start.elapsed();

        if let Some(ref p) = parsed {
            stats.record_latency(elapsed.as_micros() as u64);
            stats.record_request(total_read as u64);

            if p.status >= 400 {
                stats.errors_status += 1;
            }

            if !p.keep_alive {
                connections[conn_idx] = None;
            }
        }

        // Reset buffer for next request
        buf.clear();
        buf.resize(65536, 0);

        conn_idx = (conn_idx + 1) % conns_per_thread;
    }

    stats
}
```

- [ ] **Step 2: Update main.rs with thread spawning**

```rust
mod config;
mod http;
mod numa;
mod stats;
mod thread;

use config::Config;
use stats::{Summary, ThreadStats};

fn main() {
    let config = Config::from_args();
    let cpus = numa::resolve_cpus(&config.cpu_affinity, &config.numa_node, config.threads);

    eprintln!("Running {:?} test @ {}", config.duration, config.url);
    eprintln!("  {} threads and {} connections", config.threads, config.connections);

    let config_ref = &config;
    let start = std::time::Instant::now();

    // Spawn worker threads
    let handles: Vec<_> = (0..config.threads)
        .map(|i| {
            let cfg = Config::from_args(); // re-parse per thread (Config is not Send-safe with compio)
            let cpu = cpus[i];
            std::thread::spawn(move || thread::run_worker(&cfg, i, cpu))
        })
        .collect();

    // Collect results
    let thread_stats: Vec<ThreadStats> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let duration = start.elapsed();
    let summary = Summary::merge(thread_stats, duration);

    // Report
    report::print_wrk_format(&summary, &config);
}

mod report;
```

- [ ] **Step 3: Verify it compiles**

```bash
cargo check -p zerobench
```

- [ ] **Step 4: Commit**

```bash
git add tools/bench/src/
git commit -m "feat(zerobench): worker thread — compio event loop, connection pool, request cycle"
```

---

### Task 7: wrk-compatible report

**Files:**
- Create: `tools/bench/src/report.rs`

- [ ] **Step 1: Create report.rs**

```rust
use crate::config::Config;
use crate::stats::Summary;

pub fn print_wrk_format(summary: &Summary, config: &Config) {
    let dur_secs = summary.duration.as_secs_f64();
    let rps = summary.requests_per_sec();
    let bps = summary.bytes_per_sec();

    // Latency stats
    let lat_avg = summary.latency.mean();
    let lat_stdev = summary.latency.stdev();
    let lat_max = summary.latency.max() as f64;

    println!("  Thread Stats   Avg      Stdev     Max    +/- Stdev");
    println!("    Latency   {}  {}  {}    {:.2}%",
        format_time(lat_avg),
        format_time(lat_stdev),
        format_time(lat_max),
        within_stdev_pct(&summary.latency),
    );
    println!("    Req/Sec   {:.2}k", rps / 1000.0 / config.threads as f64);

    if config.print_latency {
        println!("  Latency Distribution");
        for pct in [50.0, 75.0, 90.0, 99.0, 99.9] {
            let val = summary.latency.value_at_percentile(pct) as f64;
            println!("    {:>5.1}%    {}", pct, format_time(val));
        }
    }

    let total_errors = summary.total_errors();
    println!("  {} requests in {:.2}s, {} read",
        format_count(summary.requests),
        dur_secs,
        format_bytes(summary.bytes),
    );

    if total_errors > 0 {
        println!("  Socket errors: connect {}, read {}, write {}, timeout {}",
            summary.errors_connect, summary.errors_read, summary.errors_write, summary.errors_timeout);
        if summary.errors_status > 0 {
            println!("  Non-2xx or 3xx responses: {}", summary.errors_status);
        }
    }

    println!("Requests/sec: {:.2}", rps);
    println!("Transfer/sec: {}", format_bytes_rate(bps));

    if config.json_output {
        let json = serde_json::json!({
            "requests": summary.requests,
            "bytes": summary.bytes,
            "duration_ms": summary.duration.as_millis(),
            "requests_per_sec": rps,
            "bytes_per_sec": bps,
            "latency_avg_us": lat_avg,
            "latency_max_us": lat_max,
            "latency_p50_us": summary.latency.value_at_percentile(50.0),
            "latency_p99_us": summary.latency.value_at_percentile(99.0),
            "errors": summary.total_errors(),
        });
        println!("\n{}", serde_json::to_string_pretty(&json).unwrap());
    }
}

fn format_time(micros: f64) -> String {
    if micros < 1000.0 {
        format!("{:.2}us", micros)
    } else if micros < 1_000_000.0 {
        format!("{:.2}ms", micros / 1000.0)
    } else {
        format!("{:.2}s", micros / 1_000_000.0)
    }
}

fn format_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.2}k", n as f64 / 1_000.0)
    } else {
        format!("{n}")
    }
}

fn format_bytes(n: u64) -> String {
    if n >= 1_073_741_824 {
        format!("{:.2}GB", n as f64 / 1_073_741_824.0)
    } else if n >= 1_048_576 {
        format!("{:.2}MB", n as f64 / 1_048_576.0)
    } else if n >= 1024 {
        format!("{:.2}KB", n as f64 / 1024.0)
    } else {
        format!("{n}B")
    }
}

fn format_bytes_rate(bps: f64) -> String {
    format!("{}/s", format_bytes(bps as u64))
}

fn within_stdev_pct(h: &hdrhistogram::Histogram<u64>) -> f64 {
    let mean = h.mean();
    let stdev = h.stdev();
    let low = (mean - stdev).max(0.0) as u64;
    let high = (mean + stdev) as u64;
    let within = h.count_between(low, high);
    (within as f64 / h.len() as f64) * 100.0
}
```

- [ ] **Step 2: Add serde + serde_json to Cargo.toml for JSON output**

Add to `tools/bench/Cargo.toml` dependencies:
```toml
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

- [ ] **Step 3: Verify**

```bash
cargo check -p zerobench
```

- [ ] **Step 4: Commit**

```bash
git add tools/bench/
git commit -m "feat(zerobench): wrk-compatible text report + optional JSON output"
```

---

### Task 8: Integration test — benchmark against echo server

- [ ] **Step 1: Build release**

```bash
cargo build --release --bin zerobench --bin echo-server
```

- [ ] **Step 2: Run against echo server**

```bash
# Start echo server
./target/release/echo-server 8888 &

# Run zerobench
./target/release/zerobench -t4 -c50 -d5s http://localhost:8888/

# Kill echo server
kill %1
```

Expected: wrk-compatible output with req/s, latency, bytes transferred.

- [ ] **Step 3: Compare with wrk**

```bash
./target/release/echo-server 8888 &

# wrk baseline
wrk -t4 -c50 -d5s http://localhost:8888/

# zerobench
./target/release/zerobench -t4 -c50 -d5s http://localhost:8888/

kill %1
```

Results should be within 20% of wrk for the same workload.

- [ ] **Step 4: Test NUMA pinning**

```bash
./target/release/echo-server 8888 &
./target/release/zerobench -t4 -c50 -d5s --cpu 0-3 http://localhost:8888/
kill %1
```

- [ ] **Step 5: Test JSON output**

```bash
./target/release/echo-server 8888 &
./target/release/zerobench -t4 -c50 -d5s --json http://localhost:8888/
kill %1
```

- [ ] **Step 6: Commit results**

```bash
git add -A
git commit -m "feat(zerobench): Phase 1 complete — wrk-compatible HTTP benchmark with NUMA"
```
