//! Kubernetes + libkrun backend.
//!
//! Drives `zeroship-sandbox-agent` Pods running under the
//! `kvm-sandbox` RuntimeClass (crun + libkrun microVM). Each sandbox
//! gets:
//!
//!   - A fresh **Ed25519 keypair** minted by the controller. The
//!     signing key never leaves this process.
//!   - A `ConfigMap` carrying the **public key only**, mounted
//!     read-only at `/run/keys/controller-pubkey` inside the Pod.
//!   - A Pod with `runtimeClassName: kvm-sandbox` and the
//!     `run.oci.handler: krun` annotation, running the agent binary
//!     as PID 1.
//!   - A reachable `agent_url` that routes to the in-VM agent's
//!     port 7777. In-cluster: the Pod IP. Local-dev (controller on
//!     host): a per-sandbox `kubectl port-forward` subprocess on a
//!     loopback port.
//!
//! Lifecycle ops then talk to the agent over HTTP, signed with the
//! sandbox's signing key. File operations go via `/files/*`,
//! commands via `/exec`, file tree via `/tree`. The agent's verifier
//! enforces HMAC-style replay protection (5 s skew + 30 s nonce LRU).
//!
//! ## Why shell-out instead of `kube-rs`
//!
//! `kube-rs` requires Tokio. The workspace is zero-tokio (compio /
//! io_uring). For now we shell-out to `kubectl`, run on
//! `compio::runtime::spawn_blocking`. This is what the e2e/exploit
//! examples already do; the controller uses the same machinery.
//!
//! ## Network access for local dev
//!
//! When `cfg.k8s.use_port_forward = true` (default for dev), each
//! sandbox spawns a `kubectl port-forward pod/<pod> <local>:7777`
//! background process and routes traffic through it. A simple
//! atomic counter picks unique loopback ports per sandbox. The
//! port-forward is killed on `stop`.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use ed25519_dalek::SigningKey;
use uuid::Uuid;
use zeroship_sandbox_agent::sig;

use super::{ExecOutput, SandboxInfo, TreeEntry};
use crate::config::SandboxConfig;

#[derive(Debug)]
pub struct K8sBackend {
    cfg: SandboxConfig,
    /// Per-sandbox bookkeeping. Populated on `create`, cleared on
    /// `stop`. Mutex-not-RwLock because writes (create/stop) and
    /// reads (every op) are at similar frequency and the contention
    /// is bounded.
    state: Arc<RwLock<HashMap<Uuid, K8sSandbox>>>,
    /// Loopback port allocator for `kubectl port-forward`. The port
    /// space is small (47k usable: 18000..=65535 minus a buffer);
    /// without a free-list we'd wrap and collide after enough churn.
    /// `Mutex<Ports>` is fine — only `create`/`stop` touch it,
    /// neither is a hot path.
    ports: Arc<Mutex<PortAllocator>>,
    /// Per-user serialization gate for `create` to prevent two
    /// concurrent creates from racing the "one active sandbox per
    /// user" check (which the RWO PVC also enforces, but slowly,
    /// via Pending Pods — we want to fail fast at the controller).
    creating_users: Arc<Mutex<HashSet<String>>>,
    /// `/readyz`-style flag, flipped by [`probe`] at startup +
    /// optionally by a background re-probe. `false` = backend not
    /// usable; consumers can route around or page out.
    healthy: Arc<AtomicBool>,
    /// Sealed-record persistence (preview-URL § II.0 §4). See
    /// [`crate::persist::Persistence`]. `None` when
    /// `SANDBOX_PERSIST_AUTH` is unset.
    persist: Option<Arc<crate::persist::Persistence>>,
}

/// Free-list-backed port allocator for `kubectl port-forward`.
/// Hands out the smallest free port ≥ `floor` (configurable via
/// `SANDBOX_K8S_PORT_FORWARD_START`); reclaims released ports so
/// we don't wrap around 65535 and collide.
#[derive(Debug)]
struct PortAllocator {
    floor: u16,
    /// Highest port we've ever allocated; new allocs prefer the
    /// `freed` list, fall back to `next = max(floor, prev+1)`.
    next: u16,
    /// Returned ports, sorted; smallest one is reused first.
    freed: BTreeSet<u16>,
}

impl PortAllocator {
    fn new(floor: u16) -> Self {
        Self {
            floor,
            next: floor,
            freed: BTreeSet::new(),
        }
    }

    fn alloc(&mut self) -> Result<u16, String> {
        if let Some(&p) = self.freed.iter().next() {
            self.freed.remove(&p);
            return Ok(p);
        }
        if self.next == u16::MAX {
            return Err("port-forward allocator exhausted (65535 cap)".into());
        }
        let p = self.next;
        self.next = self.next.saturating_add(1);
        Ok(p)
    }

    fn release(&mut self, p: u16) {
        if p >= self.floor {
            self.freed.insert(p);
        }
    }
}

struct K8sSandbox {
    user_id: String,
    pod_name: String,
    configmap_name: String,
    /// Per-user PVC currently mounted at `/home/u`. Persists across
    /// sandboxes for the same user — we record the name so we know
    /// which PVC to expect in the cluster, but **never delete it**
    /// on sandbox stop. PVC lifecycle is tied to user lifecycle, not
    /// sandbox lifecycle.
    user_home_pvc: String,
    namespace: String,
    /// Base URL the controller uses to reach the agent. Either
    /// `http://<pod-ip>:7777` (in-cluster) or
    /// `http://127.0.0.1:<local>` (port-forward).
    agent_url: String,
    /// Per-sandbox signing key. Lives only in this process; never
    /// touches the cluster.
    ///
    /// **Wrapped in Arc** so signed-RPC dispatch can clone a refcount
    /// (cheap) instead of the 32-byte secret bytes (which would mean
    /// two heap copies of the secret coexisting during every signed
    /// request, since `ed25519_dalek::SigningKey` doesn't zeroize on
    /// drop).
    signing_key: Arc<SigningKey>,
    /// Background `kubectl port-forward` subprocess if enabled, kept
    /// alive for the sandbox lifetime. Killed on stop. The local
    /// port is recorded so `stop` can return it to the allocator.
    /// `Child` owned exclusively by the HashMap entry — no `Mutex`
    /// needed since `state.write().remove(...)` is the only access
    /// point post-create (the watchdog uses a separate Arc handle).
    port_forward: Option<Child>,
    port_forward_local_port: Option<u16>,
}

impl std::fmt::Debug for K8sSandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("K8sSandbox")
            .field("user_id", &self.user_id)
            .field("pod_name", &self.pod_name)
            .field("configmap_name", &self.configmap_name)
            .field("user_home_pvc", &self.user_home_pvc)
            .field("namespace", &self.namespace)
            .field("agent_url", &self.agent_url)
            .field("port_forward", &self.port_forward.is_some())
            .field("port_forward_local_port", &self.port_forward_local_port)
            .finish_non_exhaustive()
    }
}

