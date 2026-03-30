use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

use crate::cpu_timer::{CpuLimits, CpuUsage};
use crate::v8::{create_v8_runtime, handle_rpc, Plugin};

/// Request sent from HTTP layer to an isolate worker.
pub struct IsolateRequest {
    pub body: String,
    pub reply: oneshot::Sender<Result<IsolateResponse, String>>,
}

/// Response from an isolate worker.
pub struct IsolateResponse {
    pub json: String,
    pub cpu_time_ms: f64,
    pub total_cpu_ms: f64,
    pub request_count: u64,
}

/// Factory that creates plugins for a given app.
/// Called on the isolate's thread when a new worker is spawned.
pub type PluginFactory = Arc<dyn Fn(&str) -> Vec<Box<dyn Plugin>> + Send + Sync>;

/// Configuration for the isolate pool.
#[derive(Clone)]
pub struct PoolConfig {
    pub max_isolates: usize,
    pub idle_timeout: Duration,
    pub cpu_limits: CpuLimits,
    pub plugin_factory: PluginFactory,
}

/// A running isolate worker.
struct IsolateWorker {
    tx: mpsc::Sender<IsolateRequest>,
    last_used: Instant,
    cpu_usage: Arc<Mutex<CpuUsage>>,
    _thread: std::thread::JoinHandle<()>,
}

/// Pool of V8 isolate workers, one per app.
pub struct IsolatePool {
    workers: Arc<Mutex<HashMap<String, IsolateWorker>>>,
    config: PoolConfig,
}

impl IsolatePool {
    pub fn new(config: PoolConfig) -> Self {
        let pool = Self {
            workers: Arc::new(Mutex::new(HashMap::new())),
            config,
        };

        // Spawn eviction task
        let workers = pool.workers.clone();
        let idle_timeout = pool.config.idle_timeout;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                evict_idle(&workers, idle_timeout);
            }
        });

        pool
    }

    /// Send an RPC request to the isolate for the given app.
    /// Creates a new isolate if one doesn't exist.
    pub async fn dispatch(
        &self,
        app_id: &str,
        server_js: &str,
        body: String,
    ) -> Result<IsolateResponse, String> {
        let tx = self.get_or_create(app_id, server_js)?;

        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(IsolateRequest {
            body,
            reply: reply_tx,
        })
        .await
        .map_err(|_| "Isolate worker channel closed".to_string())?;

        reply_rx
            .await
            .map_err(|_| "Isolate worker dropped reply".to_string())?
    }

    fn get_or_create(
        &self,
        app_id: &str,
        server_js: &str,
    ) -> Result<mpsc::Sender<IsolateRequest>, String> {
        let mut workers = self.workers.lock().unwrap();

        if let Some(worker) = workers.get_mut(app_id) {
            if !worker.tx.is_closed() {
                worker.last_used = Instant::now();
                return Ok(worker.tx.clone());
            }
            workers.remove(app_id);
        }

        if workers.len() >= self.config.max_isolates {
            let oldest = workers
                .iter()
                .min_by_key(|(_, w)| w.last_used)
                .map(|(k, _)| k.clone());
            if let Some(key) = oldest {
                eprintln!("[pool] Evicting idle app: {key}");
                workers.remove(&key);
            }
        }

        let (tx, rx) = mpsc::channel::<IsolateRequest>(64);
        let cpu_usage = Arc::new(Mutex::new(CpuUsage::default()));
        let cpu_usage_clone = cpu_usage.clone();
        let server_js = server_js.to_string();
        let app_id_str = app_id.to_string();
        let cpu_limits = self.config.cpu_limits;
        let plugin_factory = self.config.plugin_factory.clone();

        let thread = std::thread::Builder::new()
            .name(format!("v8-{app_id}"))
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(isolate_worker(
                    &app_id_str,
                    &server_js,
                    rx,
                    cpu_limits,
                    cpu_usage_clone,
                    plugin_factory,
                ));
            })
            .map_err(|e| format!("Failed to spawn V8 thread: {e}"))?;

        eprintln!("[pool] Started isolate for app: {app_id}");

        workers.insert(
            app_id.to_string(),
            IsolateWorker {
                tx: tx.clone(),
                last_used: Instant::now(),
                cpu_usage,
                _thread: thread,
            },
        );

        Ok(tx)
    }

    pub fn stats(&self) -> PoolStats {
        let workers = self.workers.lock().unwrap();
        let mut apps = Vec::new();

        for (id, worker) in workers.iter() {
            let usage = worker.cpu_usage.lock().unwrap();
            apps.push(AppStats {
                app_id: id.clone(),
                idle_secs: worker.last_used.elapsed().as_secs_f64(),
                total_cpu_ms: usage.total.as_secs_f64() * 1000.0,
                request_count: usage.request_count,
            });
        }

        PoolStats {
            active_isolates: workers.len(),
            max_isolates: self.config.max_isolates,
            apps,
        }
    }
}

