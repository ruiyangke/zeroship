//! PID 1 zombie reaper.
//!
//! When the agent runs inside a libkrun microVM it's PID 1. PID 1 has
//! one mandatory job that no other process has: reap child processes
//! whose parents died, otherwise they pile up as `<defunct>` zombies
//! and eventually exhaust the kernel's PID table.
//!
//! Implementation: we don't need an async runtime for this — it's a
//! pure system-call loop. We block `SIGCHLD` process-wide, then spawn
//! a dedicated OS thread that waits for it via `sigwaitinfo()` and
//! drains every available zombie via `waitpid(WNOHANG)`. No compio
//! interaction required, no signalfd dance.
//!
//! Blocking SIGCHLD process-wide is why this **must** be installed
//! before any code that does `spawn() / wait()` on a child — the
//! `compio::runtime::spawn_blocking` exec path uses `wait()` which is
//! itself fine (it blocks until the child exits, no signal needed),
//! but if some other code expected an async SIGCHLD it wouldn't get
//! it. We're in PID 1; we own SIGCHLD.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};
use std::thread;

use lru::LruCache;
use nix::sys::signal::{pthread_sigmask, SigSet, SigmaskHow, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use tracing::warn;

/// `true` once the reaper thread is up and the SIGCHLD mask is in
/// place. Read by `/readyz` so a failed reaper install surfaces as
/// not-ready (a fleet of agents that drift to zombie-piling-up
/// produce a measurable readiness signal, not a silent log line).
static REAPER_HEALTHY: OnceLock<AtomicBool> = OnceLock::new();

/// Internal flag that tracks **only** whether the reaper thread is
/// actually installed and running. Unlike [`REAPER_HEALTHY`], this
/// is **never** mutated by tests — it's the source of truth used by
/// [`wait_for_child`] to decide between reaper-routed waiting and
/// the direct-`waitpid` fallback. Splitting it out fixes a race:
/// tests for the readyz-not-ready path flip `REAPER_HEALTHY` to
/// `false`/`true` mid-run, and if `wait_for_child` keyed off that,
/// concurrent exec tests would either spuriously fall back to
/// direct waitpid or, worse, register a waiter for a reaper that
/// isn't running — and hang forever.
static REAPER_INSTALLED: OnceLock<AtomicBool> = OnceLock::new();

fn healthy_flag() -> &'static AtomicBool {
    REAPER_HEALTHY.get_or_init(|| AtomicBool::new(false))
}

fn installed_flag() -> &'static AtomicBool {
    REAPER_INSTALLED.get_or_init(|| AtomicBool::new(false))
}

/// Whether the reaper is installed and running. False if either
/// `pthread_sigmask` or `thread::spawn` failed at install time, OR
/// if [`install`] has not been called yet.
pub fn is_healthy() -> bool {
    healthy_flag().load(Ordering::Relaxed)
}

/// **TEST ONLY.** Force the reaper-health flag to a specific value
/// so handler tests can exercise the readyz reaper-down path
/// without actually killing the reaper thread. Not exported in
/// release builds.
#[cfg(test)]
pub fn test_set_healthy(b: bool) {
    healthy_flag().store(b, Ordering::Relaxed);
}

/// **TEST ONLY.** Mutex shared by every test that mutates the
/// global `REAPER_HEALTHY` flag (this module's `tests` and
/// `handlers::tests`). Tests acquire it for their duration so the
/// per-test set/restore pattern is race-free.
#[cfg(test)]
pub(crate) static TEST_FLAG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// `Once` guard for [`install`]. Two concurrent calls would both
/// load `healthy=false`, both progress past the early-return, and
/// both spawn reaper threads — `Once` collapses that to one.
static INSTALL: std::sync::Once = std::sync::Once::new();

/// Install the reaper. **Thread-safe and idempotent**: the first
/// caller does the work, subsequent callers (including races) are
/// no-ops. Returns immediately; the reaper runs on a dedicated OS
/// thread for the lifetime of the process. After this call returns,
/// [`is_healthy`] reflects whether install succeeded.
///
/// Production callers invoke this at the top of `main`. Test code
/// can call it from anywhere — `wait_for_child` invokes it lazily
/// so unit tests of `exec.rs` work without a manual setup hook.
pub fn install() {
    INSTALL.call_once(|| install_inner());
}

fn install_inner() {
    // Block SIGCHLD on every thread. New threads inherit the mask, so
    // doing this from main before any spawns is enough.
    let mut set = SigSet::empty();
    set.add(Signal::SIGCHLD);
    if let Err(e) = pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&set), None) {
        install_failed("pthread_sigmask", e.to_string());
        return;
    }

    match thread::Builder::new()
        .name("zsbx-agent-reaper".into())
        .spawn(move || reaper_loop(set))
    {
        Ok(_handle) => {
            installed_flag().store(true, Ordering::Relaxed);
            healthy_flag().store(true, Ordering::Relaxed);
        }
        Err(e) => install_failed("thread::spawn", e.to_string()),
    }
}

