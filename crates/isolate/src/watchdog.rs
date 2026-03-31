//! Global watchdog — a single background thread that monitors ALL V8 isolates.
//!
//! Polls every 500ms. For each registered isolate with an active request, checks
//! wall time AND CPU time. Terminates V8 via `IsolateHandle::terminate_execution()`
//! if either limit is exceeded.
//!
//! Uses `pthread_getcpuclockid` to read the V8 thread's CPU time from the watchdog
//! thread (cross-thread measurement, Linux only).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use deno_core::v8;

/// Per-isolate entry tracked by the watchdog.
#[derive(Debug)]
pub struct WatchdogEntry {
    /// Thread-safe V8 termination handle.
    v8_handle: v8::IsolateHandle,
    /// CPU clock ID for the V8 actor thread (from pthread_getcpuclockid).
    #[cfg(target_os = "linux")]
    cpu_clock_id: libc::clockid_t,
    /// Wall-time limit per request.
    wall_limit: Duration,
    /// CPU-time limit per request.
    cpu_limit: Duration,
    /// Wall-time when the oldest active request started. 0 = no active request.
    /// Stored as epoch nanos from SystemTime.
    request_wall_start: AtomicU64,
    /// CPU time (nanos) of the V8 thread when the oldest active request started.
    request_cpu_start: AtomicU64,
    /// Number of in-flight requests.
    active_requests: AtomicU32,
}

// Safety: v8::IsolateHandle is Send (documented by V8). The atomics are Send+Sync.
// cpu_clock_id is a plain integer (clockid_t = i32). All fields are safe to share.
#[allow(unsafe_code)]
unsafe impl Send for WatchdogEntry {}
#[allow(unsafe_code)]
unsafe impl Sync for WatchdogEntry {}

/// Limits configuration (from plan).
#[derive(Debug, Clone, Copy)]
pub struct ExecutionLimits {
    pub wall_time: Duration,
    pub cpu_time: Duration,
}

impl Default for ExecutionLimits {
    fn default() -> Self {
        Self {
            wall_time: Duration::from_secs(30), // 30s wall
            cpu_time: Duration::from_secs(5),   // 5s CPU (generous default)
        }
    }
}

/// Global watchdog — 1 thread monitors all isolates.
#[derive(Debug)]
pub struct GlobalWatchdog {
    entries: Arc<Mutex<HashMap<String, Arc<WatchdogEntry>>>>,
    _thread: Option<std::thread::JoinHandle<()>>,
}

impl GlobalWatchdog {
    /// Create and start the global watchdog.
    pub fn new() -> Self {
        let entries: Arc<Mutex<HashMap<String, Arc<WatchdogEntry>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let entries_clone = entries.clone();

        let thread = std::thread::Builder::new()
            .name("watchdog".into())
            .spawn(move || {
                watchdog_loop(entries_clone);
            })
            .expect("Failed to spawn watchdog thread");

        Self {
            entries,
            _thread: Some(thread),
        }
    }

    /// Register an isolate for monitoring.
    pub fn register(
        &self,
        app_id: &str,
        v8_handle: v8::IsolateHandle,
        thread_id: libc::pthread_t,
        limits: ExecutionLimits,
    ) {
        let _thread_id = thread_id; // used on Linux only

        #[cfg(target_os = "linux")]
        let cpu_clock_id = {
            let mut clock_id: libc::clockid_t = 0;
            #[allow(unsafe_code)]
            unsafe { libc::pthread_getcpuclockid(_thread_id, &mut clock_id) };
            clock_id
        };

        let entry = Arc::new(WatchdogEntry {
            v8_handle,
            #[cfg(target_os = "linux")]
            cpu_clock_id,
            wall_limit: limits.wall_time,
            cpu_limit: limits.cpu_time,
            request_wall_start: AtomicU64::new(0),
            request_cpu_start: AtomicU64::new(0),
            active_requests: AtomicU32::new(0),
        });

        self.entries
            .lock()
            .unwrap()
            .insert(app_id.to_string(), entry);
    }

    /// Unregister an isolate.
    pub fn unregister(&self, app_id: &str) {
        self.entries.lock().unwrap().remove(app_id);
    }

    /// Get a handle for the actor to ping on request start/end.
    pub fn get_handle(&self, app_id: &str) -> Option<Arc<WatchdogEntry>> {
        self.entries.lock().unwrap().get(app_id).cloned()
    }

