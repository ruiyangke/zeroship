//! Event loop state and drivers.
//!
//! Two event loop variants:
//! - `run_event_loop`: drives to exhaustion (fire-and-forget side effects)
//! - `run_event_loop_until_settled`: drives until a specific promise settles
//!
//! Both use the min-heap timer system and async op channel.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use crate::timers::{fire_ready_timers, TimerState};

// ---------------------------------------------------------------------------
// Crypto key store
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) enum Curve {
    P256,
    P384,
}

#[derive(Debug, Clone)]
pub(crate) enum KeyData {
    Symmetric { raw: Vec<u8> },
    EcPrivate { pkcs8_der: Vec<u8>, curve: Curve },
    EcPublic { raw: Vec<u8>, curve: Curve },
    RsaPrivate { pkcs8_der: Vec<u8> },
    RsaPublic { spki_der: Vec<u8> },
    Ed25519Private { pkcs8_der: Vec<u8> },
    Ed25519Public { raw: Vec<u8> },
}

// ---------------------------------------------------------------------------
// Event loop state
// ---------------------------------------------------------------------------

/// State shared between V8 callbacks and the event loop driver.
///
/// The async op channel receiver (`op_rx`) is intentionally kept OUTSIDE this
/// struct (and therefore outside the `Rc<RefCell<>>`) so that blocking
/// `recv_timeout()` calls never hold a mutable borrow on shared state.
/// Whoever drives the event loop owns `op_rx` separately.
#[allow(missing_debug_implementations)]
pub(crate) struct EventLoopInner {
    pub(crate) timers: TimerState,
    /// Pending promise resolvers for async ops (fetch, DB, etc.)
    pub(crate) pending_resolvers: HashMap<u32, v8::Global<v8::PromiseResolver>>,
    /// Async op channel sender (cloned into background tasks)
    pub(crate) op_tx: mpsc::Sender<OpResult>,
    pub(crate) next_op_id: u32,
    /// Tokio runtime handle for spawning async ops (fetch, etc.)
    pub(crate) tokio_handle: Option<tokio::runtime::Handle>,
    /// Optional sender for ConcurrentIsolate event channel.
    /// When set, async ops send Event::OpCompleted here instead of op_tx.
    pub(crate) concurrent_event_tx: Option<mpsc::Sender<crate::concurrent::Event>>,
    /// Per-isolate log buffer. Console output is appended here.
    pub(crate) log_buffer: Vec<String>,
    /// Per-isolate key-value store (persistent across requests, lost on evict)
    pub(crate) kv_store: HashMap<String, String>,
    /// Per-app environment variables (injected at isolate creation).
    pub(crate) env_vars: HashMap<String, String>,
    /// Crypto key store — key material stays in Rust, JS holds opaque u32 handles.
    pub(crate) key_store: HashMap<u32, KeyData>,
    pub(crate) next_key_id: u32,
}

/// Result of an async op (e.g., fetch response, DB query result).
pub(crate) struct OpResult {
    pub(crate) id: u32,
    pub(crate) value: String,
}

impl EventLoopInner {
    pub(crate) fn new() -> (Self, mpsc::Receiver<OpResult>) {
        let (op_tx, op_rx) = mpsc::channel();
        (Self {
            timers: TimerState::new(),
            pending_resolvers: HashMap::new(),
            op_tx,
            next_op_id: 1,
            tokio_handle: None,
            concurrent_event_tx: None,
            log_buffer: Vec::new(),
            kv_store: HashMap::new(),
            env_vars: HashMap::new(),
            key_store: HashMap::new(),
            next_key_id: 1,
        }, op_rx)
    }

    pub(crate) fn with_env(env_vars: HashMap<String, String>) -> (Self, mpsc::Receiver<OpResult>) {
        let (mut state, rx) = Self::new();
        state.env_vars = env_vars;
        (state, rx)
    }
}

pub(crate) type SharedState = Rc<RefCell<EventLoopInner>>;

// ---------------------------------------------------------------------------
// Event loop helpers
// ---------------------------------------------------------------------------

/// Drain completed async ops from the channel and resolve their promises.
///
/// `op_rx` is outside the `RefCell` — no borrow on shared state needed for recv.
fn drain_async_ops(scope: &mut v8::PinScope, state: &SharedState, op_rx: &mpsc::Receiver<OpResult>) {
    loop {
        match op_rx.try_recv() {
            Ok(op_result) => {
                let resolver = state.borrow_mut().pending_resolvers.remove(&op_result.id);
                if let Some(resolver) = resolver {
                    let r = v8::Local::new(scope, &resolver);
                    let val = v8::String::new(scope, &op_result.value).unwrap();
                    r.resolve(scope, val.into());
                    scope.perform_microtask_checkpoint();
                }
            }
            Err(_) => break,
        }
    }
}