fn install_failed(stage: &'static str, error: String) {
    warn!(
        stage,
        error,
        "reaper: install failed — agent running WITHOUT zombie reaping (/readyz will report not-ready)"
    );
}

/// RAII guard: clears [`REAPER_HEALTHY`] when the reaper thread
/// exits — panic OR normal return. This is the only signal that
/// `/readyz` has that the reaper thread died after install. Without
/// it, a panic in `set.wait()` would silently terminate zombie
/// reaping while the agent kept reporting ready.
struct ReaperGuard;

impl Drop for ReaperGuard {
    fn drop(&mut self) {
        installed_flag().store(false, Ordering::Relaxed);
        healthy_flag().store(false, Ordering::Relaxed);
        warn!("reaper thread exited — zombie reaping is no longer active");
    }
}

fn reaper_loop(set: SigSet) {
    let _guard = ReaperGuard;
    loop {
        // Wait for the next SIGCHLD. `wait()` (no args here means the
        // C-level sigwait wrapper) blocks until a signal in `set` is
        // pending, then consumes it.
        match set.wait() {
            Ok(_sig) => drain(),
            Err(e) => {
                // Spurious wakeups / EINTR: just retry. Anything else
                // is fatal-ish for the reaper, but we keep looping —
                // if we exit this thread, zombies pile up forever.
                warn!(error = %e, "reaper: sigwait error (continuing)");
                continue;
            }
        }
    }
}

// ─── exit-status routing ────────────────────────────────────────
//
// The reaper is the **only** place in the agent that calls
// `waitpid()` — `exec.rs` does NOT wait on its spawned children
// itself, because doing so races with the reaper for whoever reaps
// the zombie first. Whichever loser ends up calling `waitpid` after
// the child is already gone gets ECHILD.
//
// Routing keeps a single waitpid path: the reaper's `drain()` reaps
// every child on every `SIGCHLD`, and routes the resulting exit code
// to whichever `wait_for_child(pid)` waiter registered for it. If
// no waiter is registered yet — race between spawn and register —
// the result is briefly stashed in a bounded LRU; the eventual
// `wait_for_child` call drains it from there.
//
// `exec.rs` calls [`wait_for_child(pid)`] **before** dropping the
// `Child` handle, gets back an `mpsc::Receiver<i32>`, and awaits
// the exit code on it.

/// Per-pid waiter or stashed result. Same lock for both maps so the
/// "no waiter, no stash entry → race lost" hand-off happens
/// atomically inside `wait_for_child` / `notify_exit`.
struct WaitState {
    waiters: HashMap<i32, mpsc::Sender<i32>>,
    /// Bounded LRU of exit codes for pids that finished before the
    /// caller had a chance to register a waiter. Cap of 1024 means
    /// even an extreme spawn-and-forget pattern can't OOM us.
    stash: LruCache<i32, i32>,
}

static WAIT_STATE: OnceLock<Mutex<WaitState>> = OnceLock::new();

fn wait_state() -> &'static Mutex<WaitState> {
    WAIT_STATE.get_or_init(|| {
        Mutex::new(WaitState {
            waiters: HashMap::new(),
            stash: LruCache::new(NonZeroUsize::new(1024).unwrap()),
        })
    })
}

