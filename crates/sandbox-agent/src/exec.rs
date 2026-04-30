//! Run a shell command in the VM.
//!
//! Design:
//!
//!   - Spawn `sh -c <cmd>` synchronously with stdout/stderr piped.
//!   - Take ownership of the pipe handles BEFORE waiting (so we can
//!     read them concurrently with the wait, instead of relying on
//!     `wait_with_output` which is all-or-nothing).
//!   - Two **reader threads** (via `compio::runtime::spawn_blocking`)
//!     drain stdout/stderr into bounded `Mutex<Vec<u8>>` buffers.
//!     Each stream is capped at [`MAX_OUTPUT_BYTES`]; on overflow the
//!     reader keeps draining the pipe (so the writer doesn't block)
//!     but stops appending and flips `truncated`.
//!   - A **waiter task** awaits the child's exit.
//!   - We race waiter vs `compio::time::sleep(timeout)`. On timeout
//!     we `killpg(SIGKILL)` the whole process group, then await the
//!     waiter (the dead child completes quickly).
//!   - Whichever path we took, the readers see EOF when pipes close
//!     (child exit closes them) and finish. We await both, then
//!     return the captured bytes.
//!
//! This means **a timeout still returns whatever output was buffered
//! before the kill** — important for debugging long-running commands
//! that fail on the wall clock with useful logs.
//!
//! ## Security
//!
//! `env_clear()` is called before re-adding only [`PASSTHROUGH_VARS`].
//! That strips `SANDBOX_AGENT_TOKEN`, `SANDBOX_AGENT_*`, and any other
//! agent-internal state, so user code can't `printenv` its way to the
//! bearer token. See the C1 regression test
//! `does_not_leak_agent_token_to_child`.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{ChildStderr, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct ExecOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
    /// Whether the wall-clock timeout fired before the process exited.
    pub timed_out: bool,
    /// Whether stdout was truncated at [`MAX_OUTPUT_BYTES`].
    pub stdout_truncated: bool,
    /// Whether stderr was truncated at [`MAX_OUTPUT_BYTES`].
    pub stderr_truncated: bool,
}

/// Hard ceiling on a single command's wall time (10 min).
pub const MAX_TIMEOUT_MS: u64 = 600_000;

/// Default if caller omits `timeout_ms`.
pub const DEFAULT_TIMEOUT_MS: u64 = 60_000;

/// Per-stream output cap. We keep draining the pipe past this so the
/// writer doesn't block, but stop appending and set `*_truncated`.
pub const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

/// Reader thread per-iteration buffer size.
const CHUNK_SIZE: usize = 8192;

const PASSTHROUGH_VARS: &[&str] = &["PATH", "HOME", "LANG", "LC_ALL", "TERM", "USER", "TZ"];

fn curated_env() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for k in PASSTHROUGH_VARS {
        if let Ok(v) = std::env::var(k) {
            out.push(((*k).to_string(), v));
        }
    }
    if !out.iter().any(|(k, _)| k == "PATH") {
        out.push((
            "PATH".to_string(),
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
        ));
    }
    out
}

pub async fn run(cmd: &str, cwd: &str, timeout_ms: u64) -> Result<ExecOutput, String> {
    let timeout_ms = timeout_ms.clamp(1, MAX_TIMEOUT_MS);

    // Spawn. process_group(0) so we can kill the whole tree on timeout.
    // env_clear() prevents `SANDBOX_AGENT_TOKEN` and other agent
    // state from leaking into the child's environ.
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .env_clear()
        .envs(curated_env())
        .spawn()
        .map_err(|e| format!("spawn sh: {e}"))?;

    let pid = child.id() as i32;
    let pgid = Pid::from_raw(pid);

    // Take the pipe handles BEFORE we move child into the waiter task.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    // Reader buffers (shared with the spawn_blocking tasks).
    let stdout_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let stderr_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let stdout_trunc: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let stderr_trunc: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    let stdout_task = spawn_stdout_reader(stdout, stdout_buf.clone(), stdout_trunc.clone());
    let stderr_task = spawn_stderr_reader(stderr, stderr_buf.clone(), stderr_trunc.clone());

    // Waiter and timeout.
    let waiter = compio::runtime::spawn_blocking(move || child.wait());
    let timeout_fut = compio::time::sleep(Duration::from_millis(timeout_ms));

    let (status, timed_out) = race_wait(waiter, timeout_fut, pgid).await;

    // Child exit closes the child's end of each pipe; readers see
    // EOF and finish (which then drops our end of the pipe via the
    // ChildStdout/ChildStderr's Drop). Joining is best-effort — if
    // a reader panicked, take_buf below recovers via PoisonError.
    if let Some(t) = stdout_task {
        let _ = t.await;
    }
    if let Some(t) = stderr_task {
        let _ = t.await;
    }

    // Snapshot buffers. Tolerate a poisoned mutex (reader thread
    // panicked) by recovering the inner Vec — better to return what
    // we have than to crash the ntex worker on .unwrap().
    let stdout_bytes = take_buf(&stdout_buf);
    let stderr_bytes = take_buf(&stderr_buf);

    Ok(ExecOutput {
        status,
        stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
        timed_out,
        stdout_truncated: stdout_trunc.load(Ordering::Relaxed),
        stderr_truncated: stderr_trunc.load(Ordering::Relaxed),
    })
}