impl K8sBackend {
    pub fn new(
        cfg: SandboxConfig,
        persist: Option<Arc<crate::persist::Persistence>>,
    ) -> Result<Self, String> {
        let ports = PortAllocator::new(cfg.k8s.port_forward_start);
        Ok(Self {
            cfg,
            state: Arc::new(RwLock::new(HashMap::new())),
            ports: Arc::new(Mutex::new(ports)),
            creating_users: Arc::new(Mutex::new(HashSet::new())),
            healthy: Arc::new(AtomicBool::new(false)),
            persist,
        })
    }

    /// Whether the backend looks healthy (kubectl reachable,
    /// namespace exists). Set by [`probe`] at startup and refreshed
    /// by an optional background loop. Read by handlers / `/readyz`.
    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    pub async fn probe(&self) -> Result<(), String> {
        // Make sure kubectl exists and the cluster is reachable.
        let out = run_kubectl(&["version", "--output=json"]).await?;
        if out.status != 0 {
            self.healthy.store(false, Ordering::Relaxed);
            return Err(format!(
                "kubectl version → status {}: {}",
                out.status,
                out.stderr.trim()
            ));
        }
        // Make sure the namespace exists.
        let ns = self.cfg.k8s.namespace.clone();
        let out = run_kubectl(&["get", "namespace", &ns, "--no-headers"]).await?;
        if out.status != 0 {
            self.healthy.store(false, Ordering::Relaxed);
            return Err(format!(
                "kubectl get namespace {ns}: {}",
                out.stderr.trim()
            ));
        }
        self.healthy.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Best-effort cleanup of any agent Pods + ConfigMaps left over
    /// from a previous controller process. Called once at startup —
    /// the in-memory `state` is rebuilt from scratch each launch and
    /// the per-sandbox signing keys live only in process memory, so
    /// any orphan Pod's published pubkey is now a key the controller
    /// no longer holds. Those Pods would 401 every signed request
    /// forever; better to delete them and let the user re-create.
    /// PVCs (per-user) are intentionally NOT deleted — they're
    /// long-lived data.
    ///
    /// **Gated behind `SANDBOX_K8S_STARTUP_ORPHAN_CLEANUP`.** This
    /// label-selector deletes every sandbox-agent Pod in the
    /// namespace; with multiple controller replicas (HA), the first
    /// to start nukes every other replica's active sandboxes. The
    /// flag defaults to `false` so HA is safe by default; single-
    /// replica operators must opt in. For HA, run a separate prune
    /// job that uses Pod age + a per-controller-instance Lease.
    pub async fn cleanup_orphans_at_startup(&self) -> Result<usize, String> {
        if !self.cfg.k8s.startup_orphan_cleanup {
            return Ok(0);
        }
        let ns = self.cfg.k8s.namespace.clone();
        // List all sandbox-agent Pods + ConfigMaps in the namespace.
        let pods = run_kubectl(&[
            "get", "pods", "-n", &ns, "-l", "app.kubernetes.io/name=sandbox-agent",
            "-o", "name",
        ])
        .await?;
        let cms = run_kubectl(&[
            "get", "configmaps", "-n", &ns, "-l", "app.kubernetes.io/name=sandbox-agent",
            "-o", "name",
        ])
        .await?;
        let mut deleted = 0usize;
        for line in pods.stdout.lines().filter(|l| !l.trim().is_empty()) {
            let _ = run_kubectl(&[
                "delete", line, "-n", &ns, "--ignore-not-found",
                "--grace-period=0", "--force",
            ])
            .await;
            deleted += 1;
        }
        for line in cms.stdout.lines().filter(|l| !l.trim().is_empty()) {
            let _ = run_kubectl(&["delete", line, "-n", &ns, "--ignore-not-found"]).await;
            deleted += 1;
        }
        if deleted > 0 {
            eprintln!(
                "[sandbox/k8s] cleanup_orphans_at_startup: deleted {deleted} orphaned object(s)"
            );
        }
        Ok(deleted)
    }

    pub async fn create(
        &self,
        sandbox_id: Uuid,
        user_id: &str,
        project_id: &str,
    ) -> Result<SandboxInfo, String> {
        validate_id(user_id, "user_id")?;
        validate_id(project_id, "project_id")?;

        // M7 parity with NomadCHBackend. Refuse new sandboxes if
        // the most recent probe failed — kubectl/apiserver outages
        // would otherwise queue every concurrent create on the
        // spawn_blocking pool. Terminal: probe loop is what flips
        // healthy back, callers should not retry.
        if !self.is_healthy() {
            return Err(
                "k8s backend unhealthy; refusing new sandboxes \
                 (probe failed — kubectl unreachable or namespace \
                 missing). The probe loop will flip the bit back \
                 when the cluster recovers."
                    .to_string(),
            );
        }

        // **Per-user serialization.** Two concurrent creates for the
        // same user racing the "one active sandbox per user" check
        // would each see no existing → both apply Pods → second
        // sticks Pending on the RWO PVC, registry forgets the
        // first. Serialize with an in-flight set; non-blocking
        // claim, fail-fast on collision so the caller can retry.
        {
            let mut creating = self.creating_users.lock().unwrap();
            if !creating.insert(user_id.to_string()) {
                return Err(format!(
                    "concurrent sandbox create in progress for user {user_id:?}; retry"
                ));
            }
        }
        // Always release on exit, success or failure.
        let release_creating =
            ReleaseCreating::new(self.creating_users.clone(), user_id.to_string());

        // **One active sandbox per user.** The per-user PVC is
        // ReadWriteOnce; if this user already has a Pod holding
        // the lock, the new Pod stays Pending. Stop the existing
        // sandbox first.
        let existing: Vec<Uuid> = self
            .state
            .read()
            .unwrap()
            .iter()
            .filter(|(_, s)| s.user_id == user_id)
            .map(|(id, _)| *id)
            .collect();
        for old_id in existing {
            eprintln!(
                "[sandbox/k8s] user {user_id} already has sandbox {old_id}; stopping first"
            );
            if let Err(e) = self.stop(old_id).await {
                eprintln!("[sandbox/k8s] stop({old_id}) failed: {e}");
            }
        }

        // Per-step bookkeeping for cleanup-on-error. Each successful
        // step records what it created so the failure tail can tear
        // it down. PVC is intentionally NOT tracked here — it's
        // user-scoped + idempotent + persistent.
        let pod_name = format!("zsbx-{}", sandbox_id.simple());
        let configmap_name = format!("{pod_name}-trust");
        let ns = self.cfg.k8s.namespace.clone();
        let user_home_pvc = user_pvc_name(user_id);

        let mut guard = CreateGuard::new(
            self.ports.clone(),
            ns.clone(),
            pod_name.clone(),
            configmap_name.clone(),
        );

        let result = self
            .try_create(
                sandbox_id,
                user_id,
                project_id,
                &pod_name,
                &configmap_name,
                &ns,
                &user_home_pvc,
                &mut guard,
            )
            .await;

        match result {
            Ok(info) => {
                guard.disarm();
                drop(release_creating);
                Ok(info)
            }
            Err(e) => {
                drop(release_creating);
                // `guard` runs on drop, doing best-effort cleanup
                // of whatever steps did succeed (port-forward,
                // Pod, ConfigMap). PVC is left alone.
                Err(e)
            }
        }
    }

    /// The fallible portion of `create`, factored out so we can use
    /// `?`-style early returns and have `CreateGuard::Drop` clean
    /// up partial state on failure.
    #[allow(clippy::too_many_arguments)]
    async fn try_create(
        &self,
        sandbox_id: Uuid,
        user_id: &str,
        project_id: &str,
        pod_name: &str,
        configmap_name: &str,
        ns: &str,
        user_home_pvc: &str,
        guard: &mut CreateGuard,
    ) -> Result<SandboxInfo, String> {
        // 1. Mint Ed25519 keypair. Only the public key leaves this process.
        //    Wrap in Arc immediately so wait_for_agent_livez can sign
        //    /version probes (FM-A parity) without taking ownership;
        //    moved into the state map verbatim at commit-time.
        let sk_bytes = random_key32()?;
        let signing_key = Arc::new(SigningKey::from_bytes(&sk_bytes));
        let pubkey = signing_key.verifying_key();
        let pubkey_b64 = B64.encode(pubkey.as_bytes());
        let key_fp = sig::pubkey_fingerprint(&pubkey);

        // 2. Ensure per-user PVC. Idempotent.
        ensure_user_home_pvc(
            user_home_pvc,
            ns,
            user_id,
            &self.cfg.k8s.user_home_size,
            self.cfg.k8s.user_home_storage_class.as_deref(),
        )
        .await?;

        // 3. Apply ConfigMap (public key).
        apply_pubkey_configmap(configmap_name, ns, &pubkey_b64).await?;
        guard.configmap_created = true;

        // 4. Apply Pod with trust ConfigMap + per-user PVC.
        apply_agent_pod(
            pod_name,
            ns,
            &self.cfg.k8s.image,
            &self.cfg.k8s.runtime_class,
            self.cfg.memory_mb,
            self.cfg.cpus,
            configmap_name,
            user_home_pvc,
            user_id,
            project_id,
            &sandbox_id.to_string(),
        )
        .await?;
        guard.pod_created = true;

        // 5. Wait for Pod Ready.
        wait_pod_ready(pod_name, ns, self.cfg.k8s.ready_timeout_secs).await?;

        // 6. Resolve agent URL — port-forward (dev) or pod IP (prod).
        let (agent_url, pf, local_port) = if self.cfg.k8s.use_port_forward {
            let port = self.ports.lock().unwrap().alloc()?;
            let pf = start_port_forward(pod_name, ns, port)?;
            // Track immediately so cleanup can kill it on later failure.
            guard.port_forward_local_port = Some(port);
            let url = format!("http://127.0.0.1:{port}");
            wait_for_agent_livez(&url, &key_fp, &signing_key, Duration::from_secs(20)).await?;
            (url, Some(pf), Some(port))
        } else {
            let ip = pod_ip(pod_name, ns).await?;
            let url = format!("http://{ip}:7777");
            wait_for_agent_livez(&url, &key_fp, &signing_key, Duration::from_secs(20)).await?;
            (url, None, None)
        };
        // Move the Child into the guard so cleanup-on-error can
        // kill it; we'll take it back on success.
        guard.port_forward = pf;

        // 7. Commit. Take the port-forward back out of the guard.
        let port_forward = guard.port_forward.take();
        let agent_url_for_seal = agent_url.clone();
        let signing_key_for_seal = signing_key.clone();
        let sandbox = K8sSandbox {
            user_id: user_id.to_string(),
            pod_name: pod_name.to_string(),
            configmap_name: configmap_name.to_string(),
            user_home_pvc: user_home_pvc.to_string(),
            namespace: ns.to_string(),
            agent_url,
            // signing_key was already wrapped in Arc at step 1 so the
            // FM-A /version probe inside wait_for_agent_livez could
            // borrow it; move into the state map verbatim.
            signing_key,
            port_forward,
            port_forward_local_port: local_port,
        };
        self.state.write().unwrap().insert(sandbox_id, sandbox);

        let now = unix_now();

        // Seal the per-sandbox auth to disk (preview-URL § II.0 §4).
        // BEST-EFFORT: a seal failure does NOT fail create(). K8s
        // records seal `agent_url` directly because it's not a
        // deterministic function of any controller-side index — it's
        // either the Pod IP (in-cluster) or a port-forward loopback
        // address (dev). The restart-restore path (when implemented
        // for k8s — see TODO in restore_from_sealed) reads it back.
        if let Some(persist) = &self.persist {
            let record = crate::persist::SealedAuth {
                version: crate::persist::SEAL_VERSION,
                sandbox_id: sandbox_id.to_string(),
                user_id: user_id.to_string(),
                project_id: project_id.to_string(),
                backend: "k8s".to_string(),
                signing_key_bytes: signing_key_for_seal.to_bytes(),
                vm_index: None,
                agent_url: Some(agent_url_for_seal),
                pubkey_fp: key_fp.clone(),
                created_at_secs: now,
                preview_secrets: None,
                preview_audit: Vec::new(),
            };
            if let Err(e) = persist.seal(sandbox_id, &record).await {
                eprintln!(
                    "[sandbox/k8s] persist.seal failed sandbox={sandbox_id} \
                     pod={pod_name} (non-fatal; sandbox live, restart-restore \
                     unavailable for this record): {e}"
                );
            }
        }

        Ok(SandboxInfo {
            sandbox_id: sandbox_id.to_string(),
            user_id: user_id.to_string(),
            project_id: project_id.to_string(),
            backend: "k8s".to_string(),
            backend_hint: format!("pod={pod_name} key_fp={key_fp}"),
            created_at_secs: now,
            last_used_at_secs: now,
        })
    }

    pub async fn stop(&self, sandbox_id: Uuid) -> Result<(), String> {
        let sandbox = match self.state.write().unwrap().remove(&sandbox_id) {
            Some(s) => s,
            None => return Ok(()), // idempotent
        };
        let pod_name = sandbox.pod_name.clone();
        let cm_name = sandbox.configmap_name.clone();
        let ns = sandbox.namespace.clone();
        let mut errs: Vec<String> = Vec::new();

        // 1. Drain the agent FIRST, while the port-forward is still
        //    alive. /shutdown flips the agent's drain flag so the
        //    Pod stops accepting new requests; existing in-flight
        //    requests still complete (until ntex's shutdown_timeout
        //    cuts them off). Best-effort — if this fails the Pod is
        //    deleted in step 3 anyway.
        if let Err(e) = http_signed_async(
            &sandbox.signing_key,
            "POST",
            &format!("{}/shutdown", sandbox.agent_url),
            &[],
        )
        .await
        {
            eprintln!("[sandbox/k8s] /shutdown to {pod_name} failed (continuing): {e}");
        }

        // 2. Kill the port-forward (after the drain RPC, before we
        //    delete the Pod — the kubectl proxy would error out
        //    once the Pod is gone, leaving an orphan process).
        //    Child::wait blocks; off the ntex worker.
        if let Some(mut child) = sandbox.port_forward {
            let _ = compio::runtime::spawn_blocking(move || {
                let _ = child.kill();
                let _ = child.wait();
            })
            .await;
        }
        if let Some(p) = sandbox.port_forward_local_port {
            self.ports.lock().unwrap().release(p);
        }

        // 3. Pod + ConfigMap deletion. Correctness-critical: leaking
        //    either means cluster-side garbage that nothing else
        //    cleans up. Surface failures.
        if let Err(e) = delete_pod(&pod_name, &ns).await {
            errs.push(format!("delete_pod({pod_name}): {e}"));
        }
        if let Err(e) = delete_configmap(&cm_name, &ns).await {
            errs.push(format!("delete_configmap({cm_name}): {e}"));
        }
        // 4. Wait for kubelet to actually release the PVC before
        //    returning; otherwise a follow-up create for the same
        //    user races a Multi-Attach error. Best-effort: 30s.
        if let Err(e) = wait_for_pod_gone(&pod_name, &ns, Duration::from_secs(30)).await {
            errs.push(format!("wait_for_pod_gone({pod_name}): {e}"));
        }

        // 5. Delete the sealed record (preview-URL § II.0 §4).
        //    BEST-EFFORT: delete failures are logged but never fail
        //    stop().
        if let Some(persist) = &self.persist {
            if let Err(e) = persist.delete(sandbox_id).await {
                eprintln!(
                    "[sandbox/k8s] persist.delete failed sandbox={sandbox_id} \
                     pod={pod_name} (non-fatal): {e}"
                );
            }
        }

        if errs.is_empty() {
            Ok(())
        } else {
            Err(errs.join("; "))
        }
    }

    pub async fn exec(
        &self,
        sandbox_id: Uuid,
        cmd: &str,
        cwd: Option<&str>,
        timeout_ms: Option<u64>,
    ) -> Result<ExecOutput, String> {
        let (sk, url) = self.sandbox_keys(sandbox_id)?;
        let body = serde_json::json!({
            "cmd": cmd,
            "cwd": cwd,
            "timeout_ms": timeout_ms,
        })
        .to_string();
        let resp = http_signed_async(&sk, "POST", &format!("{url}/exec"), body.as_bytes()).await?;
        if resp.status != 200 {
            return Err(format!("agent /exec status {}: {}", resp.status, resp.body));
        }
        let v: serde_json::Value = serde_json::from_str(&resp.body)
            .map_err(|e| format!("agent /exec response not JSON: {e}"))?;
        Ok(ExecOutput {
            // try_into instead of `as i32` — a status outside i32
            // range is almost certainly garbage from a buggy agent;
            // falling back to -1 is no worse than the previous
            // wrap-on-cast and avoids signed-overflow surprises.
            status: v["status"]
                .as_i64()
                .unwrap_or(-1)
                .try_into()
                .unwrap_or(-1),
            stdout: v["stdout"].as_str().unwrap_or("").to_string(),
            stderr: v["stderr"].as_str().unwrap_or("").to_string(),
            timed_out: v["timed_out"].as_bool().unwrap_or(false),
        })
    }

    pub async fn read_file(&self, sandbox_id: Uuid, path: &str) -> Result<Vec<u8>, String> {
        let (sk, url) = self.sandbox_keys(sandbox_id)?;
        let p = sanitize_path(path)?;
        let resp = http_signed_async(&sk, "GET", &format!("{url}/files/{p}"), &[]).await?;
        if resp.status == 404 {
            return Err(format!("file not found: {p}"));
        }
        if resp.status != 200 {
            return Err(format!("agent /files GET status {}: {}", resp.status, resp.body));
        }
        Ok(resp.bytes)
    }

    pub async fn write_file(
        &self,
        sandbox_id: Uuid,
        path: &str,
        body: &[u8],
    ) -> Result<(), String> {
        let (sk, url) = self.sandbox_keys(sandbox_id)?;
        let p = sanitize_path(path)?;
        let resp = http_signed_async(&sk, "PUT", &format!("{url}/files/{p}"), body).await?;
        if resp.status != 200 {
            return Err(format!("agent /files PUT status {}: {}", resp.status, resp.body));
        }
        Ok(())
    }

    pub async fn delete_file(&self, sandbox_id: Uuid, path: &str) -> Result<bool, String> {
        let (sk, url) = self.sandbox_keys(sandbox_id)?;
        let p = sanitize_path(path)?;
        let resp = http_signed_async(&sk, "DELETE", &format!("{url}/files/{p}"), &[]).await?;
        match resp.status {
            200 => Ok(true),
            404 => Ok(false),
            s => Err(format!("agent /files DELETE status {s}: {}", resp.body)),
        }
    }

    pub async fn file_tree(&self, sandbox_id: Uuid) -> Result<Vec<TreeEntry>, String> {
        let (sk, url) = self.sandbox_keys(sandbox_id)?;
        let resp = http_signed_async(&sk, "GET", &format!("{url}/tree"), &[]).await?;
        if resp.status != 200 {
            return Err(format!("agent /tree status {}: {}", resp.status, resp.body));
        }
        let v: serde_json::Value = serde_json::from_str(&resp.body)
            .map_err(|e| format!("agent /tree response not JSON: {e}"))?;
        let entries = v["entries"]
            .as_array()
            .ok_or_else(|| "agent /tree: missing 'entries' array".to_string())?;
        Ok(entries
            .iter()
            .filter_map(|e| {
                let path = e["path"].as_str()?.to_string();
                // Agent tree entries currently report kind via a
                // boolean is_dir; map to the unified enum-shaped
                // string. Defaults to "file" when ambiguous.
                let kind = if e["is_dir"].as_bool().unwrap_or(false) {
                    "dir"
                } else {
                    "file"
                };
                let size = e["size"].as_u64().unwrap_or(0);
                Some(TreeEntry { path, kind, size })
            })
            .collect())
    }

    fn sandbox_keys(&self, id: Uuid) -> Result<(Arc<SigningKey>, String), String> {
        let guard = self.state.read().unwrap();
        let s = guard
            .get(&id)
            .ok_or_else(|| "sandbox not found in k8s backend".to_string())?;
        // Arc clone is a refcount bump — cheap. Cloning the SigningKey
        // by value would heap-copy the 32-byte secret on every signed
        // RPC, doubling the in-memory key count for the duration of
        // the request (ed25519-dalek::SigningKey doesn't zeroize on
        // drop).
        Ok((s.signing_key.clone(), s.agent_url.clone()))
    }

    /// Lift the per-sandbox auth material into a backend-agnostic
    /// envelope. See `super::SandboxAuth` for the contract.
    pub async fn session_auth(
        &self,
        sandbox_id: Uuid,
    ) -> Result<super::SandboxAuth, String> {
        let guard = self.state.read().unwrap();
        let s = guard
            .get(&sandbox_id)
            .ok_or_else(|| "sandbox not found in k8s backend".to_string())?;
        let pubkey_fp = sig::pubkey_fingerprint(&s.signing_key.verifying_key());
        Ok(super::SandboxAuth {
            signing_key: s.signing_key.clone(),
            agent_url: s.agent_url.clone(),
            pubkey_fp,
        })
    }

    /// Persist-on-mint helper for the k8s backend. See
    /// [`super::Backend::seal_with_preview_state`] for the contract.
    /// Returns `Ok(false)` when persistence is disabled or the sandbox
    /// is unknown to the backend.
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
        let (sk_bytes, agent_url) = {
            let guard = self.state.read().unwrap();
            let Some(s) = guard.get(&sandbox_id) else {
                return Ok(false);
            };
            (s.signing_key.to_bytes(), s.agent_url.clone())
        };
        let pubkey_fp = sig::pubkey_fingerprint(
            &SigningKey::from_bytes(&sk_bytes).verifying_key(),
        );
        let record = crate::persist::SealedAuth {
            version: crate::persist::SEAL_VERSION,
            sandbox_id: sandbox_id.to_string(),
            user_id: info.user_id.clone(),
            project_id: info.project_id.clone(),
            backend: "k8s".to_string(),
            signing_key_bytes: sk_bytes,
            vm_index: None,
            agent_url: Some(agent_url),
            pubkey_fp,
            created_at_secs: info.created_at_secs,
            preview_secrets: secrets,
            preview_audit: audit,
        };
        persist
            .seal(sandbox_id, &record)
            .await
            .map(|()| true)
            .map_err(|e| format!("seal failed: {e}"))
    }
}

