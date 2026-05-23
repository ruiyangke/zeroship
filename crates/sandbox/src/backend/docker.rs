//! Docker backend.
//!
//! Spawns a long-lived container per sandbox with the project's
//! workspace bind-mounted from the host. Shells out to the `docker`
//! CLI for lifecycle ops; performs file CRUD directly against the
//! host bind-mount path (no `docker exec` round-trip needed for
//! files — the workspace is the same file on both sides).
//!
//! ## Preview-URL support
//!
//! Each container now also receives a freshly-minted Ed25519 keypair
//! at create-time. The **public** key is written to
//! `<host_keys_dir>/<sandbox-id>/controller-pubkey` and bind-mounted
//! read-only at `/run/keys/` inside the container — same trust model
//! the K8s + nomad-ch backends already use. The signing key never
//! leaves this process. The agent inside the container reads
//! `/run/keys/controller-pubkey` at boot and verifies all signed
//! requests against it. The image is responsible for shipping the
//! agent binary (`zeroship-sandbox-agent`) and starting it on PID 1
//! or as a supervised process listening on `0.0.0.0:7777`. The
//! existing `docker exec` shell-out path is unchanged; the agent
//! adds the signed-RPC + preview surface.
//!
//! State per sandbox:
//!   - `container_name` (deterministic from sandbox id)
//!   - `container_id`   (returned by `docker run`)
//!   - `workspace_path` (host bind-mount, persists across container
//!     churn — keyed on `project_id`, so multiple sandbox re-opens
//!     reuse the same files)
//!   - `host_keys_dir`  (per-sandbox host dir holding the controller
//!                       pubkey; bind-mounted ro at `/run/keys` inside
//!                       the container; rm-rf'd on `stop`)
//!   - `signing_key`    (controller-side Ed25519 SK; matched verifying
//!                       key lives in the container; see § II.0.1)
//!   - `agent_url`      (`http://<container-bridge-ip>:7777`; computed
//!                       via `docker inspect` after `docker run`)
//!
//! See `crates/sandbox/src/backend/mod.rs` for the contract.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use ed25519_dalek::SigningKey;
use uuid::Uuid;
use zeroship_sandbox_agent::sig;
use zeroship_sandbox_agent::AGENT_PORT;

use super::{ExecOutput, SandboxInfo, TreeEntry};
use crate::config::SandboxConfig;

#[derive(Debug)]
pub struct DockerBackend {
    cfg: SandboxConfig,
    /// Per-sandbox bookkeeping. Keyed by sandbox_id; populated on
    /// `create`, dropped on `stop`.
    pub(crate) state: Arc<RwLock<HashMap<Uuid, DockerSandbox>>>,
    /// Sealed-record persistence (`docs/proposals/sandbox-preview-urls.md` § II.0 §4). `None` when
    /// `SANDBOX_PERSIST_AUTH` is unset — backend operates exactly as
    /// it did before persistence wiring. When `Some`, `create()` seals the per-
    /// sandbox auth on success and `stop()` deletes the file. Both
    /// are best-effort: a seal/delete failure is logged, never fatal.
    pub(crate) persist: Option<Arc<crate::persist::Persistence>>,
}

/// Per-sandbox bookkeeping for the Docker backend. The
/// `signing_key` is private; the hand-rolled `Debug` impl below
/// elides it. Do NOT `#[derive(Debug)]` — that would dump the secret
/// into any panic backtrace / `dbg!()` call. ed25519-dalek's
/// `SigningKey` has no redacting Debug impl of its own.
pub(crate) struct DockerSandbox {
    /// Full container ID returned by `docker run -d`. Stashed for
    /// audit / debug; reads always go through `container_name`,
    /// which is deterministic from the sandbox UUID.
    #[allow(dead_code)]
    pub(crate) container_id: String,
    pub(crate) container_name: String,
    pub(crate) workspace_path: PathBuf,
    /// Host directory holding the controller pubkey, bind-mounted
    /// read-only at `/run/keys` inside the container. `rm -rf`'d on
    /// `stop`. `None` when the agent-launch path isn't enabled (no
    /// keys minted, no preview surface available for this sandbox).
    pub(crate) host_keys_dir: Option<PathBuf>,
    /// Per-sandbox controller-side Ed25519 signing key. Lives only in
    /// this process; the matching public key is bind-mounted into the
    /// container. `None` mirrors `host_keys_dir` — set together at
    /// `create()`-time on the agent path, otherwise both stay `None`.
    /// Wrapped in Arc so signed-RPC dispatch clones a refcount, not
    /// the 32-byte secret bytes (ed25519-dalek::SigningKey doesn't
    /// zeroize on drop).
    pub(crate) signing_key: Option<Arc<SigningKey>>,
    /// `http://<container-bridge-ip>:7777`. Computed after
    /// `docker run -d` via `docker inspect`. `None` when the
    /// agent-launch path is disabled.
    pub(crate) agent_url: Option<String>,
}

