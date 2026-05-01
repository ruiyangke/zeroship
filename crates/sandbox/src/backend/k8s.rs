//! Kubernetes + libkrun backend.
//!
//! Drives `zeroship-sandbox-agent` Pods running under the
//! `kvm-sandbox` RuntimeClass (crun + libkrun microVM). Each session
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
//!     host): a per-session `kubectl port-forward` subprocess on a
//!     loopback port.
//!
//! Lifecycle ops then talk to the agent over HTTP, signed with the
//! session's signing key. File operations go via `/files/*`,
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
//! session spawns a `kubectl port-forward pod/<pod> <local>:7777`
//! background process and routes traffic through it. A simple
//! atomic counter picks unique loopback ports per session. The
//! port-forward is killed on `stop`.

use std::collections::HashMap;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use ed25519_dalek::SigningKey;
use uuid::Uuid;
use zeroship_sandbox_agent::sig;

use super::{ExecOutput, SessionInfo, TreeEntry};
use crate::config::SandboxConfig;

#[derive(Debug)]
pub struct K8sBackend {
    cfg: SandboxConfig,
    /// Per-session bookkeeping. Populated on `create`, cleared on
    /// `stop`. Mutex-not-RwLock because writes (create/stop) and
    /// reads (every op) are at similar frequency and the contention
    /// is bounded.
    state: Arc<RwLock<HashMap<Uuid, K8sSession>>>,
    /// Loopback port allocator for `kubectl port-forward`. We hand
    /// out unique ports starting from `cfg.k8s.port_forward_start`
    /// and never reuse — per-session port-forward subprocesses are
    /// short-lived enough that running out is improbable, and the
    /// agent verifier rejects cross-session replay anyway.
    next_port: AtomicU16,
}

struct K8sSession {
    pod_name: String,
    configmap_name: String,
    namespace: String,
    /// Base URL the controller uses to reach the agent. Either
    /// `http://<pod-ip>:7777` (in-cluster) or
    /// `http://127.0.0.1:<local>` (port-forward).
    agent_url: String,
    /// Per-session signing key. Lives only in this process; never
    /// touches the cluster.
    signing_key: SigningKey,
    /// Background `kubectl port-forward` subprocess if enabled, kept
    /// alive for the session lifetime. Killed on stop.
    port_forward: Option<Mutex<Child>>,
}

impl std::fmt::Debug for K8sSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("K8sSession")
            .field("pod_name", &self.pod_name)
            .field("configmap_name", &self.configmap_name)
            .field("namespace", &self.namespace)
            .field("agent_url", &self.agent_url)
            .field("port_forward", &self.port_forward.is_some())
            .finish_non_exhaustive()
    }
}

impl K8sBackend {
    pub fn new(cfg: SandboxConfig) -> Result<Self, String> {
        let next_port = AtomicU16::new(cfg.k8s.port_forward_start);
        Ok(Self {
            cfg,
            state: Arc::new(RwLock::new(HashMap::new())),
            next_port,
        })
    }

    pub async fn probe(&self) -> Result<(), String> {
        // Make sure kubectl exists and the cluster is reachable.
        let out = run_kubectl(&["version", "--output=json"]).await?;
        if out.status != 0 {
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
            return Err(format!(
                "kubectl get namespace {ns}: {}",
                out.stderr.trim()
            ));
        }
        Ok(())
    }

