//! Docker integration via shell-out.
//!
//! We invoke the `docker` CLI through `std::process::Command` on a
//! blocking thread (compio's `spawn_blocking`). This is the standard
//! pattern for interacting with the docker daemon from server code:
//! the CLI handles auth, TLS, environment overrides, container
//! lifecycle quirks, and image-pull semantics — none of which we
//! want to reimplement against the raw Engine API.
//!
//! Cost: each call costs one OS-thread-of-blocking work and one
//! `fork(2)+exec(2)` of the docker CLI (~5-15ms overhead). Fine for
//! an editor session that does a handful of ops per turn.

use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Debug)]
pub struct ExecOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

/// One-shot health probe at startup. Errors if Docker is unreachable
/// or if the user lacks permission to talk to the daemon.
pub async fn probe_docker() -> Result<(), String> {
    let out = run(&["version", "--format", "{{.Server.Version}}"]).await?;
    if out.status != 0 {
        return Err(format!("docker version → status {}: {}", out.status, out.stderr.trim()));
    }
    Ok(())
}

/// Spawn a long-lived container. Returns its container ID.
///
/// Args layout matches the spec exactly — see the "Sandbox service"
/// section. `--rm` so the container is gone the moment it stops;
/// `-d` so we don't block the API call on container startup; we
/// wait for the container to be running via a follow-up `inspect`.
#[allow(clippy::too_many_arguments)]
pub async fn run_container(
    image: &str,
    name: &str,
    network: &str,
    workspace_host: &Path,
    memory_mb: u32,
    cpus: f32,
    project_id: &str,
    session_id: &str,
) -> Result<String, String> {
    let memory = format!("{memory_mb}m");
    let cpus_s = format!("{cpus}");
    let workspace_arg = format!(
        "{}:/workspace",
        workspace_host.to_str().ok_or_else(|| "workspace path not utf-8".to_string())?,
    );
    let label_session = format!("zeroship.session={session_id}");
    let label_project = format!("zeroship.project={project_id}");

    let args: Vec<&str> = vec![
        "run", "-d", "--rm",
        "--name", name,
        "--network", network,
        "--memory", &memory,
        "--cpus", &cpus_s,
        "--pids-limit", "256",
        "--read-only",
        "--tmpfs", "/tmp:size=128m",
        "--tmpfs", "/root/.npm:size=256m",
        "--volume", &workspace_arg,
        "--label", &label_session,
        "--label", &label_project,
        "--workdir", "/workspace",
        image,
        "sleep", "infinity",
    ];

    let out = run(&args).await?;
    if out.status != 0 {
        return Err(format!("docker run → status {}: {}", out.status, out.stderr.trim()));
    }
    let id = out.stdout.trim().to_string();
    if id.is_empty() {
        return Err("docker run returned empty container id".to_string());
    }
    Ok(id)
}

/// Run a shell command inside a container. The command runs under
/// `sh -c` so the caller can use pipes, &&, redirection, etc.
pub async fn exec_in_container(
    container: &str,
    cmd: &str,
    cwd: Option<&str>,
    timeout_ms: Option<u64>,
) -> Result<ExecOutput, String> {
    let workdir_arg = cwd.map(|s| s.to_string()).unwrap_or_else(|| "/workspace".to_string());

    // We wrap the user's command in `timeout` so the child can't run
    // forever inside the container. The timeout binary lives in
    // coreutils which is in node:22-alpine via the busybox base.
    let wrapped = if let Some(ms) = timeout_ms {
        let secs = (ms / 1000).max(1);
        format!("timeout --foreground -k 2 {secs} sh -c {}", shell_quote(cmd))
    } else {
        format!("sh -c {}", shell_quote(cmd))
    };

    let args: Vec<&str> = vec!["exec", "-w", &workdir_arg, container, "sh", "-c", &wrapped];
    run(&args).await
}

/// Stop a container by name (or id). Idempotent — already-stopped
/// containers are reported as success.
pub async fn stop_container(container: &str) -> Result<(), String> {
    let out = run(&["stop", "-t", "5", container]).await?;
    if out.status != 0 {
        // Ignore "no such container" — same effect as success.
        if out.stderr.contains("No such container") || out.stderr.contains("is not running") {
            return Ok(());
        }
        return Err(format!("docker stop → status {}: {}", out.status, out.stderr.trim()));
    }
    Ok(())
}

/// Inspect a container; returns its IPv4 in the given network.
pub async fn container_ip(container: &str, network: &str) -> Result<String, String> {
    let format = format!("{{{{ (index .NetworkSettings.Networks \"{network}\").IPAddress }}}}");
    let out = run(&["inspect", "-f", &format, container]).await?;
    if out.status != 0 {
        return Err(format!("docker inspect → status {}: {}", out.status, out.stderr.trim()));
    }
    Ok(out.stdout.trim().to_string())
}

/// Pull an image. Slow; only invoked at startup when `auto_pull=true`.
pub async fn pull_image(image: &str) -> Result<(), String> {
    let out = run(&["pull", image]).await?;
    if out.status != 0 {
        return Err(format!("docker pull → status {}: {}", out.status, out.stderr.trim()));
    }
    Ok(())
}

/// Spawn `docker` with the given args on a blocking thread.
async fn run(args: &[&str]) -> Result<ExecOutput, String> {
    // Make an owned Vec<String> so the closure that runs on the
    // blocking thread doesn't borrow stack-bound slices.
    let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();

    compio::runtime::spawn_blocking(move || {
        let out = Command::new("docker")
            .args(&owned)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| format!("spawn docker: {e}"))?;

        Ok::<_, String>(ExecOutput {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    })
    .await
    .map_err(|e| format!("blocking task panic: {e:?}"))?
}

/// Wrap a shell argument in single quotes safely.
fn shell_quote(s: &str) -> String {
    // Per POSIX: replace ' with '\'' and surround with single quotes.
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

#[cfg(test)]
mod tests {
    use super::shell_quote;

    #[test]
    fn quote_plain() {
        assert_eq!(shell_quote("npm install"), "'npm install'");
    }

    #[test]
    fn quote_with_quotes() {
        assert_eq!(shell_quote("echo 'hi'"), r"'echo '\''hi'\'''");
    }

    #[test]
    fn quote_empty() {
        assert_eq!(shell_quote(""), "''");
    }
}