// SECRET-HYGIENE: signing_key MUST NEVER appear in Debug output. See
// the equivalent invariant in `nomad_ch.rs::NomadChSandbox`.
impl std::fmt::Debug for DockerSandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DockerSandbox")
            .field("container_id", &self.container_id)
            .field("container_name", &self.container_name)
            .field("workspace_path", &self.workspace_path)
            .field("host_keys_dir", &self.host_keys_dir)
            .field("agent_url", &self.agent_url)
            // signing_key intentionally omitted.
            .finish_non_exhaustive()
    }
}

impl DockerBackend {
    pub fn new(cfg: SandboxConfig, persist: Option<Arc<crate::persist::Persistence>>) -> Self {
        Self {
            cfg,
            state: Arc::new(RwLock::new(HashMap::new())),
            persist,
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
            tracing::info!(image = %self.cfg.image, "sandbox/docker pulling image");
            if let Err(e) = pull_image(&self.cfg.image).await {
                tracing::warn!(error = %e, "sandbox/docker pull failed (continuing — image may be local)");
            }
        }
        // Ensure the workspace root exists.
        std::fs::create_dir_all(&self.cfg.workspace_root)
            .map_err(|e| format!("create workspace_root: {e}"))?;
        Ok(())
    }

    pub async fn create(
        &self,
        sandbox_id: Uuid,
        user_id: &str,
        project_id: &str,
    ) -> Result<SandboxInfo, String> {
        // The Docker backend keeps the existing per-project bind-mount
        // model (workspace persists across sandboxes for the same
        // project on this host). Per-user package caches aren't yet
        // implemented for Docker; user_id is recorded on the sandbox
        // for parity with the K8s backend but doesn't currently
        // change the spawn shape. (Future: per-user host directory
        // bind-mounted at `/home/u`.)
        let container_name = format!("zsbx-{}", sandbox_id.simple());
        let workspace = self.cfg.workspace_root.join(project_id);
        std::fs::create_dir_all(&workspace)
            .map_err(|e| format!("create workspace: {e}"))?;

        // Mint a per-sandbox Ed25519 keypair (`docs/proposals/sandbox-preview-urls.md` § II.0).
        // The signing key never leaves this process; the matching
        // **public** key is written to a per-sandbox host directory
        // and bind-mounted read-only at `/run/keys` inside the
        // container. The agent reads `/run/keys/controller-pubkey`
        // on boot to populate its verifier (sandbox-agent::Verifier).
        let sk_bytes = random_key32()?;
        let signing_key = Arc::new(SigningKey::from_bytes(&sk_bytes));
        let pubkey = signing_key.verifying_key();

        let host_keys_dir = host_keys_dir_for(&self.cfg.workspace_root, sandbox_id);
        write_pubkey_file(&host_keys_dir, &B64.encode(pubkey.as_bytes()))?;

        let container_id = run_container(
            &self.cfg.image,
            &container_name,
            &self.cfg.network,
            &workspace,
            &host_keys_dir,
            self.cfg.memory_mb,
            self.cfg.cpus,
            project_id,
            &sandbox_id.to_string(),
        )
        .await
        .inspect_err(|_| {
            // Best-effort cleanup of the keys dir we just wrote so a
            // failed `docker run` doesn't leak controller-pubkey
            // material on the host. We never wrote the secret half
            // here — only the public key — but stale dirs accumulate
            // otherwise.
            let _ = std::fs::remove_dir_all(&host_keys_dir);
        })?;

        // Resolve the container's bridge IP so the controller can
        // reach the agent at `http://<ip>:7777`. On the default
        // `bridge` network, the controller (running on the host) has
        // L3 reach via the docker0 bridge. Custom networks behave
        // similarly so long as the controller has an interface on
        // the same network — operator's responsibility, identical to
        // nomad-ch's "controller has L3 to the tap subnets" model
        // (`docs/proposals/sandbox-preview-urls.md` § II.0.0).
        let agent_url = match inspect_container_ip(&container_name, &self.cfg.network).await {
            Ok(ip) => format!("http://{ip}:{AGENT_PORT}"),
            Err(e) => {
                // Don't fail create() on inspect failure — the
                // existing `docker exec` / file-CRUD paths still work
                // without an agent_url. Just log and leave the
                // signed-RPC / preview surface unavailable; the
                // session_auth lookup will surface a clean Err.
                tracing::warn!(
                    container = %container_name,
                    error = %e,
                    "sandbox/docker: could not resolve container IP (signed-RPC unavailable for this sandbox)"
                );
                String::new()
            }
        };

        let now = unix_now();
        let agent_url_opt = if agent_url.is_empty() {
            None
        } else {
            Some(agent_url.clone())
        };
        let signing_key_opt = agent_url_opt.as_ref().map(|_| signing_key.clone());
        let host_keys_dir_opt = if agent_url_opt.is_some() {
            Some(host_keys_dir.clone())
        } else {
            // Agent path didn't materialize — drop the keys dir so we
            // don't leave a controller-pubkey file lying around.
            let _ = std::fs::remove_dir_all(&host_keys_dir);
            None
        };

        self.state.write().unwrap().insert(
            sandbox_id,
            DockerSandbox {
                container_id: container_id.clone(),
                container_name: container_name.clone(),
                workspace_path: workspace.clone(),
                host_keys_dir: host_keys_dir_opt,
                signing_key: signing_key_opt.clone(),
                agent_url: agent_url_opt.clone(),
            },
        );

        // Seal the per-sandbox auth to disk (`docs/proposals/sandbox-preview-urls.md` § II.0 §4).
        // BEST-EFFORT: a seal failure does NOT fail create(). Only
        // the agent-launch path produces a sealable record (signing
        // key + agent_url both present); when `docker inspect`
        // couldn't resolve the container IP, we skip the seal — that
        // sandbox can't be restored after restart anyway because
        // there's nothing to probe. Docker records seal `agent_url`
        // (the bridge IP) since it's not deterministic from any
        // controller-side identifier.
        if let (Some(persist), Some(sk), Some(_url)) = (
            &self.persist,
            signing_key_opt.as_ref(),
            agent_url_opt.as_ref(),
        ) {
            // v3: secrets only.
            let record = crate::persist::SealedAuth {
                version: crate::persist::SEAL_VERSION,
                sandbox_id: sandbox_id.to_string(),
                signing_key_bytes: sk.to_bytes(),
                preview_secrets: None,
                boot_id: None,
            };
            if let Err(e) = persist.seal(sandbox_id, &record).await {
                tracing::warn!(
                    sandbox_id = %sandbox_id,
                    container = %container_name,
                    error = %e,
                    "sandbox/docker persist.seal failed (non-fatal; sandbox live, restart-restore unavailable for this record)"
                );
            }
        }

        Ok(SandboxInfo {
            sandbox_id: sandbox_id.to_string(),
            user_id: user_id.to_string(),
            project_id: project_id.to_string(),
            backend: "docker".to_string(),
            backend_hint: format!("container={container_name} id={}", short_id(&container_id)),
            created_at_secs: now,
            last_used_at_secs: now,
        })
    }

