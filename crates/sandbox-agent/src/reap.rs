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

use std::thread;

use nix::sys::signal::{pthread_sigmask, SigSet, SigmaskHow, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;

/// Install the reaper. Idempotent — calling more than once is safe.
/// Returns immediately; the reaper runs on a dedicated OS thread for
/// the lifetime of the process.
pub fn install() {
    // Block SIGCHLD on every thread. New threads inherit the mask, so
    // doing this from main before any spawns is enough.
    let mut set = SigSet::empty();
    set.add(Signal::SIGCHLD);
    if let Err(e) = pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&set), None) {
        eprintln!("[agent] reaper: pthread_sigmask: {e}");
        return;
    }

    thread::Builder::new()
        .name("zsbx-agent-reaper".into())
        .spawn(move || reaper_loop(set))
        .map(|_| ())
        .unwrap_or_else(|e| eprintln!("[agent] reaper: spawn: {e}"));
}

fn reaper_loop(set: SigSet) {
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
                eprintln!("[agent] reaper: sigwait: {e}");
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
                eprintln!("[agent] reaper: waitpid: {e}");
                return;
            }
        }
    }
}
