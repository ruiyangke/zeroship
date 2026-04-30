//! PID 1 zombie reaper.
//!
//! When the agent runs inside a libkrun microVM it's PID 1. PID 1 has
//! one mandatory job that no other process has: reap child processes
//! whose parents died, otherwise they pile up as `<defunct>` zombies
//! and eventually exhaust the kernel's PID table.
//!
//! We listen for `SIGCHLD` via `tokio::signal` and call `waitpid()` in
//! a non-blocking loop until the kernel has nothing left to give us.
//! That handles reparented grandchildren (the common case — `sh -c`
//! spawned by `/exec` may itself spawn long-running processes that
//! get reparented to PID 1 when sh exits).

use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use tokio::signal::unix::{signal, SignalKind};

/// Spawn the reaper. Runs forever; cancel by dropping the runtime.
pub fn spawn() {
    tokio::spawn(async move {
        let mut sigchld = match signal(SignalKind::child()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[agent] reaper: failed to install SIGCHLD handler: {e}");
                return;
            }
        };
        loop {
            // sigchld.recv() yields each time the kernel coalesces a
            // SIGCHLD; multiple zombies can be queued for one signal,
            // so we drain in a loop on every wake.
            if sigchld.recv().await.is_none() {
                return;
            }
            drain();
        }
    });
}

/// Reap every zombie that's currently waitable. Idempotent on its
/// own — if no children are waitable, returns immediately.
fn drain() {
    loop {
        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            // Nothing to reap right now.
            Ok(WaitStatus::StillAlive) => return,
            // Reaped one — keep draining.
            Ok(_) => continue,
            // ECHILD: no children at all. Done.
            Err(nix::errno::Errno::ECHILD) => return,
            Err(e) => {
                eprintln!("[agent] reaper: waitpid: {e}");
                return;
            }
        }
    }
}