    pub async fn stop(&self, sandbox_id: Uuid) -> Result<(), String> {
        let sandbox = match self.state.write().unwrap().remove(&sandbox_id) {
            Some(s) => s,
            None => return Ok(()), // idempotent
        };
        // Best-effort: rm -rf the per-sandbox keys dir. Done before
        // the container stop so that even if `docker stop` errors out
        // we don't leak the per-sandbox host directory holding the
        // (public-only) controller pubkey. Mirrors the nomad-ch
        // backend's host_dir cleanup discipline (round-3 m1).
        if let Some(dir) = sandbox.host_keys_dir.as_ref() {
            if let Err(e) = std::fs::remove_dir_all(dir) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(
                        dir = ?dir,
                        error = %e,
                        "sandbox/docker stop: rm-rf keys dir failed (non-fatal)"
                    );
                }
            }
        }
        let stop_res = stop_container(&sandbox.container_name).await;

        // Delete the sealed record (`docs/proposals/sandbox-preview-urls.md` § II.0 §4). BEST-EFFORT:
        // delete failures are logged but never fail stop().
        if let Some(persist) = &self.persist {
            if let Err(e) = persist.delete(sandbox_id).await {
                tracing::warn!(
                    sandbox_id = %sandbox_id,
                    error = %e,
                    "sandbox/docker persist.delete failed (non-fatal)"
                );
            }
        }
        stop_res
    }

    pub async fn exec(
        &self,
        sandbox_id: Uuid,
        cmd: &str,
        cwd: Option<&str>,
        timeout_ms: Option<u64>,
    ) -> Result<ExecOutput, String> {
        let name = self.container_name(sandbox_id)?;
        exec_in_container(&name, cmd, cwd, timeout_ms).await
    }

    pub async fn read_file(&self, sandbox_id: Uuid, path: &str) -> Result<Vec<u8>, String> {
        let ws = self.workspace(sandbox_id)?;
        crate::files::read_file(&ws, path)
    }

    pub async fn write_file(
        &self,
        sandbox_id: Uuid,
        path: &str,
        body: &[u8],
    ) -> Result<(), String> {
        let ws = self.workspace(sandbox_id)?;
        crate::files::write_file(&ws, path, body)
    }

    pub async fn delete_file(&self, sandbox_id: Uuid, path: &str) -> Result<bool, String> {
        let ws = self.workspace(sandbox_id)?;
        crate::files::delete_file(&ws, path)
    }

    pub async fn file_tree(&self, sandbox_id: Uuid) -> Result<Vec<TreeEntry>, String> {
        let ws = self.workspace(sandbox_id)?;
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
            .ok_or_else(|| "sandbox not found in docker backend".to_string())
    }

    fn workspace(&self, id: Uuid) -> Result<PathBuf, String> {
        self.state
            .read()
            .unwrap()
            .get(&id)
            .map(|s| s.workspace_path.clone())
            .ok_or_else(|| "sandbox not found in docker backend".to_string())
    }

    /// Lift the per-sandbox auth material into a backend-agnostic
    /// envelope. See `super::SandboxAuth` for the contract.
    ///
    /// Returns `Err("sandbox not found in docker backend")` when the
    /// sandbox-id is unknown, and a distinct `Err("agent-launch path
    /// not enabled ...")` when the sandbox exists but the agent path
    /// failed to materialize (e.g. `docker inspect` couldn't resolve
    /// an IP at create-time). The controller's audit log uses the
    /// distinction to differentiate stale-id races from misconfigured
    /// docker networking.
    pub async fn session_auth(
        &self,
        sandbox_id: Uuid,
    ) -> Result<super::SandboxAuth, String> {
        let guard = self.state.read().unwrap();
        let s = guard
            .get(&sandbox_id)
            .ok_or_else(|| "sandbox not found in docker backend".to_string())?;
        let signing_key = s.signing_key.clone().ok_or_else(|| {
            "docker backend: agent-launch path not enabled for this \
             sandbox (no Ed25519 keypair minted; preview / signed-RPC \
             unavailable)"
                .to_string()
        })?;
        let agent_url = s.agent_url.clone().ok_or_else(|| {
            "docker backend: agent-launch path not enabled for this \
             sandbox (no agent_url; preview / signed-RPC unavailable)"
                .to_string()
        })?;
        let pubkey_fp = sig::pubkey_fingerprint(&signing_key.verifying_key());
        Ok(super::SandboxAuth {
            signing_key,
            agent_url,
            pubkey_fp,
        })
    }

    /// Persist-on-mint helper for the docker backend. See
    /// [`super::Backend::seal_with_preview_state`] for the contract.
    /// Returns `Ok(false)` when persistence is disabled OR when the
    /// agent-launch path didn't materialize for this sandbox (no
    /// signing_key / agent_url to seal).
    pub async fn seal_with_preview_state(
        &self,
        sandbox_id: Uuid,
        info: &super::SandboxInfo,
        secrets: Option<crate::persist::SealedPreviewSecrets>,
        audit: Vec<crate::persist::SealedAuditEntry>,
    ) -> Result<bool, String> {
        let Some(persist) = self.persist.clone() else {
            return Ok(false);
        };
        let _ = (info, audit); // round-8: legacy fields no longer sealed
        let sk_bytes = {
            let guard = self.state.read().unwrap();
            let Some(s) = guard.get(&sandbox_id) else {
                return Ok(false);
            };
            let Some(sk) = &s.signing_key else {
                return Ok(false);
            };
            sk.to_bytes()
        };
        // v3: secrets only.
        let record = crate::persist::SealedAuth {
            version: crate::persist::SEAL_VERSION,
            sandbox_id: sandbox_id.to_string(),
            signing_key_bytes: sk_bytes,
            preview_secrets: secrets,
            boot_id: None,
        };
        persist
            .seal(sandbox_id, &record)
            .await
            .map(|()| true)
            .map_err(|e| format!("seal failed: {e}"))
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
    keys_host_dir: &std::path::Path,
    memory_mb: u32,
    cpus: f32,
    project_id: &str,
    sandbox_id: &str,
) -> Result<String, String> {
    let memory = format!("{memory_mb}m");
    let cpus_s = format!("{cpus}");
    let workspace_arg = format!(
        "{}:/workspace",
        workspace_host
            .to_str()
            .ok_or_else(|| "workspace path not utf-8".to_string())?,
    );
    // Bind-mount the per-sandbox host keys dir read-only at
    // `/run/keys` inside the container — same path the agent expects
    // on K8s (ConfigMap-projected volume) and nomad-ch (virtio-fs
    // share). The `:ro` flag prevents the agent (or any in-container
    // process) from writing to it; the rust-side `write_pubkey_file`
    // already chmodded the host file to 0400. (`docs/proposals/sandbox-preview-urls.md` § II.0
    // key-share invariants table.)
    let keys_arg = format!(
        "{}:/run/keys:ro",
        keys_host_dir
            .to_str()
            .ok_or_else(|| "keys dir path not utf-8".to_string())?,
    );
    let label_sandbox = format!("zeroship.sandbox={sandbox_id}");
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
        "--volume", &keys_arg,
        "--label", &label_sandbox,
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

// ─── agent-launch helpers ───────────────────────────────────────

/// Per-sandbox host directory holding the controller pubkey.
///
/// Lives next to the workspace root rather than under the project
/// dir so a `rm -rf` of the per-sandbox dir on `stop` can't
/// accidentally walk into user files. Format:
/// `<workspace_root>/.zsbx-keys/<sandbox-id-simple>/`. The leading
/// dot makes it a hidden dir; the leading-dot prefix is also in the
/// nomad-ch backend's `host_state_dir` style, kept consistent.
fn host_keys_dir_for(workspace_root: &std::path::Path, sandbox_id: Uuid) -> PathBuf {
    workspace_root
        .join(".zsbx-keys")
        .join(sandbox_id.simple().to_string())
}

/// Write `controller-pubkey` (base64 of the 32-byte verifying key)
/// into `dir`, mode `0400`. Best-effort `fsync` so a host crash
/// during create doesn't leave a half-written file the next agent
/// boot can't parse.
fn write_pubkey_file(dir: &std::path::Path, pubkey_b64: &str) -> Result<(), String> {
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("create keys dir {dir:?}: {e}"))?;
    let path = dir.join("controller-pubkey");
    let mut f = std::fs::File::create(&path)
        .map_err(|e| format!("create {path:?}: {e}"))?;
    f.write_all(pubkey_b64.as_bytes())
        .map_err(|e| format!("write {path:?}: {e}"))?;
    // 0400 — read-only, owner-only. The container bind-mount is
    // additionally `:ro` so the in-container agent (UID 0 inside
    // the container by default) can't rewrite it from inside. See
    // `docs/proposals/sandbox-preview-urls.md` § II.0 key-share invariants table.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = f
            .metadata()
            .map_err(|e| format!("stat {path:?}: {e}"))?
            .permissions();
        perms.set_mode(0o400);
        std::fs::set_permissions(&path, perms)
            .map_err(|e| format!("chmod {path:?}: {e}"))?;
    }
    f.sync_all().map_err(|e| format!("fsync {path:?}: {e}"))?;
    Ok(())
}