    /// Get a clone of the entries map Arc (used by pool entries for auto-unregister on drop).
    pub fn entries_ref(&self) -> Arc<Mutex<HashMap<String, Arc<WatchdogEntry>>>> {
        self.entries.clone()
    }
}

impl WatchdogEntry {
    /// Create a new watchdog entry.
    pub fn new(
        v8_handle: v8::IsolateHandle,
        #[cfg(target_os = "linux")] cpu_clock_id: libc::clockid_t,
        limits: ExecutionLimits,
    ) -> Self {
        Self {
            v8_handle,
            #[cfg(target_os = "linux")]
            cpu_clock_id,
            wall_limit: limits.wall_time,
            cpu_limit: limits.cpu_time,
            request_wall_start: AtomicU64::new(0),
            request_cpu_start: AtomicU64::new(0),
            active_requests: AtomicU32::new(0),
        }
    }

    /// Called by actor when a request starts processing.
    pub fn start_request(&self) {
        let prev = self.active_requests.fetch_add(1, Ordering::AcqRel);
        if prev == 0 {
            // First request — record start times
            let wall_nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64;
            self.request_wall_start.store(wall_nanos, Ordering::Release);

            #[cfg(target_os = "linux")]
            {
                let cpu_nanos = read_cpu_time(self.cpu_clock_id).as_nanos() as u64;
                self.request_cpu_start.store(cpu_nanos, Ordering::Release);
            }
        }
    }

    /// Called by actor when a request completes.
    pub fn end_request(&self) {
        let prev = self.active_requests.fetch_sub(1, Ordering::AcqRel);
        if prev == 1 {
            // Last request completed — clear start times
            self.request_wall_start.store(0, Ordering::Release);
            self.request_cpu_start.store(0, Ordering::Release);
        }
    }
}

/// Wrapper type so we can store `Arc<WatchdogEntry>` in `OpState`.
#[derive(Debug)]
pub struct OpWatchdogEntry(pub Arc<WatchdogEntry>);

fn watchdog_loop(entries: Arc<Mutex<HashMap<String, Arc<WatchdogEntry>>>>) {
    loop {
        std::thread::sleep(Duration::from_millis(500));

        let entries = entries.lock().unwrap();
        for (app_id, entry) in entries.iter() {
            let wall_start_nanos = entry.request_wall_start.load(Ordering::Acquire);
            if wall_start_nanos == 0 {
                continue; // no active request
            }

            // Check wall time
            let now_nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64;
            let wall_elapsed = Duration::from_nanos(now_nanos.saturating_sub(wall_start_nanos));

            if wall_elapsed > entry.wall_limit {
                eprintln!(
                    "[watchdog] {app_id}: wall time exceeded ({:.1}s > {:.1}s), terminating",
                    wall_elapsed.as_secs_f64(),
                    entry.wall_limit.as_secs_f64()
                );
                entry.v8_handle.terminate_execution();
                // Clear the start times so we don't re-terminate on next tick
                entry.request_wall_start.store(0, Ordering::Release);
                entry.request_cpu_start.store(0, Ordering::Release);
                continue;
            }

            // Check CPU time (Linux only)
            #[cfg(target_os = "linux")]
            {
                let cpu_start_nanos = entry.request_cpu_start.load(Ordering::Acquire);
                let cpu_now = read_cpu_time(entry.cpu_clock_id);
                let cpu_elapsed =
                    cpu_now.saturating_sub(Duration::from_nanos(cpu_start_nanos));

                if cpu_elapsed > entry.cpu_limit {
                    eprintln!(
                        "[watchdog] {app_id}: CPU time exceeded ({:.1}ms > {:.1}ms), terminating",
                        cpu_elapsed.as_secs_f64() * 1000.0,
                        entry.cpu_limit.as_secs_f64() * 1000.0
                    );
                    entry.v8_handle.terminate_execution();
                    // Clear the start times so we don't re-terminate on next tick
                    entry.request_wall_start.store(0, Ordering::Release);
                    entry.request_cpu_start.store(0, Ordering::Release);
                }
            }
        }
    }
}

/// Read a thread's CPU time given its clock ID.
#[cfg(target_os = "linux")]
fn read_cpu_time(clock_id: libc::clockid_t) -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    #[allow(unsafe_code)]
    unsafe { libc::clock_gettime(clock_id, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}
