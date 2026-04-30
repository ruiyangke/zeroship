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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::thread;

use nix::sys::signal::{pthread_sigmask, SigSet, SigmaskHow, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use tracing::warn;

/// `true` once the reaper thread is up and the SIGCHLD mask is in
/// place. Read by `/readyz` so a failed reaper install surfaces as
/// not-ready (a fleet of agents that drift to zombie-piling-up
/// produce a measurable readiness signal, not a silent log line).
static REAPER_HEALTHY: OnceLock<AtomicBool> = OnceLock::new();

fn healthy_flag() -> &'static AtomicBool {
    REAPER_HEALTHY.get_or_init(|| AtomicBool::new(false))
}

/// Whether the reaper is installed and running. False if either
/// `pthread_sigmask` or `thread::spawn` failed at install time, OR
/// if [`install`] has not been called yet.
pub fn is_healthy() -> bool {
    healthy_flag().load(Ordering::Relaxed)
}

/// Install the reaper. Intended to be called **once** from `main` —
/// concurrent calls aren't strictly safe (they could spawn two
/// reaper threads racing for SIGCHLD); after a successful install,
/// subsequent calls no-op via the [`REAPER_HEALTHY`] check.
///
/// Returns immediately; the reaper runs on a dedicated OS thread for
/// the lifetime of the process. After this call returns,
/// [`is_healthy`] reflects whether install succeeded.
pub fn install() {
    let healthy = healthy_flag();
    if healthy.load(Ordering::Relaxed) {
        return; // already installed
    }
    // Block SIGCHLD on every thread. New threads inherit the mask, so
    // doing this from main before any spawns is enough.
    let mut set = SigSet::empty();
    set.add(Signal::SIGCHLD);
    if let Err(e) = pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&set), None) {
        install_failed("pthread_sigmask", e.to_string());
        return;
    }

    // Parent sets healthy=true after a successful spawn. There is a
    // microsecond-scale theoretical race where the new thread could
    // panic and run [`ReaperGuard::drop`] (storing false) before
    // this store of true — `is_healthy()` would then incorrectly
    // report ready. In practice the thread doesn't die before its
    // first instruction; if it did we have bigger problems than
    // this readiness signal.
    match thread::Builder::new()
        .name("zsbx-agent-reaper".into())
        .spawn(move || reaper_loop(set))
    {
        Ok(_handle) => {
            healthy.store(true, Ordering::Relaxed);
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

/// Reap every zombie that's currently waitable. Multiple zombies may
/// have queued up under one SIGCHLD (the kernel coalesces signals),
/// so we drain in a loop until `WNOHANG` says nothing is ready.
fn drain() {
    loop {
        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => return,
            Ok(_) => continue,
            Err(nix::errno::Errno::ECHILD) => return,
            Err(e) => {
                warn!(error = %e, "reaper: waitpid error");
                return;
            }
        }
    }
}
