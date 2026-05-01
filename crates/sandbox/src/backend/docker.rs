//! Docker backend.
//!
//! Spawns a long-lived container per session with the project's
//! workspace bind-mounted from the host. Shells out to the `docker`
//! CLI for lifecycle ops; performs file CRUD directly against the
//! host bind-mount path (no `docker exec` round-trip needed for
//! files — the workspace is the same file on both sides).
//!
//! State per session:
//!   - `container_name` (deterministic from session id)
//!   - `container_id`   (returned by `docker run`)
//!   - `workspace_path` (host bind-mount, persists across container
//!     churn — keyed on `project_id`, so multiple session re-opens
//!     reuse the same files)
//!
//! See `crates/sandbox/src/backend/mod.rs` for the contract.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use uuid::Uuid;

use super::{ExecOutput, SessionInfo, TreeEntry};
use crate::config::SandboxConfig;

#[derive(Debug)]
pub struct DockerBackend {
    cfg: SandboxConfig,
    /// Per-session bookkeeping. Keyed by session_id; populated on
    /// `create`, dropped on `stop`.
    state: Arc<RwLock<HashMap<Uuid, DockerSession>>>,
}

#[derive(Debug, Clone)]
struct DockerSession {
    /// Full container ID returned by `docker run -d`. Stashed for
    /// audit / debug; reads always go through `container_name`,
    /// which is deterministic from the session UUID.
    #[allow(dead_code)]
    container_id: String,
    container_name: String,
    workspace_path: PathBuf,
}