// ─── create-time bookkeeping ────────────────────────────────────

/// RAII for the `creating_users` set. Drops the user_id from the
/// in-flight set on scope exit (success OR failure) so a future
/// call for the same user isn't blocked forever after a panic /
/// early-return.
struct ReleaseCreating {
    set: Arc<Mutex<HashSet<String>>>,
    user_id: String,
}

impl ReleaseCreating {
    fn new(set: Arc<Mutex<HashSet<String>>>, user_id: String) -> Self {
        Self { set, user_id }
    }
}

impl Drop for ReleaseCreating {
    fn drop(&mut self) {
        // Recover from poison: a panic while another thread held
        // the lock would otherwise lock this user out **forever**
        // (every `lock()` returns Err once poisoned; the user_id
        // stays in the set; every future `create` for that user
        // fails with "concurrent create in progress"). The set is
        // a HashSet of strings — no invariant can be broken — so
        // `into_inner()` recovery is always safe.
        let mut g = self.set.lock().unwrap_or_else(|p| p.into_inner());
        g.remove(&self.user_id);
    }
}

/// Tracks partially-applied state during `create` so we can clean
/// up on intermediate failure without leaking Pods, ConfigMaps, or
/// port-forward subprocesses. Activate on each step that succeeds;
/// `disarm()` only on full success. Cleanup on Drop is best-effort
/// and synchronous (Drop can't be async, and these are local-only
/// kubectl + Child::kill calls; the brief blocking is acceptable
/// in the error path which is not hot).
struct CreateGuard {
    ports: Arc<Mutex<PortAllocator>>,
    namespace: String,
    pod_name: String,
    configmap_name: String,
    pub configmap_created: bool,
    pub pod_created: bool,
    pub port_forward: Option<Child>,
    pub port_forward_local_port: Option<u16>,
    armed: bool,
}