/// Resolve the container's bridge IP via `docker inspect`. Returns
/// the first non-empty IPv4 across the container's networks; for the
/// default `bridge` network this is the docker0-bridge IP that the
/// host can reach directly.
async fn inspect_container_ip(name: &str, network: &str) -> Result<String, String> {
    // `--format` with a Go template that walks NetworkSettings.Networks
    // and emits the IPAddress. We try the named network first; fall
    // back to NetworkSettings.IPAddress (legacy single-net containers).
    let template = format!(
        "{{{{ with index .NetworkSettings.Networks \"{}\" }}}}{{{{ .IPAddress }}}}{{{{ end }}}}",
        network
    );
    let out = run_docker(&["inspect", "--format", &template, name]).await?;
    if out.status != 0 {
        return Err(format!(
            "docker inspect (network {network}) → status {}: {}",
            out.status,
            out.stderr.trim()
        ));
    }
    let ip = out.stdout.trim().to_string();
    if !ip.is_empty() {
        return Ok(ip);
    }
    // Fallback for the legacy `IPAddress` field (older docker, or a
    // container attached only to the default bridge).
    let out2 = run_docker(&[
        "inspect",
        "--format",
        "{{ .NetworkSettings.IPAddress }}",
        name,
    ])
    .await?;
    if out2.status != 0 {
        return Err(format!(
            "docker inspect (legacy IPAddress) → status {}: {}",
            out2.status,
            out2.stderr.trim()
        ));
    }
    let ip = out2.stdout.trim().to_string();
    if ip.is_empty() {
        return Err(format!(
            "docker inspect: container {name} has no IPv4 address \
             on network {network} (and no legacy IPAddress field)"
        ));
    }
    Ok(ip)
}