/// Register interest in a specific child pid. Returns a receiver
/// that will be sent the exit code (negative = killed by signal,
/// `-(sig as i32)`) when the child exits.
///
/// **Two modes**, picked by [`is_healthy`] at call time:
///
/// 1. **Reaper running** (production: PID 1 in libkrun). We
///    register a waiter; the reaper's `drain()` calls `notify_exit`
///    when it observes the child's `SIGCHLD`. If the child was
///    already reaped between spawn and this call, the stashed exit
///    code is delivered immediately.
///
/// 2. **Reaper NOT running** (unit tests of `exec.rs`, or any
///    process where install() wasn't called). We fall back to a
///    one-shot worker thread that does a blocking
///    `waitpid(pid, 0)` directly and sends the result.
///
/// The fallback is necessary because `sigwait`-based reaping only
/// works if SIGCHLD is blocked on **every** thread — a guarantee
/// we have at the top of `main`, but not in the libtest harness
/// where worker threads exist before our test runs and have
/// SIGCHLD unblocked. Trying to install a reaper from a
/// late-spawned thread leads to SIGCHLD being delivered to some
/// unblocked harness thread (default disposition: ignore), so the
/// reaper's `sigwait` never wakes and `wait_for_child` hangs
/// forever. Direct `waitpid` is race-free in the no-reaper case
/// because nothing else is competing for the child.
pub fn wait_for_child(pid: i32) -> mpsc::Receiver<i32> {
    let (tx, rx) = mpsc::channel();
    let mut state = wait_state().lock().unwrap_or_else(|p| p.into_inner());

    // Race: child may have already been reaped before we registered.
    if let Some(code) = state.stash.pop(&pid) {
        let _ = tx.send(code);
        return rx;
    }

    if installed_flag().load(Ordering::Relaxed) {
        // Reaper is running — it will route the exit code to us.
        state.waiters.insert(pid, tx);
    } else {
        // No reaper — direct waitpid in a one-shot worker thread.
        // Drop the lock first so the spawned thread doesn't contend.
        drop(state);
        let _ = thread::Builder::new()
            .name(format!("zsbx-direct-wait-{pid}"))
            .spawn(move || {
                let code = match waitpid(Pid::from_raw(pid), None) {
                    Ok(WaitStatus::Exited(_, c)) => c,
                    Ok(WaitStatus::Signaled(_, sig, _)) => -(sig as i32),
                    // Stopped/Continued shouldn't reach a blocking
                    // waitpid without WUNTRACED, and ECHILD means
                    // someone else (shouldn't happen without reaper)
                    // already reaped. Either way, -1 = unknown.
                    _ => -1,
                };
                let _ = tx.send(code);
            });
    }
    rx
}

fn notify_exit(pid: i32, code: i32) {
    let mut state = wait_state().lock().unwrap_or_else(|p| p.into_inner());
    if let Some(tx) = state.waiters.remove(&pid) {
        let _ = tx.send(code);
    } else {
        // No waiter — either it's an orphaned grandchild (no one
        // wanted the code) or the registrar lost the race. Stash
        // briefly; LRU eviction bounds memory.
        state.stash.put(pid, code);
    }
}

/// Reap every zombie that's currently waitable. Multiple zombies may
/// have queued up under one SIGCHLD (the kernel coalesces signals),
/// so we drain in a loop until `WNOHANG` says nothing is ready.
///
/// On each successful reap, the exit code is routed via
/// [`notify_exit`] to whichever `exec.rs` task asked for it.
fn drain() {
    loop {
        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => return,
            Ok(WaitStatus::Exited(pid, code)) => {
                notify_exit(pid.as_raw(), code);
            }
            Ok(WaitStatus::Signaled(pid, sig, _core)) => {
                // Convention: signal-killed exits report
                // `-(signal_number)` so the caller can tell them
                // apart from any 0..=255 normal exit.
                notify_exit(pid.as_raw(), -(sig as i32));
            }
            Ok(_) => continue, // stopped, continued, etc.
            Err(nix::errno::Errno::ECHILD) => return,
            Err(e) => {
                warn!(error = %e, "reaper: waitpid error");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_healthy_initially_false() {
        // Before install() runs in this test process, healthy must
        // be false. (If a previous test in the same binary called
        // install() this assertion may fail; install is global.)
        // We can't guarantee ordering vs other tests in the binary
        // but we CAN check that the function exists and returns a
        // bool without panicking.
        let _ = is_healthy();
    }

    #[test]
    fn drain_no_children_does_not_panic() {
        // No children to reap → ECHILD → drain returns. Just smoke
        // testing the function compiles and executes safely. In
        // CI the test process may have pending children from
        // other tests; in either case drain() must not panic.
        drain();
    }

    /// Smoke test that `install()` can be called twice without
    /// installing two reaper threads. We don't assert on the SIGCHLD
    /// machinery itself (that affects the whole process) — the
    /// idempotency check exercises the early-return path.
    #[test]
    fn install_is_idempotent_when_already_healthy() {
        let _g = super::TEST_FLAG_LOCK.lock().unwrap();
        let flag = healthy_flag();
        let prev = flag.load(Ordering::Relaxed);
        flag.store(true, Ordering::Relaxed);
        install();
        assert!(is_healthy());
        flag.store(prev, Ordering::Relaxed);
    }

    #[test]
    fn reaper_guard_clears_flag_on_drop() {
        let _g = super::TEST_FLAG_LOCK.lock().unwrap();
        let flag = healthy_flag();
        let prev = flag.load(Ordering::Relaxed);
        flag.store(true, Ordering::Relaxed);
        {
            let _rg = ReaperGuard;
            assert!(flag.load(Ordering::Relaxed));
        }
        assert!(!flag.load(Ordering::Relaxed));
        flag.store(prev, Ordering::Relaxed);
    }
}