// ─── child wait + timeout race ────────────────────────────────────

/// Three-deep nesting because `compio::runtime::spawn_blocking` wraps
/// the closure's result in `Result<_, Box<dyn Any + Send>>` (panic
/// payload), the closure here returns `std::io::Result<ExitStatus>`
/// (the OS-level wait error), and `ExitStatus` is the actual code.
///
/// Layers, outer → inner:
///
///   `Result<                                            // panic?
///       std::io::Result<                                // wait err?
///           std::process::ExitStatus                    // exit code
///       >,
///       Box<dyn Any + Send>                             // panic payload
///   >`
type WaitJoin = Result<std::io::Result<std::process::ExitStatus>, Box<dyn std::any::Any + Send>>;

async fn race_wait<W>(
    waiter: W,
    timeout: impl std::future::Future<Output = ()>,
    pgid: Pid,
) -> (i32, bool)
where
    W: std::future::Future<Output = WaitJoin>,
{
    use std::pin::pin;
    use std::task::Poll;

    let mut waiter = pin!(waiter);
    let mut timeout = pin!(timeout);

    enum Outcome { Wait(WaitJoin), Timeout }

    let outcome = std::future::poll_fn(|cx| {
        if let Poll::Ready(j) = waiter.as_mut().poll(cx) {
            return Poll::Ready(Outcome::Wait(j));
        }
        if timeout.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Outcome::Timeout);
        }
        Poll::Pending
    })
    .await;

    match outcome {
        Outcome::Wait(j) => (status_from(j), false),
        Outcome::Timeout => {
            // SIGKILL the whole process group. ESRCH = already gone.
            let _ = killpg(pgid, Signal::SIGKILL);
            // Now wait for the (now-dead) child to be reaped. This
            // returns quickly because the kernel just delivered exit.
            let j = waiter.await;
            (status_from(j), true)
        }
    }
}

fn status_from(j: WaitJoin) -> i32 {
    match j {
        Ok(Ok(s)) => s.code().unwrap_or(-1),
        Ok(Err(_)) | Err(_) => -1,
    }
}

/// Drain the buffer, recovering from a poisoned Mutex (reader thread
/// panicked) rather than propagating the panic. We always know we're
/// the last reader by the time this is called (reader tasks have
/// been awaited), so taking the inner Vec is safe.
fn take_buf(m: &Mutex<Vec<u8>>) -> Vec<u8> {
    match m.lock() {
        Ok(mut g) => std::mem::take(&mut *g),
        Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
    }
}

// ─── pipe readers ─────────────────────────────────────────────────

fn spawn_stdout_reader(
    pipe: Option<ChildStdout>,
    buf: Arc<Mutex<Vec<u8>>>,
    trunc: Arc<AtomicBool>,
) -> Option<compio::runtime::Task<Result<(), Box<dyn std::any::Any + Send>>>> {
    pipe.map(|p| {
        compio::runtime::spawn_blocking(move || drain_into(p, &buf, &trunc))
    })
}

fn spawn_stderr_reader(
    pipe: Option<ChildStderr>,
    buf: Arc<Mutex<Vec<u8>>>,
    trunc: Arc<AtomicBool>,
) -> Option<compio::runtime::Task<Result<(), Box<dyn std::any::Any + Send>>>> {
    pipe.map(|p| {
        compio::runtime::spawn_blocking(move || drain_into(p, &buf, &trunc))
    })
}

