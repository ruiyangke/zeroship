//! Long-running stress test against the `zeroship-sandbox` k8s+libkrun
//! backend. Drives N concurrent sessions through a realistic workload
//! mix for a configurable duration and emits per-op latency stats.
//!
//! What it exercises that `lifecycle_e2e.rs` doesn't:
//!
//!   - **N sessions in parallel** — Pod scheduling, port-forward fan-out,
//!     ConfigMap/Pod cleanup at scale.
//!   - **Sustained throughput** — agent's nonce LRU under steady traffic
//!     (10 000-entry cap @ 30 s TTL = ~333 RPS sustained per session
//!     before legit nonces start aging out).
//!   - **Heavy bodies** — periodic 100 KiB file writes / reads cover
//!     ntex's payload limits and pipe-readers in the agent.
//!   - **Real subprocess churn** — node/python/sh invocations exercise
//!     the dropuser pre_exec lockdown + reaper-routed wait under load.
//!   - **Long uptime** — over a multi-minute run we'd notice memory
//!     growth, port-forward attrition, or reaper-down events.
//!
//! Configurable via env (defaults shown):
//!
//!     SBX_STRESS_SESSIONS=5           # concurrent sessions
//!     SBX_STRESS_DURATION_SECS=180    # total runtime
//!     SBX_STRESS_OPS_PER_SECOND=2     # per-session ops/sec
//!     SBX_STRESS_HEAVY_EVERY=25       # 1 in N ops is "heavy"
//!     SBX_STRESS_NAMESPACE=default
//!
//! Usage:
//!     cargo run --release -p zeroship-sandbox --example stress_e2e
//!
//! Pre-reqs: same as lifecycle_e2e. The agent image must include
//! node + python (the `docker/agent-runtime/Dockerfile` does).

#![allow(unsafe_code)]

use std::env;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use uuid::Uuid;
use zeroship_sandbox::backend::Backend;
use zeroship_sandbox::config::SandboxConfig;

const G: &str = "\x1b[32m";
const R: &str = "\x1b[31m";
const Y: &str = "\x1b[33m";
const Z: &str = "\x1b[0m";

#[compio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("{R}STRESS E2E FAILED: {e}{Z}");
        std::process::exit(1);
    }
}

struct StressConfig {
    sessions: usize,
    duration_secs: u64,
    ops_per_second: u64,
    heavy_every: u64,
}

impl StressConfig {
    fn from_env() -> Self {
        Self {
            sessions: env_usize("SBX_STRESS_SESSIONS", 5),
            duration_secs: env_u64("SBX_STRESS_DURATION_SECS", 180),
            ops_per_second: env_u64("SBX_STRESS_OPS_PER_SECOND", 2),
            heavy_every: env_u64("SBX_STRESS_HEAVY_EVERY", 25),
        }
    }
}