impl DockerBackend {
    pub fn new(cfg: SandboxConfig) -> Self {
        Self {
            cfg,
            state: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn probe(&self) -> Result<(), String> {
        let out = run_docker(&["version", "--format", "{{.Server.Version}}"]).await?;
        if out.status != 0 {
            return Err(format!(
                "docker version → status {}: {}",
                out.status,
                out.stderr.trim()
            ));
        }
        if self.cfg.auto_pull {
            eprintln!("[sandbox/docker] pulling {}...", self.cfg.image);
            if let Err(e) = pull_image(&self.cfg.image).await {
                eprintln!("[sandbox/docker] pull failed (continuing — image may be local): {e}");
            }
        }
        // Ensure the workspace root exists.
        std::fs::create_dir_all(&self.cfg.workspace_root)
            .map_err(|e| format!("create workspace_root: {e}"))?;
        Ok(())
    }

    pub async fn create(
        &self,
        session_id: Uuid,
        user_id: &str,
        project_id: &str,
    ) -> Result<SessionInfo, String> {
        // The Docker backend keeps the existing per-project bind-mount
        // model (workspace persists across sessions for the same
        // project on this host). Per-user package caches aren't yet
        // implemented for Docker; user_id is recorded on the session
        // for parity with the K8s backend but doesn't currently
        // change the spawn shape. (Future: per-user host directory
        // bind-mounted at `/home/u`.)
        let container_name = format!("zsbx-{}", session_id.simple());
        let workspace = self.cfg.workspace_root.join(project_id);
        std::fs::create_dir_all(&workspace)
            .map_err(|e| format!("create workspace: {e}"))?;

        let container_id = run_container(
            &self.cfg.image,
            &container_name,
            &self.cfg.network,
            &workspace,
            self.cfg.memory_mb,
            self.cfg.cpus,
            project_id,
            &session_id.to_string(),
        )
        .await?;

        let now = unix_now();
        self.state.write().unwrap().insert(
            session_id,
            DockerSession {
                container_id: container_id.clone(),
                container_name: container_name.clone(),
                workspace_path: workspace.clone(),
            },
        );

        Ok(SessionInfo {
            session_id: session_id.to_string(),
            user_id: user_id.to_string(),
            project_id: project_id.to_string(),
            backend: "docker".to_string(),
            backend_hint: format!("container={container_name} id={}", short_id(&container_id)),
            created_at_secs: now,
            last_used_at_secs: now,
        })
    }

    pub async fn stop(&self, session_id: Uuid) -> Result<(), String> {
        let session = match self.state.write().unwrap().remove(&session_id) {
            Some(s) => s,
            None => return Ok(()), // idempotent
        };
        stop_container(&session.container_name).await
    }

    pub async fn exec(
        &self,
        session_id: Uuid,
        cmd: &str,
        cwd: Option<&str>,
        timeout_ms: Option<u64>,
    ) -> Result<ExecOutput, String> {
        let name = self.container_name(session_id)?;
        exec_in_container(&name, cmd, cwd, timeout_ms).await
    }

    pub async fn read_file(&self, session_id: Uuid, path: &str) -> Result<Vec<u8>, String> {
        let ws = self.workspace(session_id)?;
        crate::files::read_file(&ws, path)
    }

    pub async fn write_file(
        &self,
        session_id: Uuid,
        path: &str,
        body: &[u8],
    ) -> Result<(), String> {
        let ws = self.workspace(session_id)?;
        crate::files::write_file(&ws, path, body)
    }

    pub async fn delete_file(&self, session_id: Uuid, path: &str) -> Result<bool, String> {
        let ws = self.workspace(session_id)?;
        crate::files::delete_file(&ws, path)
    }

    pub async fn file_tree(&self, session_id: Uuid) -> Result<Vec<TreeEntry>, String> {
        let ws = self.workspace(session_id)?;
        let entries = crate::files::file_tree(&ws)?;
        Ok(entries
            .into_iter()
            .map(|e| TreeEntry {
                path: e.path,
                kind: e.kind,
                size: e.size,
            })
            .collect())
    }

    // ─── lookup helpers ─────────────────────────────────────────

    fn container_name(&self, id: Uuid) -> Result<String, String> {
        self.state
            .read()
            .unwrap()
            .get(&id)
            .map(|s| s.container_name.clone())
            .ok_or_else(|| "session not found in docker backend".to_string())
    }

    fn workspace(&self, id: Uuid) -> Result<PathBuf, String> {
        self.state
            .read()
            .unwrap()
            .get(&id)
            .map(|s| s.workspace_path.clone())
            .ok_or_else(|| "session not found in docker backend".to_string())
    }
}

// ─── docker CLI shell-outs ──────────────────────────────────────
//
// Same call shape as the original `docker.rs`. Kept module-local so
// the K8s backend doesn't accidentally import them.

#[derive(Debug)]
struct CliOutput {
    status: i32,
    stdout: String,
    stderr: String,
}

#[allow(clippy::too_many_arguments)]
async fn run_container(
    image: &str,
    name: &str,
    network: &str,
    workspace_host: &std::path::Path,
    memory_mb: u32,
    cpus: f32,
    project_id: &str,
    session_id: &str,
) -> Result<String, String> {
    let memory = format!("{memory_mb}m");
    let cpus_s = format!("{cpus}");
    let workspace_arg = format!(
        "{}:/workspace",
        workspace_host
            .to_str()
            .ok_or_else(|| "workspace path not utf-8".to_string())?,
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

    let out = run_docker(&args).await?;
    if out.status != 0 {
        return Err(format!(
            "docker run → status {}: {}",
            out.status,
            out.stderr.trim()
        ));
    }
    let id = out.stdout.trim().to_string();
    if id.is_empty() {
        return Err("docker run returned empty container id".to_string());
    }
    Ok(id)
}

async fn exec_in_container(
    container: &str,
    cmd: &str,
    cwd: Option<&str>,
    timeout_ms: Option<u64>,
) -> Result<ExecOutput, String> {
    let workdir = cwd.unwrap_or("/workspace").to_string();
    // Wrap in `timeout` to bound runaway commands. BusyBox-friendly
    // flags so this works on alpine-based images.
    let wrapped = if let Some(ms) = timeout_ms {
        let secs = (ms / 1000).max(1);
        format!("timeout -k 2 {secs} sh -c {}", shell_quote(cmd))
    } else {
        format!("sh -c {}", shell_quote(cmd))
    };
    let args: Vec<&str> = vec!["exec", "-w", &workdir, container, "sh", "-c", &wrapped];
    let out = run_docker(&args).await?;
    // BusyBox `timeout` returns 124 (or 137 on -k SIGKILL) when the
    // wall clock fires — surface that to the caller.
    let timed_out = matches!(out.status, 124 | 137);
    Ok(ExecOutput {
        status: out.status,
        stdout: out.stdout,
        stderr: out.stderr,
        timed_out,
    })
}

async fn stop_container(container: &str) -> Result<(), String> {
    let out = run_docker(&["stop", "-t", "5", container]).await?;
    if out.status != 0 {
        if out.stderr.contains("No such container") || out.stderr.contains("is not running") {
            return Ok(());
        }
        return Err(format!(
            "docker stop → status {}: {}",
            out.status,
            out.stderr.trim()
        ));
    }
    Ok(())
}

async fn pull_image(image: &str) -> Result<(), String> {
    let out = run_docker(&["pull", image]).await?;
    if out.status != 0 {
        return Err(format!(
            "docker pull → status {}: {}",
            out.status,
            out.stderr.trim()
        ));
    }
    Ok(())
}

async fn run_docker(args: &[&str]) -> Result<CliOutput, String> {
    let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
    compio::runtime::spawn_blocking(move || {
        let out = Command::new("docker")
            .args(&owned)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| format!("spawn docker: {e}"))?;
        Ok::<_, String>(CliOutput {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    })
    .await
    .map_err(|e| format!("blocking task panic: {e:?}"))?
}

// ─── small utilities ────────────────────────────────────────────

fn shell_quote(s: &str) -> String {
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn short_id(id: &str) -> &str {
    if id.len() > 12 { &id[..12] } else { id }
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

    #[test]
    fn short_id_shortens() {
        assert_eq!(super::short_id("abcdef0123456789"), "abcdef012345");
        assert_eq!(super::short_id("short"), "short");
    }
}
