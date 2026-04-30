//! Run a shell command in the VM, with a hard timeout.
//!
//! We use `std::process::Command` (synchronous spawn) inside
//! `compio::runtime::spawn_blocking` for the wait-for-output path,
//! and race that future against a `compio::time::sleep`. On timeout
//! we send `SIGKILL` to the child's process group via `killpg(2)` so
//! sub-shells / npm scripts go down too.

use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
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
}

/// Hard ceiling on a single command's wall time (10 min). Caller may
/// pass a smaller value via `timeout_ms`; we clamp to this.
pub const MAX_TIMEOUT_MS: u64 = 600_000;

/// Default if caller omits `timeout_ms`. Long enough for `npm install`
/// in most cases, short enough that mistakes don't hang forever.
pub const DEFAULT_TIMEOUT_MS: u64 = 60_000;

/// Run `sh -c <cmd>` in the given working directory.
pub async fn run(cmd: &str, cwd: &str, timeout_ms: u64) -> Result<ExecOutput, String> {
    let timeout_ms = timeout_ms.clamp(1, MAX_TIMEOUT_MS);

    // Spawn synchronously (cheap). `process_group(0)` puts the child
    // into its own process group so we can kill the whole tree later.
    let child = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("spawn sh: {e}"))?;

    let pid = child.id() as i32;
    let pgid = Pid::from_raw(pid);

    // Move the child into a blocking task so wait_with_output() can
    // park a thread without blocking the compio reactor.
    let wait = compio::runtime::spawn_blocking(move || child.wait_with_output());

    // Race the wait against the timeout.
    let timeout = compio::time::sleep(Duration::from_millis(timeout_ms));

    futures_lite_select(wait, timeout, pgid).await
}

/// Outer Result is the spawn_blocking join (Err = closure panicked);
/// inner Result is the closure's own return value (the wait_with_output
/// io::Result). We collapse both into our `Result<ExecOutput, String>`.
type WaitJoin = Result<std::io::Result<std::process::Output>, Box<dyn std::any::Any + Send>>;

async fn futures_lite_select<W>(
    wait: W,
    timeout: impl std::future::Future<Output = ()>,
    pgid: Pid,
) -> Result<ExecOutput, String>
where
    W: std::future::Future<Output = WaitJoin>,
{
    use std::pin::pin;
    use std::task::Poll;

    let mut wait = pin!(wait);
    let mut timeout = pin!(timeout);

    std::future::poll_fn(move |cx| {
        if let Poll::Ready(joined) = wait.as_mut().poll(cx) {
            return Poll::Ready(map_output(joined));
        }
        if timeout.as_mut().poll(cx).is_ready() {
            // Best-effort kill the whole process group. ESRCH means
            // the group is already gone (race with exit); ignore.
            let _ = killpg(pgid, Signal::SIGKILL);
            return Poll::Ready(Ok(ExecOutput {
                status: -1,
                stdout: String::new(),
                stderr: "timed out".to_string(),
                timed_out: true,
            }));
        }
        Poll::Pending
    })
    .await
}

fn map_output(joined: WaitJoin) -> Result<ExecOutput, String> {
    let inner = joined.map_err(|_| "wait child: blocking task panicked".to_string())?;
    let o = inner.map_err(|e| format!("wait child: {e}"))?;
    Ok(ExecOutput {
        status: o.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        timed_out: false,
    })
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
}