#[derive(Debug, serde::Serialize)]
pub struct PoolStats {
    pub active_isolates: usize,
    pub max_isolates: usize,
    pub apps: Vec<AppStats>,
}

#[derive(Debug, serde::Serialize)]
pub struct AppStats {
    pub app_id: String,
    pub idle_secs: f64,
    pub total_cpu_ms: f64,
    pub request_count: u64,
}

fn evict_idle(
    workers: &Arc<Mutex<HashMap<String, IsolateWorker>>>,
    idle_timeout: Duration,
) {
    let mut workers = workers.lock().unwrap();
    let before = workers.len();

    workers.retain(|id, worker| {
        if worker.last_used.elapsed() > idle_timeout {
            eprintln!("[pool] Evicting idle app: {id}");
            false
        } else {
            true
        }
    });

    let evicted = before - workers.len();
    if evicted > 0 {
        eprintln!(
            "[pool] Evicted {evicted} idle isolates, {} active",
            workers.len()
        );
    }
}

async fn isolate_worker(
    app_id: &str,
    server_js: &str,
    mut rx: mpsc::Receiver<IsolateRequest>,
    cpu_limits: CpuLimits,
    cpu_usage: Arc<Mutex<CpuUsage>>,
    plugin_factory: PluginFactory,
) {
    // Create plugins for this app
    let plugins = plugin_factory(app_id);

    let (mut runtime, rpc_result) = match create_v8_runtime(&plugins) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[pool] [{app_id}] Failed to create V8 runtime: {e}");
            return;
        }
    };

    if !server_js.is_empty() {
        if let Err(e) = runtime.execute_script("<server>", server_js.to_string()) {
            eprintln!("[pool] [{app_id}] Failed to execute server.js: {e}");
            return;
        }
        if let Err(e) = runtime.run_event_loop(Default::default()).await {
            eprintln!("[pool] [{app_id}] Event loop error: {e}");
            return;
        }
    }

    eprintln!("[pool] [{app_id}] Isolate ready");

    while let Some(req) = rx.recv().await {
        let result =
            handle_rpc(&mut runtime, &rpc_result, &req.body, &cpu_limits).await;

        let reply = match result {
            Ok(rpc_resp) => {
                let mut usage = cpu_usage.lock().unwrap();
                usage.record(rpc_resp.cpu_time);
                Ok(IsolateResponse {
                    json: rpc_resp.json,
                    cpu_time_ms: rpc_resp.cpu_time.as_secs_f64() * 1000.0,
                    total_cpu_ms: usage.total.as_secs_f64() * 1000.0,
                    request_count: usage.request_count,
                })
            }
            Err(e) => Err(e.to_string()),
        };

        let _ = req.reply.send(reply);
    }

    eprintln!("[pool] [{app_id}] Isolate shutting down");
}