async fn run() -> Result<(), String> {
    let stress = StressConfig::from_env();
    println!("{Y}== zeroship-sandbox stress test (k8s + libkrun) =={Z}");
    println!("sessions:        {}", stress.sessions);
    println!("duration:        {}s", stress.duration_secs);
    println!("ops/session/sec: {}", stress.ops_per_second);
    println!("heavy every:     1/{}", stress.heavy_every);
    let total_target = stress.sessions as u64 * stress.duration_secs * stress.ops_per_second;
    println!("target ops:      {total_target}");
    println!();

    set_default("SANDBOX_BACKEND", "k8s");
    set_default("SANDBOX_K8S_USE_PORT_FORWARD", "true");
    if let Ok(ns) = env::var("SBX_STRESS_NAMESPACE") {
        unsafe { env::set_var("SANDBOX_K8S_NAMESPACE", ns); }
    }

    let config = SandboxConfig::from_env().map_err(|e| format!("config: {e}"))?;
    let backend = Arc::new(Backend::from_config(&config)?);
    backend.probe().await?;

    let stats = Arc::new(Stats::new());
    let stop = Arc::new(AtomicBool::new(false));

    // ── spin up workers ─────────────────────────────────────────
    // compio's `spawn` requires `Output = ()`, so workers handle
    // their own errors internally and push them onto `worker_errors`.
    let started_all = Instant::now();
    let worker_errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let mut handles = Vec::new();
    for n in 0..stress.sessions {
        let backend = backend.clone();
        let stats = stats.clone();
        let stop = stop.clone();
        let errs = worker_errors.clone();
        let project_id = format!("stress-{}-{}", n, Uuid::new_v4().simple());
        let cfg = WorkerConfig {
            ops_per_second: stress.ops_per_second,
            heavy_every: stress.heavy_every,
        };
        let handle = compio::runtime::spawn(async move {
            if let Err(e) = run_worker(n, project_id, backend, stats, stop, cfg).await {
                errs.lock().unwrap().push(format!("worker {n}: {e}"));
            }
        });
        handles.push(handle);
        // Stagger session starts so we don't slam the k8s scheduler
        // with N concurrent Pod creations.
        compio::time::sleep(Duration::from_millis(750)).await;
    }

    // ── progress reporter ───────────────────────────────────────
    let stats_for_progress = stats.clone();
    let stop_for_progress = stop.clone();
    let progress = compio::runtime::spawn(async move {
        let started = Instant::now();
        loop {
            compio::time::sleep(Duration::from_secs(10)).await;
            if stop_for_progress.load(Ordering::Relaxed) {
                break;
            }
            let elapsed = started.elapsed().as_secs();
            let snap = stats_for_progress.snapshot();
            println!(
                "  [{elapsed:>3}s] ops={} (write={} read={} delete={} tree={} exec={} heavy={}) errors={}",
                snap.total, snap.write, snap.read, snap.delete, snap.tree, snap.exec, snap.heavy, snap.errors,
            );
        }
    });

    // ── run for the target duration ─────────────────────────────
    compio::time::sleep(Duration::from_secs(stress.duration_secs)).await;
    println!("{Y}-- duration reached; signalling workers --{Z}");
    stop.store(true, Ordering::Relaxed);

    // ── wait for workers ────────────────────────────────────────
    // The Task type's await returns a JoinHandle-style Result whose
    // Err carries a panic payload (compio re-uses the std::any::Any
    // shape). Bubble up any panics as worker errors.
    for h in handles {
        if let Err(panic) = h.await {
            worker_errors
                .lock()
                .unwrap()
                .push(format!("worker panicked: {panic:?}"));
        }
    }
    let _ = progress.await;
    let errors = worker_errors.lock().unwrap().clone();

    // ── final report ────────────────────────────────────────────
    let snap = stats.snapshot();
    let elapsed = started_all.elapsed().as_secs_f64();
    println!();
    println!("{Y}== run complete ({elapsed:.1}s wall-clock) =={Z}");
    println!("total ops:    {}", snap.total);
    println!("  write:      {}", snap.write);
    println!("  read:       {}", snap.read);
    println!("  delete:     {}", snap.delete);
    println!("  tree:       {}", snap.tree);
    println!("  exec:       {}", snap.exec);
    println!("  heavy:      {}", snap.heavy);
    println!("errors:       {}", snap.errors);
    if snap.total > 0 {
        let rate = snap.total as f64 / elapsed;
        println!("rate:         {rate:.2} ops/s");
        println!("error rate:   {:.4}%", 100.0 * snap.errors as f64 / snap.total as f64);
    }
    println!();
    println!("latency (ms):");
    print_lat("  write", &stats.write_lat);
    print_lat("  read ", &stats.read_lat);
    print_lat("  tree ", &stats.tree_lat);
    print_lat("  exec ", &stats.exec_lat);
    print_lat("  heavy", &stats.heavy_lat);

    if !errors.is_empty() {
        eprintln!();
        eprintln!("{R}== {} worker(s) reported errors =={Z}", errors.len());
        for e in &errors {
            eprintln!("  - {e}");
        }
        return Err(format!("{} worker error(s)", errors.len()));
    }

    if snap.errors > 0 {
        return Err(format!(
            "{} op errors (acceptable rate is 0; see worker logs)",
            snap.errors,
        ));
    }

    println!();
    println!("{G}== STRESS TEST PASSED =={Z}");
    Ok(())
}

// ─── worker ──────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct WorkerConfig {
    ops_per_second: u64,
    heavy_every: u64,
}

async fn run_worker(
    worker_id: usize,
    project_id: String,
    backend: Arc<Backend>,
    stats: Arc<Stats>,
    stop: Arc<AtomicBool>,
    cfg: WorkerConfig,
) -> Result<(), String> {
    let session_id = Uuid::new_v4();

    // Create — this is the slow path; up to ~15s on a slow node.
    let create_started = Instant::now();
    let info = backend
        .create(session_id, &project_id)
        .await
        .map_err(|e| format!("worker {worker_id}: create: {e}"))?;
    println!(
        "  worker {worker_id}: session up in {:?} ({})",
        create_started.elapsed(),
        info.backend_hint
    );

    // Per-op interval. ops_per_second=2 → 500ms between ops.
    let interval_ms = (1000 / cfg.ops_per_second.max(1)).max(50);
    let mut op_n: u64 = 0;
    let mut written: Vec<String> = Vec::new(); // keep around for read/delete

    while !stop.load(Ordering::Relaxed) {
        op_n += 1;
        let heavy = op_n % cfg.heavy_every == 0;

        let op_started = Instant::now();
        let result: Result<(), String> = if heavy {
            run_heavy_op(&backend, session_id, op_n).await
        } else {
            run_light_op(&backend, session_id, op_n, &mut written, &stats).await
        };

        let dt = op_started.elapsed().as_millis() as u64;
        if heavy {
            stats.heavy.fetch_add(1, Ordering::Relaxed);
            stats.heavy_lat.lock().unwrap().push(dt);
        }
        stats.total.fetch_add(1, Ordering::Relaxed);

        if let Err(e) = result {
            stats.errors.fetch_add(1, Ordering::Relaxed);
            eprintln!("  {R}worker {worker_id} op {op_n}: {e}{Z}");
        }

        compio::time::sleep(Duration::from_millis(interval_ms)).await;
    }

    // Stop — best-effort; report if it fails but don't fail the worker.
    if let Err(e) = backend.stop(session_id).await {
        eprintln!("  worker {worker_id}: stop failed: {e}");
    }
    Ok(())
}