    pub async fn create(
        &self,
        session_id: Uuid,
        project_id: &str,
    ) -> Result<SessionInfo, String> {
        // 1. Mint Ed25519 keypair. Only the public key leaves this process.
        let sk_bytes = random_key32()?;
        let signing_key = SigningKey::from_bytes(&sk_bytes);
        let pubkey = signing_key.verifying_key();
        let pubkey_b64 = B64.encode(pubkey.as_bytes());
        let key_fp = sig::pubkey_fingerprint(&pubkey);

        let pod_name = format!("zsbx-{}", session_id.simple());
        let configmap_name = format!("{pod_name}-trust");
        let ns = self.cfg.k8s.namespace.clone();

        // 2. Apply ConfigMap (public key) — NOT a Secret.
        apply_pubkey_configmap(&configmap_name, &ns, &pubkey_b64).await?;

        // 3. Apply Pod.
        apply_agent_pod(
            &pod_name,
            &ns,
            &self.cfg.k8s.image,
            &self.cfg.k8s.runtime_class,
            self.cfg.memory_mb,
            self.cfg.cpus,
            &configmap_name,
            project_id,
            &session_id.to_string(),
        )
        .await?;

        // 4. Wait for the Pod to be Ready. Wrap in a Result so we
        //    clean up on failure (don't leak a half-spawned Pod).
        let ready_res = wait_pod_ready(
            &pod_name,
            &ns,
            self.cfg.k8s.ready_timeout_secs,
        )
        .await;
        if let Err(e) = ready_res {
            // Best-effort cleanup; ignore further errors.
            let _ = delete_pod(&pod_name, &ns).await;
            let _ = delete_configmap(&configmap_name, &ns).await;
            return Err(e);
        }

        // 5. Resolve agent URL.
        let (agent_url, port_forward) = if self.cfg.k8s.use_port_forward {
            let local_port = self.next_port.fetch_add(1, Ordering::Relaxed);
            let pf = start_port_forward(&pod_name, &ns, local_port)?;
            let url = format!("http://127.0.0.1:{local_port}");
            wait_for_agent_livez(&url, Duration::from_secs(20))?;
            (url, Some(Mutex::new(pf)))
        } else {
            let pod_ip = pod_ip(&pod_name, &ns).await?;
            let url = format!("http://{pod_ip}:7777");
            wait_for_agent_livez(&url, Duration::from_secs(20))?;
            (url, None)
        };

        // 6. Stash internal state.
        let session = K8sSession {
            pod_name: pod_name.clone(),
            configmap_name: configmap_name.clone(),
            namespace: ns,
            agent_url: agent_url.clone(),
            signing_key,
            port_forward,
        };
        self.state.write().unwrap().insert(session_id, session);

        let now = unix_now();
        Ok(SessionInfo {
            session_id: session_id.to_string(),
            project_id: project_id.to_string(),
            backend: "k8s".to_string(),
            backend_hint: format!("pod={pod_name} key_fp={key_fp} url={agent_url}"),
            created_at_secs: now,
            last_used_at_secs: now,
        })
    }

