//! POSIX CPU timer — per-thread CPU time enforcement for V8 isolates (Linux only).
//!
//! Architecture:
//! - One `CpuTimerSystem` per process: shared pipe + watchdog thread
//! - One `CpuTimer` per V8 actor thread: POSIX timer on CLOCK_THREAD_CPUTIME_ID
//! - Signal handler (SIGRTMIN+1): async-signal-safe write to pipe
//! - Watchdog thread: reads pipe, calls v8::IsolateHandle::terminate_execution()
//!
//! The signal handler never calls V8 directly (V8's TerminateExecution acquires a
//! mutex, which would deadlock if the signal fires while V8 holds it). Instead,
//! it writes the app_id to a pipe; a separate watchdog thread reads the pipe and
//! calls terminate_execution() in normal (non-signal) context.

use std::collections::HashMap;
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Global statics for signal handler (must be async-signal-safe)
// ---------------------------------------------------------------------------

/// Pipe write fd, accessed from signal handler. -1 means not initialized.
static PIPE_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

/// Cached value of SIGRTMIN() + 1. Set once during CpuTimerSystem::new().
static TIMER_SIGNAL: AtomicI32 = AtomicI32::new(0);

/// Global singleton for the CPU timer system.
static CPU_TIMER_SYSTEM: OnceLock<CpuTimerSystem> = OnceLock::new();

// ---------------------------------------------------------------------------
// Signal handler — MUST be async-signal-safe
// ---------------------------------------------------------------------------

/// Signal handler for CPU timer expiration.
///
/// # Safety
/// Only async-signal-safe operations: atomic load, libc::write.
/// No malloc, no Mutex, no println, no panic.
#[allow(unsafe_code)]
extern "C" fn cpu_timeout_handler(
    _sig: libc::c_int,
    info: *mut libc::siginfo_t,
    _ctx: *mut libc::c_void,
) {
    // Extract app_id from sigval (set during timer_create via sival_ptr)
    let app_id: u64 = unsafe { (*info).si_value().sival_ptr as u64 };

    // Write app_id to pipe (write() is async-signal-safe per POSIX)
    let fd = PIPE_WRITE_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        unsafe {
            libc::write(fd, &app_id as *const u64 as *const libc::c_void, 8);
        }
    }
}

// ---------------------------------------------------------------------------
// CpuTimerSystem — per-process shared state
// ---------------------------------------------------------------------------

/// Per-process shared state for CPU timer enforcement.
///
/// Created once via `get_or_init()`. Owns the pipe and watchdog thread.
/// Isolate actors register/unregister their V8 handles here.
pub struct CpuTimerSystem {
    #[allow(dead_code)]
    pipe_read: RawFd,
    #[allow(dead_code)]
    pipe_write: RawFd,
    handles: Arc<Mutex<HashMap<u64, v8::IsolateHandle>>>,
    #[allow(dead_code)]
    _watchdog: std::thread::JoinHandle<()>,
}

// Safety: RawFd is Copy/Send, the HashMap is behind Arc<Mutex>, JoinHandle is Send.
#[allow(unsafe_code)]
unsafe impl Send for CpuTimerSystem {}
#[allow(unsafe_code)]
unsafe impl Sync for CpuTimerSystem {}

impl CpuTimerSystem {
    /// Get or initialize the global CPU timer system singleton.
    pub fn get_or_init() -> &'static CpuTimerSystem {
        CPU_TIMER_SYSTEM.get_or_init(|| CpuTimerSystem::new())
    }

    /// Create the shared CPU timer system.
    ///
    /// - Creates a pipe (with O_CLOEXEC)
    /// - Registers the signal handler for SIGRTMIN+1
    /// - Spawns the watchdog thread
    #[allow(unsafe_code)]
    fn new() -> Self {
        // Create pipe with O_CLOEXEC
        let mut fds = [0i32; 2];
        let ret = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
        if ret != 0 {
            panic!(
                "pipe2 failed: {}",
                std::io::Error::last_os_error()
            );
        }
        let pipe_read = fds[0];
        let pipe_write = fds[1];

        // Store pipe_write in global static for signal handler access
        PIPE_WRITE_FD.store(pipe_write, Ordering::Relaxed);

        // Cache the signal number (SIGRTMIN() is a function call)
        let sig = libc::SIGRTMIN() + 1;
        TIMER_SIGNAL.store(sig, Ordering::Relaxed);

        // Register signal handler for SIGRTMIN+1 with SA_SIGINFO
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = cpu_timeout_handler as *const () as usize;
            sa.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
            libc::sigemptyset(&mut sa.sa_mask);
            if libc::sigaction(sig, &sa, std::ptr::null_mut()) != 0 {
                panic!(
                    "sigaction failed: {}",
                    std::io::Error::last_os_error()
                );
            }
        }

        let handles: Arc<Mutex<HashMap<u64, v8::IsolateHandle>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let handles_clone = handles.clone();

        let watchdog = std::thread::Builder::new()
            .name("cpu-timer-watchdog".into())
            .spawn(move || watchdog_loop(pipe_read, handles_clone))
            .expect("Failed to spawn CPU timer watchdog thread");

        Self {
            pipe_read,
            pipe_write,
            handles,
            _watchdog: watchdog,
        }
    }

    /// Register an isolate's V8 handle for termination by app_id hash.
    pub fn register(&self, app_id: u64, handle: v8::IsolateHandle) {
        self.handles.lock().unwrap().insert(app_id, handle);
    }

    /// Unregister an isolate by app_id hash.
    #[allow(dead_code)]
    pub fn unregister(&self, app_id: u64) {
        self.handles.lock().unwrap().remove(&app_id);
    }
}

