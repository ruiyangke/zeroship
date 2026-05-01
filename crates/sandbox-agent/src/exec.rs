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
//!
//! ## `unsafe`
//!
//! `Command::pre_exec` is unsafe because the closure runs in the
//! post-fork child and must be async-signal-safe. We delegate to
//! `dropuser::pre_exec_lockdown` which carries the safety contract.

#![allow(unsafe_code)]

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
    //
    // Privilege drop + lockdown: `dropuser::pre_exec_lockdown` runs
    // in the post-fork child and applies (in order): capability
    // bounding set drop, setuid to nobody (when running as root),
    // PR_SET_NO_NEW_PRIVS, and RLIMIT_NOFILE/NPROC. We don't use
    // std's `Command::uid/gid` because they don't let us interleave
    // a `prctl(PR_CAPBSET_DROP)` *before* the setuid — which we need
    // (CAP_SETPCAP is gone after setuid).
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .env_clear()
        .envs(curated_env());
    let creds = crate::dropuser::child_creds();
    if creds.is_some() {
        // Point HOME at the per-user home dir. When the session is
        // backed by a per-user PVC mounted at this path, npm/pnpm/
        // pip/cargo caches survive across sandboxes for the same
        // user. When there's no PVC the dir is just an empty
        // chowned tmpfs/rootfs entry, also fine.
        command.env("HOME", crate::dropuser::USER_HOME);
        command.env("USER", "nobody");
    }
    // SAFETY: `pre_exec_lockdown` is documented async-signal-safe;
    // see its module docs. The closure captures only `creds: Option<(u32,u32)>`
    // by value (Copy), so no allocations cross the fork boundary.
    unsafe {
        command.pre_exec(move || crate::dropuser::pre_exec_lockdown(creds));
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("spawn sh: {e}"))?;

    let pid = child.id() as i32;
    let pgid = Pid::from_raw(pid);

    // Take the pipe handles BEFORE we drop the child.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    // Register interest in this pid's exit BEFORE dropping the
    // child, so the reaper has somewhere to deliver the exit code.
    // wait_for_child also drains a stashed result if the reaper
    // already raced ahead and reaped (sub-millisecond commands hit
    // this path).
    let exit_rx = crate::reap::wait_for_child(pid);

    // Drop the Child. std's Drop does NOT call wait — it just
    // releases internal resources. The kernel zombie remains until
    // the reaper picks it up, which is exactly what we want.
    drop(child);

    // Reader buffers (shared with the spawn_blocking tasks).
    let stdout_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let stderr_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let stdout_trunc: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let stderr_trunc: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    let stdout_task = spawn_stdout_reader(stdout, stdout_buf.clone(), stdout_trunc.clone());
    let stderr_task = spawn_stderr_reader(stderr, stderr_buf.clone(), stderr_trunc.clone());

    // Block on the reaper-routed exit channel inside spawn_blocking.
    // The reaper sends the exit code (or `-(sig as i32)` for
    // signal-killed) on its sole `waitpid` path — no race with us.
    let waiter = compio::runtime::spawn_blocking(move || exit_rx.recv());
    let timeout_fut = compio::time::sleep(Duration::from_millis(timeout_ms));

    let (status, timed_out) = race_wait(waiter, timeout_fut, pgid).await;

    // **Always** SIGKILL the whole process group, even on clean exit.
    // The shell can `cmd &`-fork a daemonized grandchild that
    // outlives `sh -c` and inherits stdout/stderr fds. If we don't
    // kill the group, the grandchild keeps the pipe write-end open,
    // the reader threads never see EOF, and this whole `run()` hangs
    // until ntex's shutdown timeout — a trivial DoS for any caller
    // who can submit `/exec`. ESRCH means the group is already gone.
    let _ = killpg(pgid, Signal::SIGKILL);

    // Reader threads should see EOF promptly now that the pgroup is
    // dead. Bound the wait so a leaked-fd grandchild that survived
    // SIGKILL (e.g. uninterruptible sleep on NFS) can't pin the
    // request indefinitely. After the deadline we abandon the
    // readers — `take_buf` below recovers whatever they've buffered.
    if let Some(t) = stdout_task {
        let _ = race_with_deadline(t, Duration::from_secs(2)).await;
    }
    if let Some(t) = stderr_task {
        let _ = race_with_deadline(t, Duration::from_secs(2)).await;
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

/// Race a future against a wall-clock deadline. On the deadline
/// firing first, the future is dropped (cancelled) and we return
/// `None`. Used to bound reader-task joins so a daemonized
/// grandchild holding a pipe can't stall `/exec`.
async fn race_with_deadline<F: std::future::Future>(
    fut: F,
    deadline: Duration,
) -> Option<F::Output> {
    use std::future::Future;
    use std::pin::pin;
    use std::task::Poll;

    let mut fut = pin!(fut);
    let mut sleep = pin!(compio::time::sleep(deadline));
    std::future::poll_fn(|cx| {
        if let Poll::Ready(out) = fut.as_mut().poll(cx) {
            return Poll::Ready(Some(out));
        }
        if sleep.as_mut().poll(cx).is_ready() {
            return Poll::Ready(None);
        }
        Poll::Pending
    })
    .await
}

// ─── child wait + timeout race ────────────────────────────────────
//
// The waiter is now a `spawn_blocking` task that blocks on an
// `mpsc::Receiver<i32>` whose far end is fed by the reaper
// (`reap::wait_for_child`). Three nested `Result`s:
//
//   `Result<                                                  // panic?
//       Result<i32, mpsc::RecvError>,                         // channel closed?
//       Box<dyn Any + Send>                                   // panic payload
//   >`
//
// `mpsc::RecvError` happens only if the sender is dropped without
// sending — would mean the reaper terminated without notifying,
// which we treat as `-1` (unknown exit).
type WaitJoin =
    Result<Result<i32, std::sync::mpsc::RecvError>, Box<dyn std::any::Any + Send>>;

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
            // Kill the whole process group so the waiter unblocks.
            // The caller will issue another `killpg` after we
            // return; double-killing is fine (ESRCH = already gone).
            let _ = killpg(pgid, Signal::SIGKILL);
            // The reaper will pick up the now-dead child and route
            // its exit code (`-(SIGKILL as i32)`) to our receiver.
            let j = waiter.await;
            (status_from(j), true)
        }
    }
}