    pub async fn stop(&self, session_id: Uuid) -> Result<(), String> {
        let session = match self.state.write().unwrap().remove(&session_id) {
            Some(s) => s,
            None => return Ok(()), // idempotent
        };
        // Kill the port-forward first so we don't keep a process
        // talking to a Pod that's about to vanish.
        if let Some(pf) = session.port_forward {
            if let Ok(mut child) = pf.lock() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        // Try a graceful shutdown via the agent first (flips drain),
        // then delete the Pod + ConfigMap.
        let _ = http_signed(
            &session.signing_key,
            "POST",
            &format!("{}/shutdown", session.agent_url),
            &[],
        );
        let _ = delete_pod(&session.pod_name, &session.namespace).await;
        let _ = delete_configmap(&session.configmap_name, &session.namespace).await;
        Ok(())
    }

    pub async fn exec(
        &self,
        session_id: Uuid,
        cmd: &str,
        cwd: Option<&str>,
        timeout_ms: Option<u64>,
    ) -> Result<ExecOutput, String> {
        let (sk, url) = self.session_keys(session_id)?;
        let body = serde_json::json!({
            "cmd": cmd,
            "cwd": cwd,
            "timeout_ms": timeout_ms,
        })
        .to_string();
        let resp = http_signed(&sk, "POST", &format!("{url}/exec"), body.as_bytes())?;
        if resp.status != 200 {
            return Err(format!("agent /exec status {}: {}", resp.status, resp.body));
        }
        let v: serde_json::Value = serde_json::from_str(&resp.body)
            .map_err(|e| format!("agent /exec response not JSON: {e}"))?;
        Ok(ExecOutput {
            status: v["status"].as_i64().unwrap_or(-1) as i32,
            stdout: v["stdout"].as_str().unwrap_or("").to_string(),
            stderr: v["stderr"].as_str().unwrap_or("").to_string(),
            timed_out: v["timed_out"].as_bool().unwrap_or(false),
        })
    }

    pub async fn read_file(&self, session_id: Uuid, path: &str) -> Result<Vec<u8>, String> {
        let (sk, url) = self.session_keys(session_id)?;
        let p = sanitize_path(path)?;
        let resp = http_signed(&sk, "GET", &format!("{url}/files/{p}"), &[])?;
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
        session_id: Uuid,
        path: &str,
        body: &[u8],
    ) -> Result<(), String> {
        let (sk, url) = self.session_keys(session_id)?;
        let p = sanitize_path(path)?;
        let resp = http_signed(&sk, "PUT", &format!("{url}/files/{p}"), body)?;
        if resp.status != 200 {
            return Err(format!("agent /files PUT status {}: {}", resp.status, resp.body));
        }
        Ok(())
    }

    pub async fn delete_file(&self, session_id: Uuid, path: &str) -> Result<bool, String> {
        let (sk, url) = self.session_keys(session_id)?;
        let p = sanitize_path(path)?;
        let resp = http_signed(&sk, "DELETE", &format!("{url}/files/{p}"), &[])?;
        match resp.status {
            200 => Ok(true),
            404 => Ok(false),
            s => Err(format!("agent /files DELETE status {s}: {}", resp.body)),
        }
    }

    pub async fn file_tree(&self, session_id: Uuid) -> Result<Vec<TreeEntry>, String> {
        let (sk, url) = self.session_keys(session_id)?;
        let resp = http_signed(&sk, "GET", &format!("{url}/tree"), &[])?;
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

    fn session_keys(&self, id: Uuid) -> Result<(SigningKey, String), String> {
        let guard = self.state.read().unwrap();
        let s = guard
            .get(&id)
            .ok_or_else(|| "session not found in k8s backend".to_string())?;
        Ok((s.signing_key.clone(), s.agent_url.clone()))
    }
}

// ─── HTTP signed call to the agent ───────────────────────────────

#[derive(Debug)]
struct AgentResponse {
    status: u16,
    body: String,
    bytes: Vec<u8>,
}

/// Sign + send a single HTTP request to the in-VM agent. Runs the
/// blocking `ureq` call as-is on this thread; callers that want to
/// avoid blocking the ntex worker should already be inside a
/// `compio::runtime::spawn_blocking`. Most of our backend ops do
/// kubectl shell-outs anyway, so adding another layer of blocking
/// is fine.
fn http_signed(
    signing_key: &SigningKey,
    method: &str,
    url: &str,
    body: &[u8],
) -> Result<AgentResponse, String> {
    // Parse the path out of the URL for canonical-string
    // construction. We only support `http://host:port/path` shapes.
    let path = url
        .splitn(4, '/')
        .nth(3)
        .map(|p| format!("/{p}"))
        .unwrap_or_else(|| "/".to_string());
    // Strip query string — the agent rejects signed requests that
    // carry them. We never set them, but defend against future drift.
    let path = path.split('?').next().unwrap_or("/").to_string();

    let ts = unix_now();
    let nonce = random_nonce()?;
    let signature = sig::sign(signing_key, method, &path, body, ts, &nonce);

    let result = compio_blocking_call(method, url, body, ts, &nonce, &signature);
    result
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

#[allow(clippy::too_many_arguments)]
async fn apply_agent_pod(
    name: &str,
    namespace: &str,
    image: &str,
    runtime_class: &str,
    memory_mb: u32,
    cpus: f32,
    configmap_name: &str,
    project_id: &str,
    session_id: &str,
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
    zeroship.session: "{session_id}"
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

fn wait_for_agent_livez(base_url: &str, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let url = format!("{base_url}/livez");
    while Instant::now() < deadline {
        if let Ok(resp) = ureq::get(&url).timeout(Duration::from_millis(500)).call() {
            if resp.status() == 200 {
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    Err(format!("agent at {base_url} never returned 200 on /livez"))
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

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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

#[cfg(test)]
mod tests {
    use super::sanitize_path;

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
}