/// Light ops: 70% file CRUD, 20% exec, 10% tree. The mix mirrors a
/// realistic AI-builder session (lots of small file edits, occasional
/// shell command, occasional list).
async fn run_light_op(
    backend: &Arc<Backend>,
    session_id: Uuid,
    op_n: u64,
    written: &mut Vec<String>,
    stats: &Arc<Stats>,
) -> Result<(), String> {
    // Cheap "random" derived from op_n; deterministic across runs for
    // easier debugging when something flakes.
    let bucket = op_n % 10;
    let started = Instant::now();
    match bucket {
        0..=4 => {
            // 50% writes
            let path = format!("src/file_{op_n}.txt");
            let body = format!("op_n={op_n} ts={}", unix_now());
            backend.write_file(session_id, &path, body.as_bytes()).await?;
            stats.write.fetch_add(1, Ordering::Relaxed);
            stats.write_lat.lock().unwrap().push(started.elapsed().as_millis() as u64);
            written.push(path);
            // Bound memory of the per-worker history.
            if written.len() > 256 {
                written.drain(..64);
            }
        }
        5..=6 => {
            // 20% reads (only if we have something to read)
            if let Some(p) = written.last().cloned() {
                let _ = backend.read_file(session_id, &p).await?;
                stats.read.fetch_add(1, Ordering::Relaxed);
                stats.read_lat.lock().unwrap().push(started.elapsed().as_millis() as u64);
            }
        }
        7 => {
            // 10% deletes — pick a random older file
            if written.len() > 5 {
                let idx = (op_n as usize) % (written.len() - 1);
                let p = written.remove(idx);
                let _ = backend.delete_file(session_id, &p).await?;
                stats.delete.fetch_add(1, Ordering::Relaxed);
            }
        }
        8 => {
            // 10% tree
            let _ = backend.file_tree(session_id).await?;
            stats.tree.fetch_add(1, Ordering::Relaxed);
            stats.tree_lat.lock().unwrap().push(started.elapsed().as_millis() as u64);
        }
        _ => {
            // 10% exec — quick shell
            let out = backend
                .exec(session_id, "echo ok && date +%s", None, Some(5_000))
                .await?;
            if out.status != 0 {
                return Err(format!("exec exited {} stderr={}", out.status, out.stderr));
            }
            stats.exec.fetch_add(1, Ordering::Relaxed);
            stats.exec_lat.lock().unwrap().push(started.elapsed().as_millis() as u64);
        }
    }
    Ok(())
}