/// 32 bytes from `/dev/urandom` for an Ed25519 secret key. Same
/// rationale as the nomad-ch backend's identically-named helper:
/// the kernel CSPRNG is the right answer for one-shot key material;
/// shelling out via `getrandom` here would only add a syscall.
fn random_key32() -> Result<[u8; 32], String> {
    let mut buf = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .map_err(|e| format!("open /dev/urandom: {e}"))?
        .read_exact(&mut buf)
        .map_err(|e| format!("read /dev/urandom: {e}"))?;
    Ok(buf)
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

    /// Docker has no agent-launch path yet, so
    /// `session_auth` cleanly errors instead of silently misbehaving.
    /// Distinguishes "no such sandbox" from "no agent path on docker"
    /// so the controller's audit log says the right thing.
    #[compio::test]
    async fn session_auth_reports_no_agent_path() {
        use super::DockerBackend;
        use crate::config::{ApiToken, K8sConfig, NomadCHConfig, SandboxConfig};
        use ed25519_dalek::SigningKey;
        use std::path::PathBuf;
        use std::sync::Arc;
        use uuid::Uuid;
        use zeroship_sandbox_agent::sig;

        let cfg = SandboxConfig {
            port: 9091,
            token: ApiToken::new("x"),
            backend: "docker".into(),
            image: "alpine".into(),
            workspace_root: PathBuf::from("/tmp/zsbx-test"),
            network: "bridge".into(),
            memory_mb: 256,
            cpus: 1.0,
            idle_timeout_secs: 1800,
            max_lifetime_secs: 28800,
            auto_pull: false,
            k8s: K8sConfig {
                namespace: "default".into(),
                image: "i".into(),
                runtime_class: "kvm-sandbox".into(),
                ready_timeout_secs: 120,
                use_port_forward: false,
                port_forward_start: 18000,
                user_home_size: "5Gi".into(),
                user_home_storage_class: None,
                startup_orphan_cleanup: false,
            },
            nomad_ch: NomadCHConfig {
                nomad_addr: "http://127.0.0.1:4646".into(),
                datacenter: "dc1".into(),
                wrapper_path: PathBuf::from("/etc/zeroship/nomad-vm-wrapper.sh"),
                runtime_dir: PathBuf::from("/var/lib/zeroship/ch"),
                host_state_dir: PathBuf::from("/var/zeroship/ch"),
                user_home_dir_root: PathBuf::from("/var/zeroship/ch/users"),
                vm_index_floor: 1,
                vm_index_ceil: 155,
                alloc_running_timeout_secs: 120,
                agent_livez_timeout_secs: 30,
                host_fence_timeout_secs: 30,
                startup_orphan_cleanup: false,
                subnet_second_octet: 99,
            },
            create_retry_max: 2,
            create_retry_total_timeout_secs: 90,
            snapshot_enabled: false,
            snapshot_l1_root: std::path::PathBuf::from("/var/zeroship/ch/snapshots"),
            snapshot_use_gcs: false,
            snapshot_gcs_bucket: None,
            snapshot_root_kek_path: None,
            workspace_image_size_gb: 20,
        };
        let backend = DockerBackend::new(cfg, None);
        let id = Uuid::now_v7();

        // Unknown sandbox → "not found".
        let err = backend.session_auth(id).await.expect_err("missing");
        assert!(
            err.contains("not found"),
            "expected 'not found' for unknown id; got {err:?}"
        );

        // Insert a fake record with no agent path populated
        // (mirrors a `create()` where `docker inspect` failed to
        // resolve the container IP). Both fields are `None`, so
        // `session_auth` reports the no-agent-path error.
        backend.state.write().unwrap().insert(
            id,
            super::DockerSandbox {
                container_id: "abc123".into(),
                container_name: "zsbx-test".into(),
                workspace_path: PathBuf::from("/tmp/zsbx-test/p"),
                host_keys_dir: None,
                signing_key: None,
                agent_url: None,
            },
        );
        let err = backend.session_auth(id).await.expect_err("no agent path");
        assert!(
            err.contains("agent-launch path not enabled"),
            "expected the no-agent-path error message; got {err:?}"
        );

        // Now exercise the happy path — populate signing_key +
        // agent_url and assert `session_auth` returns the lifted
        // record with a stable pubkey_fp.
        let sk = Arc::new(SigningKey::from_bytes(&[5u8; 32]));
        let expected_fp = sig::pubkey_fingerprint(&sk.verifying_key());
        let agent_url = "http://172.17.0.4:7777".to_string();
        {
            let mut g = backend.state.write().unwrap();
            let s = g.get_mut(&id).expect("inserted");
            s.signing_key = Some(sk.clone());
            s.agent_url = Some(agent_url.clone());
            s.host_keys_dir = Some(PathBuf::from("/tmp/zsbx-test/.zsbx-keys/x"));
        }
        let auth = backend.session_auth(id).await.expect("happy path");
        assert_eq!(auth.agent_url, agent_url);
        assert_eq!(auth.pubkey_fp, expected_fp);
        assert!(Arc::ptr_eq(&auth.signing_key, &sk));
    }

    #[test]
    fn write_pubkey_file_writes_chmod_0400() {
        // Round-trip: write a known public key; assert file content
        // matches and (on Unix) the permissions are exactly 0400.
        // Regression guard: a future contributor relaxes the chmod
        // and the controller-pubkey becomes world-readable on a
        // multi-tenant host.
        let tmp = std::env::temp_dir()
            .join(format!("zsbx-docker-test-{}", uuid::Uuid::now_v7().simple()));
        super::write_pubkey_file(&tmp, "AAAA").expect("write");
        let path = tmp.join("controller-pubkey");
        let bytes = std::fs::read(&path).expect("read");
        assert_eq!(bytes, b"AAAA");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let perms = std::fs::metadata(&path).unwrap().permissions();
            assert_eq!(
                perms.mode() & 0o777,
                0o400,
                "controller-pubkey must be mode 0400 (owner read-only)"
            );
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn host_keys_dir_for_is_under_workspace_root() {
        use std::path::PathBuf;
        let id = uuid::Uuid::now_v7();
        let dir = super::host_keys_dir_for(&PathBuf::from("/var/zeroship"), id);
        // Hidden dir, sandbox-id-keyed, parent is workspace_root.
        assert_eq!(dir.parent().unwrap().file_name().unwrap(), ".zsbx-keys");
        assert_eq!(
            dir.file_name().unwrap().to_str().unwrap(),
            id.simple().to_string()
        );
    }
}