impl CreateGuard {
    fn new(
        ports: Arc<Mutex<PortAllocator>>,
        namespace: String,
        pod_name: String,
        configmap_name: String,
    ) -> Self {
        Self {
            ports,
            namespace,
            pod_name,
            configmap_name,
            configmap_created: false,
            pod_created: false,
            port_forward: None,
            port_forward_local_port: None,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CreateGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Kill port-forward (sync — we're in Drop).
        if let Some(mut child) = self.port_forward.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(p) = self.port_forward_local_port.take() {
            if let Ok(mut alloc) = self.ports.lock() {
                alloc.release(p);
            }
        }
        // Pod + ConfigMap deletion via blocking kubectl. Not async,
        // but the create-failure path runs at most once per failed
        // request — acceptable to spend ~500ms sync here.
        if self.pod_created {
            let _ = std::process::Command::new("kubectl")
                .args([
                    "delete", "pod", &self.pod_name, "-n", &self.namespace,
                    "--ignore-not-found", "--grace-period=0", "--force",
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        if self.configmap_created {
            let _ = std::process::Command::new("kubectl")
                .args([
                    "delete", "configmap", &self.configmap_name,
                    "-n", &self.namespace, "--ignore-not-found",
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

// ─── HTTP signed call to the agent ───────────────────────────────

#[derive(Debug)]
struct AgentResponse {
    status: u16,
    body: String,
    bytes: Vec<u8>,
}

/// Async wrapper that signs + sends to the in-VM agent **without
/// blocking the ntex worker**. ureq is sync; the call is moved onto
/// a `compio::runtime::spawn_blocking` thread.
///
/// **Signing happens INSIDE the closure**, not on the calling
/// thread. This matters because the agent's verifier rejects
/// signatures whose timestamp is more than 5s skewed from now.
/// If we signed on the caller and the spawn_blocking pool was
/// saturated (every worker busy on a 60s ureq), the signed
/// request would sit in the queue past the 5s skew window and
/// 401 on arrival. Generating `ts`/`nonce`/`signature` after
/// the queue drains preserves freshness.
///
/// Previously the call sites used the sync `http_signed` directly —
/// every `ureq::call()` could block the ntex worker for up to 60s,
/// and a single slow-network sandbox stalled all unrelated handlers
/// on that worker. The stress test never caught it because port-
/// forward to localhost is fast.
async fn http_signed_async(
    signing_key: &Arc<SigningKey>,
    method: &str,
    url: &str,
    body: &[u8],
) -> Result<AgentResponse, String> {
    let path = url
        .splitn(4, '/')
        .nth(3)
        .map(|p| format!("/{p}"))
        .unwrap_or_else(|| "/".to_string());
    let path = path.split('?').next().unwrap_or("/").to_string();

    // Arc clone — refcount bump, NOT a 32-byte secret copy. Avoids
    // having two heap copies of the secret coexisting during every
    // signed RPC (ed25519-dalek::SigningKey doesn't zeroize on drop).
    let signing_key = Arc::clone(signing_key);
    let method = method.to_string();
    let url = url.to_string();
    let body = body.to_vec();
    compio::runtime::spawn_blocking(move || {
        // Generate ts/nonce/signature **here**, after the queue has
        // drained, so the signature is fresh against the agent's 5s
        // skew window.
        let ts = unix_now();
        let nonce = random_nonce()?;
        let signature = sig::sign(&signing_key, &method, &path, &body, ts, &nonce);
        compio_blocking_call(&method, &url, &body, ts, &nonce, &signature)
    })
    .await
    .map_err(|e| format!("blocking task panic: {e:?}"))?
}

/// Plain blocking ureq call. Caller is responsible for already
/// running on a blocking thread when concurrency matters.
fn compio_blocking_call(
    method: &str,
    url: &str,
    body: &[u8],
    ts: u64,
    nonce: &str,
    signature: &str,
) -> Result<AgentResponse, String> {
    let mut req = match method {
        "GET" => ureq::get(url),
        "POST" => ureq::post(url),
        "PUT" => ureq::put(url),
        "DELETE" => ureq::delete(url),
        m => return Err(format!("unsupported method {m}")),
    };
    req = req
        .timeout(Duration::from_secs(60))
        .set("x-sbx-timestamp", &ts.to_string())
        .set("x-sbx-nonce", nonce)
        .set("x-sbx-signature", signature);
    let send = if body.is_empty() { req.call() } else { req.send_bytes(body) };
    match send {
        Ok(resp) => {
            let status = resp.status();
            let mut bytes = Vec::new();
            let _ = resp.into_reader().take(64 * 1024 * 1024).read_to_end(&mut bytes);
            let body = String::from_utf8_lossy(&bytes).into_owned();
            Ok(AgentResponse { status, body, bytes })
        }
        Err(ureq::Error::Status(code, resp)) => {
            let mut bytes = Vec::new();
            let _ = resp.into_reader().take(8192).read_to_end(&mut bytes);
            let body = String::from_utf8_lossy(&bytes).into_owned();
            Ok(AgentResponse { status: code, body, bytes })
        }
        Err(e) => Err(format!("{method} {url}: {e}")),
    }
}

// ─── kubectl shell-outs ─────────────────────────────────────────

#[derive(Debug)]
struct CliOutput {
    status: i32,
    stdout: String,
    stderr: String,
}

async fn run_kubectl(args: &[&str]) -> Result<CliOutput, String> {
    let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
    compio::runtime::spawn_blocking(move || {
        let out = Command::new("kubectl")
            .args(&owned)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| format!("spawn kubectl: {e}"))?;
        Ok::<_, String>(CliOutput {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    })
    .await
    .map_err(|e| format!("blocking task panic: {e:?}"))?
}

async fn kubectl_apply_stdin(yaml: String) -> Result<(), String> {
    compio::runtime::spawn_blocking(move || {
        use std::io::Write;
        let mut child = Command::new("kubectl")
            .args(["apply", "-f", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn kubectl apply: {e}"))?;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(yaml.as_bytes())
            .map_err(|e| format!("write yaml: {e}"))?;
        let out = child
            .wait_with_output()
            .map_err(|e| format!("wait kubectl: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "kubectl apply failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok::<_, String>(())
    })
    .await
    .map_err(|e| format!("blocking task panic: {e:?}"))?
}

async fn apply_pubkey_configmap(name: &str, namespace: &str, pubkey_b64: &str) -> Result<(), String> {
    let yaml = format!(
        r#"apiVersion: v1
kind: ConfigMap
metadata:
  name: {name}
  namespace: {namespace}
data:
  controller-pubkey: {pubkey_b64}
"#
    );
    kubectl_apply_stdin(yaml).await
}

/// Ensure a per-user PVC exists. Idempotent (`kubectl apply`
/// handles the "already there" case as a no-op). The PVC is the
/// home directory of the dropped /exec child inside every sandbox
/// the user opens — package caches (pnpm, npm, pip, cargo),
/// dotfiles, and SSH config land there and survive across
/// sandboxes. The size + StorageClass are configurable; reclaim
/// policy is whatever the StorageClass defaults to (typically
/// `Delete`, which is fine — PVC deletion happens only on user
/// deletion, controlled by the control plane).
async fn ensure_user_home_pvc(
    name: &str,
    namespace: &str,
    user_id: &str,
    size: &str,
    storage_class: Option<&str>,
) -> Result<(), String> {
    // YAML emitted only when the PVC doesn't already exist; we
    // could `apply` unconditionally, but that re-validates / writes
    // the spec each time. Cheap-but-not-free; check first.
    let check = run_kubectl(&[
        "get", "pvc", name, "-n", namespace, "--ignore-not-found",
        "-o", "name",
    ])
    .await?;
    if check.status == 0 && !check.stdout.trim().is_empty() {
        return Ok(());
    }

    let storage_class_line = match storage_class {
        Some(c) if !c.is_empty() => format!("  storageClassName: {c}\n"),
        _ => String::new(),
    };
    let yaml = format!(
        r#"apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: {name}
  namespace: {namespace}
  labels:
    app.kubernetes.io/name: sandbox-agent
    zeroship.user: "{user_id}"
spec:
  accessModes:
    - ReadWriteOnce
{storage_class_line}  resources:
    requests:
      storage: {size}
"#
    );
    kubectl_apply_stdin(yaml).await
}

#[allow(clippy::too_many_arguments)]
async fn apply_agent_pod(
    name: &str,
    namespace: &str,
    image: &str,
    runtime_class: &str,
    memory_mb: u32,
    cpus: f32,
    configmap_name: &str,
    user_home_pvc: &str,
    user_id: &str,
    project_id: &str,
    sandbox_id: &str,
) -> Result<(), String> {
    let mem = format!("{memory_mb}Mi");
    let cpu_limit = format!("{cpus}");
    let yaml = format!(
        r#"apiVersion: v1
kind: Pod
metadata:
  name: {name}
  namespace: {namespace}
  labels:
    app.kubernetes.io/name: sandbox-agent
    zeroship.user: "{user_id}"
    zeroship.sandbox: "{sandbox_id}"
    zeroship.project: "{project_id}"
  annotations:
    run.oci.handler: krun
spec:
  runtimeClassName: {runtime_class}
  restartPolicy: Never
  automountServiceAccountToken: false
  enableServiceLinks: false
  containers:
    - name: agent
      image: {image}
      imagePullPolicy: IfNotPresent
      ports:
        - containerPort: 7777
          name: agent
      readinessProbe:
        httpGet:
          path: /readyz
          port: 7777
        initialDelaySeconds: 1
        periodSeconds: 1
        timeoutSeconds: 2
        failureThreshold: 30
      env:
        - name: SANDBOX_AGENT_LOG
          value: "info"
      volumeMounts:
        - name: trust
          mountPath: /run/keys
          readOnly: true
        - name: user-home
          mountPath: /home/u
      resources:
        limits:
          memory: {mem}
          cpu: "{cpu_limit}"
  volumes:
    - name: trust
      configMap:
        name: {configmap_name}
        items:
          - key: controller-pubkey
            path: controller-pubkey
            mode: 0444
    - name: user-home
      persistentVolumeClaim:
        claimName: {user_home_pvc}
"#
    );
    kubectl_apply_stdin(yaml).await
}

async fn wait_pod_ready(name: &str, namespace: &str, timeout_secs: u64) -> Result<(), String> {
    let timeout_arg = format!("--timeout={timeout_secs}s");
    let pod = format!("pod/{name}");
    let out = run_kubectl(&[
        "wait",
        "--for=condition=Ready",
        &pod,
        &timeout_arg,
        "-n",
        namespace,
    ])
    .await?;
    if out.status != 0 {
        return Err(format!(
            "kubectl wait Ready {name}: {} {}",
            out.stdout.trim(),
            out.stderr.trim()
        ));
    }
    Ok(())
}

async fn pod_ip(name: &str, namespace: &str) -> Result<String, String> {
    let out = run_kubectl(&[
        "get",
        "pod",
        name,
        "-n",
        namespace,
        "-o",
        "jsonpath={.status.podIP}",
    ])
    .await?;
    if out.status != 0 || out.stdout.trim().is_empty() {
        return Err(format!("kubectl get podIP {name}: {}", out.stderr.trim()));
    }
    Ok(out.stdout.trim().to_string())
}

async fn delete_pod(name: &str, namespace: &str) -> Result<(), String> {
    let out = run_kubectl(&[
        "delete",
        "pod",
        name,
        "-n",
        namespace,
        "--ignore-not-found",
        "--grace-period=0",
        "--force",
    ])
    .await?;
    if out.status != 0 {
        return Err(format!("kubectl delete pod {name}: {}", out.stderr.trim()));
    }
    Ok(())
}

async fn delete_configmap(name: &str, namespace: &str) -> Result<(), String> {
    let out = run_kubectl(&[
        "delete", "configmap", name, "-n", namespace, "--ignore-not-found",
    ])
    .await?;
    if out.status != 0 {
        return Err(format!("kubectl delete configmap {name}: {}", out.stderr.trim()));
    }
    Ok(())
}

/// Block until the kubelet has actually torn down the Pod
/// (terminated AND its volume mounts released). `kubectl delete
/// --force` returns when the API has accepted the deletion, NOT
/// when the kubelet has finished — on real CSI drivers (EBS,
/// Longhorn, Ceph) the volume detach can take 10-30 s. If the
/// next sandbox for the same user races a still-attaching PVC,
/// the new Pod sticks Pending with a Multi-Attach error and
/// `wait_pod_ready` times out — and we've already deleted the
/// old Pod, so the user loses both.
async fn wait_for_pod_gone(name: &str, namespace: &str, timeout: Duration) -> Result<(), String> {
    let timeout_arg = format!("--timeout={}s", timeout.as_secs().max(1));
    let pod = format!("pod/{name}");
    let out = run_kubectl(&[
        "wait",
        "--for=delete",
        &pod,
        &timeout_arg,
        "-n",
        namespace,
    ])
    .await?;
    // Pod-already-gone is success (the wait subcommand returns 0
    // with stderr "no matching resources found" or similar).
    if out.status != 0
        && !out.stderr.contains("no matching resources")
        && !out.stderr.contains("not found")
    {
        return Err(format!(
            "kubectl wait --for=delete pod/{name}: {}",
            out.stderr.trim()
        ));
    }
    Ok(())
}

fn start_port_forward(pod: &str, namespace: &str, local_port: u16) -> Result<Child, String> {
    let pod_arg = format!("pod/{pod}");
    let port_arg = format!("{local_port}:7777");
    Command::new("kubectl")
        .args(["port-forward", "-n", namespace, &pod_arg, &port_arg])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn port-forward: {e}"))
}

/// Poll the agent's `/livez` until 200 AND the agent's `/version`
/// reports the **expected pubkey fingerprint**, or the deadline
/// expires.
///
/// **Async** — uses `compio::time::sleep` between polls and
/// `compio::runtime::spawn_blocking` for each ureq call. The
/// previous version used `std::thread::sleep` and `ureq::call()`
/// directly from this `async fn` running on an ntex worker, which
/// blocked the worker for up to `timeout` seconds during every Pod
/// create. With multiple concurrent creates that's an O(N×timeout)
/// stall on the entire ntex pool.
///
/// **FM-A parity:** the K8s backend has the identical race shape as
/// nomad-ch — when a Pod is recycled, the cluster may serve a
/// /livez=200 from the *previous* tenant's still-alive Pod (Pod IP
/// reuse during teardown, kubelet lag, or a stale `kubectl
/// port-forward` connection that survived a pod restart). Verify
/// the agent answering /livez is also signing-and-attesting with
/// OUR pubkey (its `/version.pubkey_fingerprint` matches the fp the
/// controller minted at create-time). See the nomad-ch helper of
/// the same name for the full rationale and the backward-compat
/// fall-back for legacy agents.
async fn wait_for_agent_livez(
    base_url: &str,
    expected_fp: &str,
    signing_key: &Arc<SigningKey>,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let livez_url = format!("{base_url}/livez");
    let mut last_fp: Option<String> = None;
    let mut last_version_status: Option<u16> = None;
    while Instant::now() < deadline {
        let probe_url = livez_url.clone();
        let livez_status = compio::runtime::spawn_blocking(move || {
            ureq::get(&probe_url)
                .timeout(Duration::from_millis(500))
                .call()
                .map(|r| r.status())
                .ok()
        })
        .await
        .ok()
        .flatten();
        if livez_status == Some(200) {
            let version_url = format!("{base_url}/version");
            match http_signed_async(signing_key, "GET", &version_url, &[]).await {
                Ok(resp) => {
                    last_version_status = Some(resp.status);
                    if resp.status == 200 {
                        let fp_opt = serde_json::from_str::<serde_json::Value>(&resp.body)
                            .ok()
                            .and_then(|v| {
                                v.get("pubkey_fingerprint")
                                    .and_then(|s| s.as_str())
                                    .map(|s| s.to_string())
                            });
                        match fp_opt {
                            Some(fp) if fp == expected_fp => return Ok(()),
                            Some(fp) => {
                                last_fp = Some(fp);
                            }
                            None => {
                                // Legacy agent missing the field —
                                // /version was signed-auth-checked, so
                                // the agent IS verifying with our
                                // pubkey. Warn + accept.
                                eprintln!(
                                    "[sandbox/k8s] wait_for_agent: legacy agent at \
                                     {base_url} returned no pubkey_fingerprint on \
                                     /version; falling back to signed-auth-only \
                                     attestation"
                                );
                                return Ok(());
                            }
                        }
                    }
                }
                Err(_e) => {
                    // Transport error on /version — retry.
                }
            }
        }
        compio::time::sleep(Duration::from_millis(150)).await;
    }
    if let Some(actual_fp) = last_fp {
        Err(format!(
            "stale agent at {base_url}: expected pubkey_fingerprint={expected_fp}, \
             got {actual_fp}; previous tenant's Pod still answers on this IP/port"
        ))
    } else if last_version_status == Some(401) {
        Err(format!(
            "stale agent at {base_url}: /version returned 401 (agent is verifying with \
             a different controller pubkey); expected fp={expected_fp}"
        ))
    } else {
        Err(format!(
            "agent at {base_url} never returned 200 on /livez (expected fp={expected_fp})"
        ))
    }
}

// ─── small utilities ────────────────────────────────────────────

fn random_key32() -> Result<[u8; 32], String> {
    let mut buf = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .map_err(|e| format!("open /dev/urandom: {e}"))?
        .read_exact(&mut buf)
        .map_err(|e| format!("read /dev/urandom: {e}"))?;
    Ok(buf)
}

fn random_nonce() -> Result<String, String> {
    let bytes = random_key32()?;
    Ok(bytes.iter().take(16).map(|b| format!("{b:02x}")).collect())
}

/// Wall-clock seconds since UNIX_EPOCH.
///
/// Crash-loud on clock-before-epoch instead of silently returning 0:
/// the silent fallback would put signed-RPC timestamps 56 years in
/// the past (agent 401s forever), make the idle reaper cull every
/// sandbox immediately, and lie in the UI's "created_at" field.
/// Better to panic and let orchestration surface the broken host
/// clock. Mirrors `nomad_ch.rs::unix_now` and `sandbox-agent::sig::unix_now`.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX_EPOCH")
        .as_secs()
}

/// Sanitize a request-supplied relative path before forwarding to
/// the agent. The agent has its own (stricter) checks via openat2,
/// but pre-rejecting obvious nonsense here gives a faster + clearer
/// 4xx response and avoids consuming an HMAC nonce on a doomed call.
fn sanitize_path(p: &str) -> Result<String, String> {
    if p.is_empty() {
        return Err("path is empty".into());
    }
    if p.starts_with('/') {
        return Err("absolute paths not allowed".into());
    }
    for seg in p.split('/') {
        if seg == ".." {
            return Err("'..' segments not allowed".into());
        }
    }
    Ok(p.to_string())
}

/// Validate a user_id / project_id at the backend boundary.
///
/// The HTTP handler (`is_safe_id`) is the primary gate; this
/// repeats the rule **identically** as defense-in-depth for any
/// future caller that bypasses the handler (programmatic backend
/// use, integration tests, a mistakenly-added admin endpoint).
///
/// **Charset (mirrored, do not relax):** `[a-z0-9-]{1,50}` with
/// the first char in `[a-z0-9]`. Underscore is intentionally
/// excluded — previous versions accepted both `_` and uppercase
/// and rewrote them in `user_pvc_name`, which collapsed distinct
/// user_ids to the same PVC name and produced cross-user data
/// bleed. After this, `user_id` and PVC name are 1:1.
fn validate_id(id: &str, what: &'static str) -> Result<(), String> {
    if id.is_empty() || id.len() > 50 {
        return Err(format!(
            "{what} must be 1..=50 chars; got {} chars",
            id.len()
        ));
    }
    let mut chars = id.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return Err(format!(
            "{what} must start with [a-z0-9]; got {id:?}"
        ));
    }
    if !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
        return Err(format!(
            "{what} must match [a-z0-9-]+ after first char; got {id:?}"
        ));
    }
    Ok(())
}

/// Stable PVC name derived from `user_id`. Because `validate_id`
/// already restricts the input to `[a-z0-9-]`, this is now an
/// identity-prefix function — no rewriting, no collisions.
fn user_pvc_name(user_id: &str) -> String {
    debug_assert!(validate_id(user_id, "user_id").is_ok());
    format!("zsbx-userhome-{user_id}")
}

#[cfg(test)]
mod tests {
    use super::{sanitize_path, user_pvc_name, validate_id};

    #[test]
    fn sanitize_rejects_parent() {
        assert!(sanitize_path("../etc/passwd").is_err());
        assert!(sanitize_path("foo/../bar").is_err());
    }

    #[test]
    fn sanitize_rejects_absolute() {
        assert!(sanitize_path("/etc/passwd").is_err());
    }

    #[test]
    fn sanitize_rejects_empty() {
        assert!(sanitize_path("").is_err());
    }

    #[test]
    fn sanitize_accepts_relative() {
        assert_eq!(sanitize_path("src/main.rs").unwrap(), "src/main.rs");
        assert_eq!(sanitize_path("a.txt").unwrap(), "a.txt");
    }

    #[test]
    fn validate_id_accepts_lowercase_dns_1123_subset() {
        assert!(validate_id("alice", "user_id").is_ok());
        assert!(validate_id("alice-1", "user_id").is_ok());
        assert!(validate_id("u123", "user_id").is_ok());
        assert!(validate_id("0alice", "user_id").is_ok());
    }

    #[test]
    fn validate_id_rejects_bad_chars() {
        // Underscore is intentionally rejected — see history note
        // in `validate_id` doc.
        assert!(validate_id("alice_1", "user_id").is_err());
        // Uppercase rejected for the same reason.
        assert!(validate_id("Alice", "user_id").is_err());
        assert!(validate_id("ALICE", "user_id").is_err());
        // Standard "obviously bad" cases.
        assert!(validate_id("alice@example.com", "user_id").is_err());
        assert!(validate_id("alice/bob", "user_id").is_err());
        assert!(validate_id("alice bob", "user_id").is_err());
        assert!(validate_id("", "user_id").is_err());
        assert!(validate_id(&"a".repeat(51), "user_id").is_err());
        // Leading dash / digit rule: dash-leading rejected.
        assert!(validate_id("-alice", "user_id").is_err());
    }

    /// Regression: `validate_id` and `user_pvc_name` together must
    /// guarantee a 1:1 between user_id and PVC name. Distinct
    /// user_ids that pass validation must produce distinct PVC
    /// names. Previously `Alice` and `alice` (and `alice_1` /
    /// `alice-1`) collapsed to the same PVC — cross-user data bleed.
    #[test]
    fn user_pvc_name_is_one_to_one() {
        let cases = [
            ("alice", "zsbx-userhome-alice"),
            ("alice-1", "zsbx-userhome-alice-1"),
            ("u123", "zsbx-userhome-u123"),
            ("0a", "zsbx-userhome-0a"),
        ];
        for (input, expected) in cases {
            assert!(validate_id(input, "user_id").is_ok());
            let got = user_pvc_name(input);
            assert_eq!(got, expected, "user_pvc_name({input:?})");
            assert!(got.len() <= 253, "{got} too long");
            assert!(
                got.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "{got} not DNS-1123: contains non-alphanum-or-dash"
            );
        }
        // No two distinct valid inputs map to the same PVC name.
        let inputs = ["alice", "alice-1", "alice-2", "u1", "u2"];
        let names: std::collections::HashSet<String> =
            inputs.iter().map(|s| user_pvc_name(s)).collect();
        assert_eq!(names.len(), inputs.len(), "PVC name collision: {names:?}");
    }
}
