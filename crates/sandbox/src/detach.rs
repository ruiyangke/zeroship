//! Isolated detach for fire-and-forget compio futures that must NOT share the
//! ntex-worker runtime (avoids C-3/C-6 starvation pattern).
//!
//! Spawns a dedicated OS thread with its own private `compio::runtime::Runtime`
//! and runs the future to completion. Thread is named for debugging.
//! Spawn failure logs and drops the future (fire-and-forget contract).
//!
//! ## Why a dedicated OS thread + private compio runtime?
//!
//! The C-6 (and sibling C-3) wedge pattern: a fire-and-forget future detached
//! via `compio::runtime::spawn(...).detach()` lands on the SAME ntex-worker
//! compio runtime that subsequent HTTP requests land on (1-worker fleet →
//! guaranteed collision; N-worker fleets pin requests per TCP connection, so
//! a same-client reuse co-locates with the detached work too). If the detached
//! future has a long blocking await (e.g. ureq HTTP call to a half-dead agent
//! burning the full 60 s connect timeout), it starves sibling tasks on that
//! runtime — including subsequent wake/heartbeat work — for the full deadline.
//!
//! The fix is to break the runtime affinity: spawn a dedicated OS thread, mint
//! a fresh `compio::runtime::Runtime`, and `block_on(fut)`. The future runs to
//! completion independent of any worker runtime; sibling worker tasks never
//! observe its blocking awaits.
//!
//! `spawn_blocking` on the worker runtime is INSUFFICIENT — the detached
//! future itself (between blocking calls) still runs on the worker.
//!
//! `std::thread::spawn` only fails on ENOMEM / EAGAIN — if we can't allocate
//! a thread the controller has bigger problems; we log and drop the future
//! (the contract is already best-effort / fire-and-forget). The same applies
//! to `compio::runtime::Runtime::new()` failure.
//!
//! ## Why the factory closure (not a bare future)?
//!
//! The caller passes `make_fut: FnOnce() -> Future`, not the future directly.
//! Reason: compio's runtime internals (`Runtime::with_current`,
//! `RefCell<TimerRuntime>`) make any future that awaits a compio I/O op
//! `!Send`. A future is `Send` only until its first `!Send` await point.
//! Constructing the future inside the spawned thread (via the factory) means
//! we only need `make_fut: Send` — its captures are obviously `Send`
//! (`Arc<AppState>`, `Uuid`, etc.) even when the future it produces is not.
//! The future is polled on the same thread that built it, so its `!Send`
//! internals are never crossed.
//!
//! This matches the open-coded pre-refactor pattern at
//! `admin_handlers.rs::teardown_source_for_snapshot` and
//! `snapshot_store_gcs.rs::Tiered::put` exactly: a `std::thread::spawn`
//! closure that, inside the thread, mints the runtime and `block_on`s an
//! async block constructed in place.
//!
//! ## Thread naming
//!
//! Linux's `pr_set_name` truncates thread names at 15 bytes (TASK_COMM_LEN-1),
//! so only the leading characters of `name` will be visible in `ps`/`top -H`.
//! The full Rust-side name is preserved for `tracing` / log lines that
//! include `std::thread::current().name()`. Callers SHOULD pass a short prefix
//! (≤ 15 chars) plus optional tail; the tail is preserved at the language
//! level but truncated at the kernel level.

use std::future::Future;