// Note: Drop is not implemented for the singleton — it lives for the process lifetime.
// The pipe and watchdog thread are cleaned up when the process exits.

// ---------------------------------------------------------------------------
// Watchdog thread — reads pipe, terminates V8 isolates
// ---------------------------------------------------------------------------

/// Watchdog loop: blocks on pipe read, calls terminate_execution() on timeout.
///
/// Runs in normal thread context, so V8 mutex acquisition is safe.
#[allow(unsafe_code)]
fn watchdog_loop(pipe_read: RawFd, handles: Arc<Mutex<HashMap<u64, v8::IsolateHandle>>>) {
    loop {
        let mut app_id: u64 = 0;
        let n = unsafe {
            libc::read(
                pipe_read,
                &mut app_id as *mut u64 as *mut libc::c_void,
                8,
            )
        };
        if n == 0 {
            // Pipe closed — system is shutting down
            break;
        }
        if n != 8 {
            // Short read or error — skip
            continue;
        }

        let handles = handles.lock().unwrap();
        if let Some(handle) = handles.get(&app_id) {
            handle.terminate_execution();
            eprintln!("[cpu-timer] app {app_id:#x} terminated: CPU limit exceeded");
        }
    }
}

// ---------------------------------------------------------------------------
// CpuTimer — per-isolate POSIX timer
// ---------------------------------------------------------------------------

/// Per-isolate POSIX timer bound to the V8 actor thread's CPU clock.
///
/// MUST be created on the V8 actor thread (CLOCK_THREAD_CPUTIME_ID = "this thread").
/// Armed/disarmed around active request periods.
pub struct CpuTimer {
    timer_id: libc::timer_t,
    #[allow(dead_code)]
    app_id: u64,
}

// Safety: CpuTimer is only used on the thread that created it (the V8 actor thread).
// timer_t is a raw pointer but POSIX timer operations are thread-safe.
#[allow(unsafe_code)]
unsafe impl Send for CpuTimer {}

impl CpuTimer {
    /// Create a POSIX timer for the current thread's CPU time.
    ///
    /// MUST be called from the V8 actor thread.
    #[allow(unsafe_code)]
    pub fn new(app_id: u64) -> Result<Self, String> {
        let tid = unsafe { libc::syscall(libc::SYS_gettid) } as libc::c_int;

        let sig = TIMER_SIGNAL.load(Ordering::Relaxed);
        if sig == 0 {
            return Err("CpuTimerSystem not initialized (signal not registered)".into());
        }

        let mut sev: libc::sigevent = unsafe { std::mem::zeroed() };
        sev.sigev_notify = libc::SIGEV_THREAD_ID;
        sev.sigev_signo = sig;
        sev.sigev_value = libc::sigval {
            sival_ptr: app_id as *mut libc::c_void,
        };
        // sigev_notify_thread_id is a non-standard Linux extension.
        // In glibc, it shares storage with sigev_notify_function via a union.
        sev.sigev_notify_thread_id = tid;

        let mut timer_id: libc::timer_t = std::ptr::null_mut();
        let ret = unsafe {
            libc::timer_create(
                libc::CLOCK_THREAD_CPUTIME_ID,
                &mut sev,
                &mut timer_id,
            )
        };
        if ret != 0 {
            return Err(format!(
                "timer_create failed: {}",
                std::io::Error::last_os_error()
            ));
        }

        Ok(Self { timer_id, app_id })
    }

    /// Get the app_id hash this timer is associated with.
    #[allow(dead_code)]
    pub fn app_id(&self) -> u64 {
        self.app_id
    }

    /// Arm the timer with a CPU time limit (one-shot).
    ///
    /// When the V8 thread consumes `limit` CPU time, SIGRTMIN+1 fires.
    #[allow(unsafe_code)]
    pub fn arm(&self, limit: Duration) {
        let spec = libc::itimerspec {
            it_interval: libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: libc::timespec {
                tv_sec: limit.as_secs() as i64,
                tv_nsec: limit.subsec_nanos() as i64,
            },
        };
        unsafe {
            libc::timer_settime(self.timer_id, 0, &spec, std::ptr::null_mut());
        }
    }

    /// Disarm the timer (set to zero = stopped).
    #[allow(unsafe_code)]
    pub fn disarm(&self) {
        let zero = libc::itimerspec {
            it_interval: libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
        };
        unsafe {
            libc::timer_settime(self.timer_id, 0, &zero, std::ptr::null_mut());
        }
    }
}

impl Drop for CpuTimer {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        unsafe {
            libc::timer_delete(self.timer_id);
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Hash a string app_id to a u64 for use as the POSIX timer sigval.
#[allow(dead_code)]
pub fn app_id_hash(app_id: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    app_id.hash(&mut hasher);
    hasher.finish()
}