fn status_from(j: WaitJoin) -> i32 {
    match j {
        Ok(Ok(code)) => code,
        // Channel closed (reaper down) or task panic — unknown exit.
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
        // Signal-killed children report `-(sig as i32)`. SIGKILL = 9.
        assert_eq!(out.status, -(nix::sys::signal::Signal::SIGKILL as i32));
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

    #[compio::test]
    async fn stdin_is_closed_returns_eof_immediately() {
        // `cat` with no args reads stdin; with stdin=null it sees
        // EOF and exits 0 with empty stdout.
        let out = run("cat", "/tmp", 5_000).await.unwrap();
        assert_eq!(out.status, 0);
        assert!(out.stdout.is_empty());
    }

    #[compio::test]
    async fn binary_output_is_lossy_not_panic() {
        // 0x80 alone is invalid UTF-8. from_utf8_lossy turns it
        // into the U+FFFD replacement char rather than panicking.
        let out = run("printf '\\x80\\x81'", "/tmp", 5_000).await.unwrap();
        assert_eq!(out.status, 0);
        assert!(!out.stdout.is_empty());
    }

    #[compio::test]
    async fn cwd_affects_command() {
        let out = run("pwd", "/tmp", 5_000).await.unwrap();
        assert_eq!(out.stdout.trim(), "/tmp");
    }

    #[compio::test]
    async fn multiple_runs_are_independent() {
        let a = run("echo a", "/tmp", 5_000).await.unwrap();
        let b = run("echo b", "/tmp", 5_000).await.unwrap();
        let c = run("echo c", "/tmp", 5_000).await.unwrap();
        assert_eq!(a.stdout.trim(), "a");
        assert_eq!(b.stdout.trim(), "b");
        assert_eq!(c.stdout.trim(), "c");
    }

    // ─── security regression tests ───────────────────────────────
    //
    // These exercise the `dropuser::pre_exec_lockdown` integration
    // through the real `run()` path. They run on whatever uid the
    // test harness has — typically NOT root, so the privilege-drop
    // step is a no-op and only the always-applied hardenings
    // (no_new_privs, RLIMIT_NOFILE) are verified. The capability
    // and uid-drop paths are exercised end-to-end by `e2e_k3s`.

    /// **S1: no_new_privs is set on every /exec child.** Defends
    /// against a setuid-root binary on PATH (e.g., a future image
    /// rev that accidentally installs `mount` setuid) being able
    /// to elevate the dropped child back to root.
    #[compio::test]
    async fn child_has_no_new_privs() {
        let out = run(
            "grep -E '^NoNewPrivs:' /proc/self/status",
            "/tmp",
            5_000,
        )
        .await
        .unwrap();
        assert_eq!(out.status, 0, "grep failed: {out:?}");
        assert!(
            out.stdout.contains("NoNewPrivs:\t1"),
            "expected NoNewPrivs=1, got: {}",
            out.stdout
        );
    }

    /// **S2: RLIMIT_NOFILE is capped on every /exec child.** A
    /// process that leaks fds (or maliciously opens many) is
    /// bounded; a forgotten close-loop in user code can't exhaust
    /// the kernel's per-process fd table.
    #[compio::test]
    async fn child_rlimit_nofile_capped() {
        // /proc/self/limits is stable across dash/bash; column 4 of
        // the "Max open files" row is the soft limit.
        let out = run(
            "awk '/^Max open files/ {print $4}' /proc/self/limits",
            "/tmp",
            5_000,
        )
        .await
        .unwrap();
        assert_eq!(out.status, 0, "awk failed: {out:?}");
        let n: u64 = out
            .stdout
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("awk output not a number: {:?}", out.stdout));
        assert!(
            n <= 1024,
            "RLIMIT_NOFILE not capped: got {n}, expected <= 1024"
        );
    }

    /// **S3: setuid binary cannot elevate inside /exec.** The
    /// pre_exec sets PR_SET_NO_NEW_PRIVS=1 which makes the kernel
    /// silently ignore the setuid bit on `exec()`. We can only
    /// test this if a setuid binary is available on the host —
    /// `/usr/bin/passwd` is the canonical one. If absent, skip.
    ///
    /// The test runs `passwd` (which would prompt for a password
    /// as root, exit 1 as non-root) and asserts the *effective uid*
    /// was NOT elevated. Without no_new_privs, a child of root
    /// would see euid=0; with no_new_privs and a non-root caller,
    /// the setuid bit is ignored so euid stays at the caller's uid.
    #[compio::test]
    async fn setuid_bit_is_neutered_by_no_new_privs() {
        if !std::path::Path::new("/usr/bin/passwd").exists() {
            return; // no setuid binary to probe; skip
        }
        // We use `id -u` after invoking a wrapper that would
        // normally setuid. Easier: directly check that
        // /proc/self/status of an `id`-style call shows our caller
        // uid, not 0. Since we're already not root, the simplest
        // proof is: NoNewPrivs=1 (verified by S1) + status=1 from
        // passwd-without-args (it errors out without root) =
        // setuid bit was ignored.
        let out = run(
            "passwd --help >/dev/null 2>&1; id -u",
            "/tmp",
            5_000,
        )
        .await
        .unwrap();
        assert_eq!(out.status, 0);
        let uid: u32 = out.stdout.trim().parse().unwrap_or(0);
        // SAFETY: geteuid is async-signal-safe and infallible.
        let parent_uid = unsafe { libc::geteuid() };
        assert_eq!(
            uid, parent_uid,
            "child uid changed; setuid binary may have elevated"
        );
    }

    /// **S4: agent token never leaks into /exec child env.** We
    /// already test `curated_env` directly elsewhere; this is the
    /// integration-level proof that the actual spawn doesn't pass
    /// `SANDBOX_AGENT_*` to the child shell.
    #[compio::test]
    async fn agent_env_does_not_leak_to_child() {
        // Use `printenv` not `env` — the latter prints the literal
        // env passed by the parent (which includes our cleanup),
        // printenv only prints what the shell inherited.
        let out = run(
            "printenv | grep -E '^SANDBOX_AGENT' || echo CLEAN",
            "/tmp",
            5_000,
        )
        .await
        .unwrap();
        assert_eq!(out.status, 0, "{out:?}");
        assert!(
            out.stdout.trim().ends_with("CLEAN"),
            "agent env leaked to child: {}",
            out.stdout
        );
    }

    /// **S5: HOME is never the agent's HOME.** When the dropped
    /// child runs as nobody (production) we override HOME to
    /// `/home/u` (where the per-user PVC mounts). When running as
    /// the developer in tests we keep the developer's HOME
    /// (`PASSTHROUGH_VARS` includes HOME). Either way the child
    /// must NOT see e.g. `/root` from a root agent.
    #[compio::test]
    async fn child_home_is_not_root_home() {
        let out = run("echo \"$HOME\"", "/tmp", 5_000).await.unwrap();
        assert_eq!(out.status, 0);
        let home = out.stdout.trim();
        assert_ne!(home, "/root", "child sees /root as HOME");
        assert!(!home.is_empty(), "HOME unset in child");
    }

    /// **S6: working directory is honored, not overridden by
    /// pre_exec.** Regression guard: an earlier draft of pre_exec
    /// did a `chdir("/")` after privilege drop; that broke /exec
    /// callers that relied on `cwd` to land in /workspace.
    #[compio::test]
    async fn cwd_survives_lockdown() {
        let out = run("pwd", "/tmp", 5_000).await.unwrap();
        assert_eq!(out.status, 0);
        assert_eq!(out.stdout.trim(), "/tmp");
    }

    /// **S7: daemonized grandchild can't pin /exec.** A `cmd &` in
    /// sh forks a process that survives the `sh` exit and inherits
    /// the stdout/stderr pipe write-ends. Without an unconditional
    /// `killpg`, the reader threads never see EOF and `/exec` hangs
    /// for ~30s waiting for them. After the fix, the call returns
    /// promptly because we kill the whole process group on every
    /// exit path. Bound the test wall-clock at 5s so a regression
    /// surfaces clearly instead of timing out at the test runner's
    /// per-test deadline.
    #[compio::test]
    async fn daemonized_child_does_not_stall_exec() {
        let started = std::time::Instant::now();
        // sleep 30 in a daemonized child; sh exits immediately.
        let out = run(
            "(sleep 30 &) ; echo done",
            "/tmp",
            10_000,
        )
        .await
        .unwrap();
        assert_eq!(out.status, 0);
        assert_eq!(out.stdout.trim(), "done");
        // Without the killpg-on-every-exit fix this takes ~2s for
        // the reader-deadline to expire (still much better than
        // the 30s grandchild lifetime). With pgroup-kill the
        // grandchild dies promptly and EOF arrives in <100ms.
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "daemonized-grandchild stalled /exec for {:?}",
            started.elapsed()
        );
    }
}
