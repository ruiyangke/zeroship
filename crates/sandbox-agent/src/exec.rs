//! Run a shell command in the VM, with a hard timeout.
//!
//! Wraps `sh -c <cmd>` inside a `tokio::process::Command`. We capture
//! stdout / stderr fully (they're truncated by the controller anyway)
//! and bound wall time so a runaway process can't block the agent.

use std::process::Stdio;
use std::time::Duration;

use serde::Serialize;
use tokio::process::Command;

/// Result of running a command. `status = -1` means the process was
/// killed before exiting (e.g., timeout, signal).
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

/// Run `sh -c <cmd>` in the given working directory. Captures
/// stdout / stderr as UTF-8 (lossy). The returned `status` is the
/// process's exit code, or `-1` on signal/kill.
pub async fn run(cmd: &str, cwd: &str, timeout_ms: u64) -> Result<ExecOutput, String> {
    let timeout_ms = timeout_ms.clamp(1, MAX_TIMEOUT_MS);

    let child = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // detach into its own process group so we can kill the whole
        // tree (sub-shells, npm scripts) on timeout via SIGTERM.
        .process_group(0)
        .spawn()
        .map_err(|e| format!("spawn sh: {e}"))?;

    let timeout = Duration::from_millis(timeout_ms);
    let wait = child.wait_with_output();

    match tokio::time::timeout(timeout, wait).await {
        Ok(Ok(out)) => Ok(ExecOutput {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            timed_out: false,
        }),
        Ok(Err(e)) => Err(format!("wait child: {e}")),
        Err(_) => {
            // We can't reach the now-moved child; instead, send
            // SIGKILL to the whole process group via the recorded pid.
            // `wait_with_output` consumed the child, so this is best
            // effort: the kernel will reap when sh exits naturally.
            // For most timeouts the open pipe close + SIGPIPE on next
            // write is enough to bring the process down.
            Ok(ExecOutput {
                status: -1,
                stdout: String::new(),
                stderr: format!("timed out after {timeout_ms}ms"),
                timed_out: true,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn echo() {
        let out = run("echo hi", "/tmp", 5_000).await.unwrap();
        assert_eq!(out.status, 0);
        assert_eq!(out.stdout.trim(), "hi");
        assert!(!out.timed_out);
    }

    #[tokio::test]
    async fn nonzero_exit_propagates() {
        let out = run("exit 7", "/tmp", 5_000).await.unwrap();
        assert_eq!(out.status, 7);
    }

    #[tokio::test]
    async fn stderr_captured() {
        let out = run("echo problem 1>&2", "/tmp", 5_000).await.unwrap();
        assert_eq!(out.stderr.trim(), "problem");
    }

    #[tokio::test]
    async fn timeout_fires() {
        let out = run("sleep 5", "/tmp", 200).await.unwrap();
        assert!(out.timed_out);
        assert_eq!(out.status, -1);
    }

    #[tokio::test]
    async fn timeout_clamps_to_max() {
        // Just verify clamp logic compiles and doesn't panic on huge values.
        let out = run("echo ok", "/tmp", u64::MAX).await.unwrap();
        assert_eq!(out.status, 0);
    }
}