/// Spawn a future produced by `make_fut` on a dedicated OS thread with its
/// own compio runtime. `name` is used for the OS thread name (truncated to
/// 15 chars by the kernel).
///
/// Fire-and-forget. The caller cannot await the result; the helper is
/// intended for background work where the contract is best-effort (teardown,
/// cleanup, fire-and-forget upload, periodic loops).
///
/// The future is constructed INSIDE the spawned thread by calling
/// `make_fut()`; see the module-level docs for why the factory shape is
/// required instead of a bare future.
///
/// On thread-spawn or runtime-construction failure the future is never
/// constructed and the error is logged at `tracing::error!` level — the
/// operator has no other signal that the background work was lost.
pub fn detach_isolated<MakeF, F, T>(name: impl Into<String>, make_fut: MakeF)
where
    MakeF: FnOnce() -> F + Send + 'static,
    F: Future<Output = T>,
    T: 'static,
{
    let name = name.into();
    let inner_name = name.clone();
    let spawn_result = std::thread::Builder::new()
        .name(name.clone())
        .spawn(move || {
            let rt = match compio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!(
                        target: "sandbox::detach",
                        thread = %inner_name,
                        error = %e,
                        "detach_isolated: failed to construct private compio runtime — dropping future"
                    );
                    return;
                }
            };
            let fut = make_fut();
            let _ = rt.block_on(fut);
        });
    if let Err(e) = spawn_result {
        tracing::error!(
            target: "sandbox::detach",
            thread = %name,
            error = %e,
            "detach_isolated: failed to spawn OS thread — dropping future"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// Happy path: the future runs to completion on the dedicated thread.
    /// We use an `Arc<AtomicBool>` flipped by the future and poll for it
    /// (bounded wait) on the main thread. The future awaits
    /// `compio::time::sleep` to prove the private compio runtime is real
    /// (not just a panic-fast stub) — this future is `!Send` due to
    /// compio's `RefCell`-backed timer runtime, which is exactly why
    /// the helper takes a factory closure (see module docs).
    #[test]
    fn detach_isolated_runs_future_to_completion() {
        let flag = Arc::new(AtomicBool::new(false));
        let flag_for_fut = Arc::clone(&flag);
        detach_isolated("test-happy", move || async move {
            compio::time::sleep(Duration::from_millis(5)).await;
            flag_for_fut.store(true, Ordering::SeqCst);
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while !flag.load(Ordering::SeqCst) {
            if Instant::now() > deadline {
                panic!("detach_isolated future never completed");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Future that returns immediately (no awaits). Exercises the
    /// `block_on` shape with a trivially-ready future — verifies the
    /// helper doesn't deadlock when there's nothing to schedule.
    #[test]
    fn detach_isolated_handles_immediate_future() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_for_fut = Arc::clone(&counter);
        detach_isolated("test-immediate", move || async move {
            counter_for_fut.fetch_add(1, Ordering::SeqCst);
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while counter.load(Ordering::SeqCst) == 0 {
            if Instant::now() > deadline {
                panic!("immediate future never ran");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    /// A future that panics on the dedicated thread must NOT bring down the
    /// main thread / runtime. The panic is contained to the detached OS
    /// thread; the caller observes nothing (fire-and-forget contract).
    ///
    /// We confirm the main thread is unaffected by spawning a panicking
    /// future, then dispatching a normal future right after and waiting
    /// for it to complete.
    #[test]
    fn detach_isolated_contains_panics() {
        detach_isolated("test-panicker", || async {
            panic!("intentional test panic — must not escape the detached thread");
        });
        // Main thread is still alive — schedule a normal one and wait.
        let flag = Arc::new(AtomicBool::new(false));
        let flag_for_fut = Arc::clone(&flag);
        detach_isolated("test-after-panic", move || async move {
            flag_for_fut.store(true, Ordering::SeqCst);
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while !flag.load(Ordering::SeqCst) {
            if Instant::now() > deadline {
                panic!("post-panic future never completed — runtime affected by panic?");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Many futures dispatched concurrently each spin up their own
    /// thread + runtime. We don't assert isolation here (the
    /// per-future runtime is the contract; if it leaked the helper
    /// itself would be broken), only that all of them complete.
    #[test]
    fn detach_isolated_dispatches_many() {
        let counter = Arc::new(AtomicUsize::new(0));
        let n = 8usize;
        for i in 0..n {
            let c = Arc::clone(&counter);
            detach_isolated(format!("test-many-{i}"), move || async move {
                compio::time::sleep(Duration::from_millis(3)).await;
                c.fetch_add(1, Ordering::SeqCst);
            });
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while counter.load(Ordering::SeqCst) < n {
            if Instant::now() > deadline {
                panic!(
                    "only {} of {n} detached futures completed",
                    counter.load(Ordering::SeqCst)
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(counter.load(Ordering::SeqCst), n);
    }

    /// Thread name is reflected in `std::thread::current().name()` from
    /// inside the dedicated thread. The kernel-side `pr_set_name`
    /// truncation does NOT affect the Rust-side name; this test
    /// asserts the language-level name is preserved.
    #[test]
    fn detach_isolated_preserves_rust_thread_name() {
        let observed: Arc<std::sync::Mutex<Option<String>>> =
            Arc::new(std::sync::Mutex::new(None));
        let observed_for_fut = Arc::clone(&observed);
        let want = "test-thread-name-long-tail";
        detach_isolated(want.to_string(), move || async move {
            let name = std::thread::current().name().map(|s| s.to_string());
            *observed_for_fut.lock().unwrap() = name;
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(name) = observed.lock().unwrap().as_ref() {
                assert_eq!(name, want);
                break;
            }
            if Instant::now() > deadline {
                panic!("name never observed");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