/// Heavy ops: deeper exercises, run periodically. These are the
/// shapes a real AI workload would produce — bigger files, real
/// interpreters, larger output.
async fn run_heavy_op(
    backend: &Arc<Backend>,
    session_id: Uuid,
    op_n: u64,
) -> Result<(), String> {
    let kind = (op_n / 13) % 5;
    match kind {
        0 => {
            // 100 KiB write + read roundtrip — bigger payload than
            // normal traffic, exercises the agent's body buffer + the
            // kubectl-port-forward proxy.
            let path = format!("blobs/blob_{op_n}.bin");
            let body = vec![b'A'; 100 * 1024];
            backend.write_file(session_id, &path, &body).await?;
            let read = backend.read_file(session_id, &path).await?;
            if read.len() != body.len() {
                return Err(format!(
                    "blob roundtrip size mismatch: wrote {} got {}",
                    body.len(),
                    read.len()
                ));
            }
            backend.delete_file(session_id, &path).await?;
        }
        1 => {
            // Node startup. Confirms the in-VM image has Node and
            // the privilege-dropped child can fork+exec it.
            let out = backend
                .exec(
                    session_id,
                    "node -e 'console.log(\"node-ok-\"+process.pid)'",
                    None,
                    Some(15_000),
                )
                .await?;
            if out.status != 0 || !out.stdout.contains("node-ok-") {
                return Err(format!(
                    "node smoke failed: status={} stdout={:?} stderr={:?}",
                    out.status, out.stdout, out.stderr
                ));
            }
        }
        2 => {
            // Python startup.
            let out = backend
                .exec(
                    session_id,
                    "python3 -c 'import os; print(\"py-ok-\"+str(os.getpid()))'",
                    None,
                    Some(15_000),
                )
                .await?;
            if out.status != 0 || !out.stdout.contains("py-ok-") {
                return Err(format!(
                    "python smoke failed: status={} stdout={:?}",
                    out.status, out.stdout
                ));
            }
        }
        3 => {
            // Filesystem-heavy: write a directory of small files, then
            // run `find | wc -l` to verify they're all there. Mirrors
            // a small npm install.
            let n = 50;
            for i in 0..n {
                let p = format!("modules/m_{op_n}/file_{i}.txt");
                backend
                    .write_file(session_id, &p, format!("file_{i}").as_bytes())
                    .await?;
            }
            let out = backend
                .exec(
                    session_id,
                    &format!("find modules/m_{op_n} -type f | wc -l"),
                    None,
                    Some(10_000),
                )
                .await?;
            let count: usize = out.stdout.trim().parse().unwrap_or(0);
            if count != n {
                return Err(format!(
                    "fs-fanout mismatch: expected {n} files, found {count}; stderr={:?}",
                    out.stderr
                ));
            }
        }
        _ => {
            // Stdout-heavy command — make sure we don't lose output
            // and the agent doesn't OOM under a big response body.
            let out = backend
                .exec(
                    session_id,
                    "yes | head -c 200000",
                    None,
                    Some(15_000),
                )
                .await?;
            // 200_000 bytes; agent caps stdout at 16 MiB so this is fine.
            if out.stdout.len() < 199_000 {
                return Err(format!("stdout-heavy got {} bytes (expected ~200k)", out.stdout.len()));
            }
        }
    }
    Ok(())
}

// ─── stats ────────────────────────────────────────────────────────

struct Stats {
    total: AtomicU64,
    write: AtomicU64,
    read: AtomicU64,
    delete: AtomicU64,
    tree: AtomicU64,
    exec: AtomicU64,
    heavy: AtomicU64,
    errors: AtomicU64,
    write_lat: Mutex<Vec<u64>>,
    read_lat: Mutex<Vec<u64>>,
    tree_lat: Mutex<Vec<u64>>,
    exec_lat: Mutex<Vec<u64>>,
    heavy_lat: Mutex<Vec<u64>>,
}

#[derive(Default)]
struct StatsSnap {
    total: u64,
    write: u64,
    read: u64,
    delete: u64,
    tree: u64,
    exec: u64,
    heavy: u64,
    errors: u64,
}

impl Stats {
    fn new() -> Self {
        Self {
            total: AtomicU64::new(0),
            write: AtomicU64::new(0),
            read: AtomicU64::new(0),
            delete: AtomicU64::new(0),
            tree: AtomicU64::new(0),
            exec: AtomicU64::new(0),
            heavy: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            write_lat: Mutex::new(Vec::new()),
            read_lat: Mutex::new(Vec::new()),
            tree_lat: Mutex::new(Vec::new()),
            exec_lat: Mutex::new(Vec::new()),
            heavy_lat: Mutex::new(Vec::new()),
        }
    }

    fn snapshot(&self) -> StatsSnap {
        StatsSnap {
            total: self.total.load(Ordering::Relaxed),
            write: self.write.load(Ordering::Relaxed),
            read: self.read.load(Ordering::Relaxed),
            delete: self.delete.load(Ordering::Relaxed),
            tree: self.tree.load(Ordering::Relaxed),
            exec: self.exec.load(Ordering::Relaxed),
            heavy: self.heavy.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
        }
    }
}

fn print_lat(label: &str, lat: &Mutex<Vec<u64>>) {
    let mut v = lat.lock().unwrap().clone();
    if v.is_empty() {
        println!("{label}  (no samples)");
        return;
    }
    v.sort_unstable();
    let n = v.len();
    let p = |q: f64| -> u64 {
        let idx = ((n as f64 - 1.0) * q).round() as usize;
        v[idx]
    };
    let avg = v.iter().sum::<u64>() / n as u64;
    println!(
        "{label}  n={n:<5} avg={avg:>4}  p50={:>4}  p95={:>5}  p99={:>5}  max={:>5}",
        p(0.50),
        p(0.95),
        p(0.99),
        v[n - 1],
    );
}

// ─── small utilities ──────────────────────────────────────────────

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn set_default(key: &str, value: &str) {
    if env::var_os(key).is_none() {
        unsafe { env::set_var(key, value); }
    }
}