/// Drain `stream` into `buf`, capped at [`MAX_OUTPUT_BYTES`]. After
/// the cap is hit we keep reading (so the writer doesn't block on a
/// full pipe) but stop appending; `trunc` is flipped.
///
/// Tolerates a poisoned Mutex by recovering the inner Vec — important
/// because the parent task uses `take_buf` which itself recovers. If
/// THIS function panicked on poison, the reader thread would die and
/// the pipe might fill up, blocking the child.
fn drain_into<R: Read>(mut stream: R, buf: &Mutex<Vec<u8>>, trunc: &AtomicBool) {
    let mut chunk = [0u8; CHUNK_SIZE];
    let mut capped = false;
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return, // EOF
            Ok(n) => {
                if !capped {
                    let mut b = match buf.lock() {
                        Ok(g) => g,
                        Err(p) => p.into_inner(),
                    };
                    let space = MAX_OUTPUT_BYTES.saturating_sub(b.len());
                    if n <= space {
                        b.extend_from_slice(&chunk[..n]);
                    } else {
                        if space > 0 {
                            b.extend_from_slice(&chunk[..space]);
                        }
                        capped = true;
                        trunc.store(true, Ordering::Relaxed);
                    }
                }
                // Whether or not we stored, keep reading so the
                // writer doesn't block on a full pipe.
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[compio::test]
    async fn echo() {
        let out = run("echo hi", "/tmp", 5_000).await.unwrap();
        assert_eq!(out.status, 0);
        assert_eq!(out.stdout.trim(), "hi");
        assert!(!out.timed_out);
        assert!(!out.stdout_truncated);
    }

    #[compio::test]
    async fn nonzero_exit_propagates() {
        let out = run("exit 7", "/tmp", 5_000).await.unwrap();
        assert_eq!(out.status, 7);
    }

    #[compio::test]
    async fn stderr_captured() {
        let out = run("echo problem 1>&2", "/tmp", 5_000).await.unwrap();
        assert_eq!(out.stderr.trim(), "problem");
    }

    #[compio::test]
    async fn timeout_fires_and_kills_group() {
        let out = run("sleep 5", "/tmp", 200).await.unwrap();
        assert!(out.timed_out);
        assert_eq!(out.status, -1);
    }

    #[compio::test]
    async fn timeout_clamps_to_max() {
        let out = run("echo ok", "/tmp", u64::MAX).await.unwrap();
        assert_eq!(out.status, 0);
    }

    /// **C1 regression: token from agent's env never reaches child.**
    ///
    /// We don't mutate the parent process env (that races with other
    /// parallel tests). Instead we use the unit-tested helper
    /// [`curated_env`] directly — if it ever starts including
    /// `SANDBOX_AGENT_*`, the assertion catches it. The integration-
    /// flavor "real spawn with sentinel env" check lives in the
    /// end-to-end smoke run (a sub-process so its env mutation is
    /// scoped), not here.
    #[test]
    fn curated_env_excludes_sandbox_agent_vars() {
        // Even if the parent process happens to have these set,
        // curated_env() must not propagate them.
        let parent_var_seen = curated_env()
            .iter()
            .any(|(k, _)| k.starts_with("SANDBOX_AGENT"));
        assert!(!parent_var_seen, "curated_env leaked SANDBOX_AGENT_*");
        // The set of allowed keys is exactly PASSTHROUGH_VARS.
        let allowed: std::collections::HashSet<&str> =
            PASSTHROUGH_VARS.iter().copied().collect();
        for (k, _) in curated_env() {
            assert!(
                allowed.contains(k.as_str()) || k == "PATH",
                "unexpected env var passed through: {k}"
            );
        }
    }

    #[compio::test]
    async fn passes_through_path() {
        let out = run("printenv PATH", "/tmp", 5_000).await.unwrap();
        assert!(!out.stdout.trim().is_empty());
    }

    /// **C4 regression: timeout returns whatever stdout was already
    /// buffered before the kill.** Without the new architecture the
    /// timeout path returned an empty stdout, losing all the progress
    /// logs the caller needed to diagnose why a long-running command
    /// hit the wall clock.
    #[compio::test]
    async fn timeout_preserves_partial_output() {
        // Print "early" then sleep past the timeout. The "early"
        // bytes must survive the SIGKILL and reach the response.
        let out = run("echo early; sleep 5", "/tmp", 500).await.unwrap();
        assert!(out.timed_out);
        assert_eq!(out.stdout.trim(), "early", "partial output must survive");
    }

    /// **H1 regression: huge output is capped, marked truncated,
    /// reader keeps draining.** Without draining the pipe the writer
    /// blocks at the kernel buffer (~64 KiB) and never reaches the
    /// cap, so this test would also catch a missing drain.
    #[compio::test]
    async fn huge_output_truncated() {
        // ~24 MiB of output via dd if /dev/zero (24 MB > 16 MB cap).
        let out = run(
            "head -c 25000000 /dev/zero | base64",
            "/tmp",
            30_000,
        )
        .await
        .unwrap();
        assert!(out.stdout_truncated, "stdout should be marked truncated");
        assert!(
            out.stdout.len() <= MAX_OUTPUT_BYTES,
            "stdout {} exceeds cap {MAX_OUTPUT_BYTES}",
            out.stdout.len()
        );
        // Process should have completed normally (not timed out).
        assert!(!out.timed_out, "should NOT time out — pipe must keep draining");
    }
}