/// Compute the wait duration until the next valid timer fires.
fn compute_wait_timeout(state: &SharedState) -> Option<Duration> {
    let s = state.borrow();

    // Find next valid timer (skip cleared ones via lazy deletion)
    let mut next_fire = None;
    for std::cmp::Reverse(entry) in s.timers.heap.iter() {
        if s.timers.callbacks.contains_key(&entry.id) {
            next_fire = Some(entry.fire_at);
            break;
        }
    }

    match next_fire {
        Some(fire_at) => Some(fire_at.saturating_duration_since(std::time::Instant::now())),
        None if !s.pending_resolvers.is_empty() => Some(Duration::from_secs(60)),
        None => None,
    }
}

// ---------------------------------------------------------------------------
// Event loop drivers
// ---------------------------------------------------------------------------

/// Drive the event loop until no more pending work (timers, async ops).
/// Used for fire-and-forget side effects after a sync response.
pub(crate) fn run_event_loop(
    scope: &mut v8::PinScope,
    state: &SharedState,
    op_rx: &mpsc::Receiver<OpResult>,
) {
    loop {
        scope.perform_microtask_checkpoint();
        fire_ready_timers(scope, state);
        drain_async_ops(scope, state, op_rx);

        {
            let s = state.borrow();
            if s.timers.callbacks.is_empty() && s.pending_resolvers.is_empty() {
                break;
            }
        }

        let timeout = match compute_wait_timeout(state) {
            Some(d) => d,
            None => break,
        };

        if timeout.is_zero() {
            continue;
        }

        let has_pending_ops = !state.borrow().pending_resolvers.is_empty();
        if has_pending_ops {
            if let Ok(op_result) = op_rx.recv_timeout(timeout) {
                let resolver = state.borrow_mut().pending_resolvers.remove(&op_result.id);
                if let Some(resolver) = resolver {
                    let r = v8::Local::new(scope, &resolver);
                    let val = v8::String::new(scope, &op_result.value).unwrap();
                    r.resolve(scope, val.into());
                    scope.perform_microtask_checkpoint();
                }
            }
        } else {
            std::thread::sleep(timeout);
        }
    }
}

/// Drive the event loop until a specific promise settles, no more work, or
/// wall-time limit is exceeded.
pub(crate) fn run_event_loop_until_settled(
    scope: &mut v8::PinScope,
    state: &SharedState,
    op_rx: &mpsc::Receiver<OpResult>,
    promise: &v8::Global<v8::Promise>,
    wall_timeout: Duration,
) {
    let deadline = std::time::Instant::now() + wall_timeout;

    loop {
        // Check if promise already settled
        {
            let local = v8::Local::new(scope, promise);
            match local.state() {
                v8::PromiseState::Fulfilled | v8::PromiseState::Rejected => return,
                v8::PromiseState::Pending => {}
            }
        }

        // Check wall-time limit
        if std::time::Instant::now() > deadline {
            return; // wall-time exceeded, promise still pending
        }

        scope.perform_microtask_checkpoint();

        // Check after microtasks
        {
            let local = v8::Local::new(scope, promise);
            match local.state() {
                v8::PromiseState::Fulfilled | v8::PromiseState::Rejected => return,
                v8::PromiseState::Pending => {}
            }
        }

        fire_ready_timers(scope, state);

        // Check after timers
        {
            let local = v8::Local::new(scope, promise);
            match local.state() {
                v8::PromiseState::Fulfilled | v8::PromiseState::Rejected => return,
                v8::PromiseState::Pending => {}
            }
        }

        drain_async_ops(scope, state, op_rx);

        // Check after ops
        {
            let local = v8::Local::new(scope, promise);
            match local.state() {
                v8::PromiseState::Fulfilled | v8::PromiseState::Rejected => return,
                v8::PromiseState::Pending => {}
            }
        }

        // Check if done (no work left)
        {
            let s = state.borrow();
            if s.timers.callbacks.is_empty() && s.pending_resolvers.is_empty() {
                drop(s);
                scope.perform_microtask_checkpoint();
                return;
            }
        }

        let timeout = match compute_wait_timeout(state) {
            Some(d) => d,
            None => {
                scope.perform_microtask_checkpoint();
                return;
            }
        };

        if timeout.is_zero() {
            continue;
        }

        // Cap wait at remaining wall-time
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let timeout = timeout.min(remaining);

        let has_pending_ops = !state.borrow().pending_resolvers.is_empty();
        if has_pending_ops {
            if let Ok(op_result) = op_rx.recv_timeout(timeout) {
                let resolver = state.borrow_mut().pending_resolvers.remove(&op_result.id);
                if let Some(resolver) = resolver {
                    let r = v8::Local::new(scope, &resolver);
                    let val = v8::String::new(scope, &op_result.value).unwrap();
                    r.resolve(scope, val.into());
                    scope.perform_microtask_checkpoint();
                }
            }
        } else {
            std::thread::sleep(timeout);
        }
    }
}
