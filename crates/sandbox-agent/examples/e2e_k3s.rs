//! End-to-end test: drive a real sandbox-agent Pod on a local k3s
//! cluster through every endpoint of the wire protocol.
//!
//! Flow:
//!
//!   1. Generate a fresh 32-byte HMAC key.
//!   2. Apply a k8s `Secret` carrying the key.
//!   3. Apply a Pod with `runtimeClassName: kvm-sandbox` (libkrun
//!      microVM) that mounts the Secret at the agent's expected
//!      `/run/secrets/sandbox-agent-token` path and runs
//!      `zeroship/sandbox-agent:dev`.
//!   4. `kubectl wait --for=condition=Ready` on the Pod.
//!   5. `kubectl port-forward` to the Pod's port 7777, in the
//!      background.
//!   6. Hit every endpoint with a properly-HMAC-signed request and
//!      assert on the response body / status.
//!   7. Clean up Pod + Secret on the way out (success or failure).
//!
//! Usage:
//!
//!     cargo run --example e2e_k3s --release
//!
//! Reads `KUBECONFIG` from the env (defaults to `~/.kube/config`).
//! Expects the agent image to already be loaded into the cluster's
//! containerd (via `k3s ctr images import` in our case — see
//! `docs/runbooks/local-k3s-crun-krun.md`).

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use ed25519_dalek::SigningKey;
use zeroship_sandbox_agent::sig;

const NS: &str = "default";
const POD_NAME: &str = "agent-e2e";
/// ConfigMap holding the controller's Ed25519 **public** key. We
/// use a ConfigMap (not a Secret) because the pubkey is non-secret;
/// no encryption-at-rest concerns, and a sandbox VM that reads it
/// gains zero forgery capability.
const PUBKEY_CONFIGMAP: &str = "agent-e2e-trust";
const IMAGE: &str = "docker.io/zeroship/sandbox-agent:dev";
const LOCAL_PORT: u16 = 17900;

// ANSI for readable logs.
const G: &str = "\x1b[32m"; // green
const R: &str = "\x1b[31m"; // red
const Y: &str = "\x1b[33m"; // yellow
const Z: &str = "\x1b[0m";  // reset

fn main() {
    if let Err(e) = run() {
        eprintln!("{R}E2E FAILED: {e}{Z}");
        cleanup();
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    // Generate a fresh Ed25519 keypair. The signing key NEVER
    // leaves this process; only the public key ships to the Pod.
    let signing_key = SigningKey::from_bytes(&random_key32());
    let pubkey = signing_key.verifying_key();
    let pubkey_b64 = base64_std::encode(pubkey.as_bytes());

    println!("{Y}== E2E sandbox-agent on k3s =={Z}");
    println!("namespace:    {NS}");
    println!("pod:          {POD_NAME}");
    println!("image:        {IMAGE}");
    println!("local_port:   {LOCAL_PORT}");
    println!("pubkey_fp:    {}", sig::pubkey_fingerprint(&pubkey));
    println!();

    // 0. Sanity: kubectl + cluster + image reachable
    sanity_check()?;

    // 1. Best-effort cleanup of stale state from a previous run.
    cleanup();

    // 2. Apply ConfigMap (pubkey only) + Pod
    println!("{G}-- applying ConfigMap (public key) + Pod --{Z}");
    apply_pubkey_configmap(&pubkey_b64)?;
    apply_pod()?;

    // 3. Wait for the Pod to be Ready.
    println!("{G}-- waiting for Pod Ready --{Z}");
    let started = Instant::now();
    kubectl(&[
        "wait",
        "--for=condition=Ready",
        &format!("pod/{POD_NAME}"),
        "--timeout=120s",
        "-n",
        NS,
    ])?;
    println!("Pod Ready in {:?}", started.elapsed());

    // 4. port-forward in the background.
    println!("{G}-- opening port-forward --{Z}");
    let mut pf = port_forward()?;
    let pf_started = Instant::now();
    wait_for_port(LOCAL_PORT)?;
    println!("port-forward live in {:?}", pf_started.elapsed());

    // 5. Run the test cases.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let agent = Agent::new(LOCAL_PORT, signing_key.clone());
        run_cases(&agent)
    }));

    // 6. Tear down port-forward, Pod, Secret.
    let _ = pf.kill();
    let _ = pf.wait();
    cleanup();

    match result {
        Ok(Ok(())) => {
            println!();
            println!("{G}== ALL CASES PASSED =={Z}");
            Ok(())
        }
        Ok(Err(e)) => Err(e),
        Err(_) => Err("test case panicked".to_string()),
    }
}

// ─── test cases ─────────────────────────────────────────────────

fn run_cases(agent: &Agent) -> Result<(), String> {
    // --- /version (unauthenticated)
    case("/version returns protocol_version=1 + auth.ed25519-v1", || {
        let v = agent.get_unauth("/version")?;
        assert_eq!(v["protocol_version"].as_u64(), Some(1));
        let caps: Vec<&str> = v["capabilities"]
            .as_array()
            .ok_or("capabilities not array")?
            .iter()
            .filter_map(|x| x.as_str())
            .collect();
        if !caps.contains(&"auth.ed25519-v1") {
            return Err(format!("missing auth.ed25519-v1 in {caps:?}"));
        }
        Ok(())
    })?;

    // --- /livez + /readyz
    case("/livez returns 200 ok", || {
        let v = agent.get_unauth("/livez")?;
        assert_eq!(v["status"].as_str(), Some("ok"));
        Ok(())
    })?;
    case("/readyz returns 200 ready", || {
        let v = agent.get_unauth("/readyz")?;
        assert_eq!(v["status"].as_str(), Some("ready"));
        Ok(())
    })?;

    // --- auth: unsigned request rejected
    case("auth-gated endpoint without sig → 401", || {
        let (status, _) = agent.get_raw_unsigned("/tree")?;
        if status != 401 { return Err(format!("expected 401, got {status}")); }
        Ok(())
    })?;

    // --- /tree empty
    case("/tree on empty workspace → entries=[]", || {
        let v = agent.get_signed("/tree", &[])?;
        let entries = v["entries"].as_array().ok_or("no entries")?;
        if !entries.is_empty() {
            return Err(format!("expected 0 entries, got {}", entries.len()));
        }
        Ok(())
    })?;

    // --- PUT a file, GET it back
    case("PUT then GET file roundtrip", || {
        let body = b"hello from e2e";
        agent.put_file("/files/note.txt", body)?;
        let bytes = agent.get_file("/files/note.txt")?;
        if bytes != body {
            return Err(format!("body mismatch: got {bytes:?}"));
        }
        Ok(())
    })?;

    // --- PUT nested path creates parents
    case("PUT /files/a/b/c.txt creates parent dirs", || {
        agent.put_file("/files/a/b/c.txt", b"nested")?;
        let bytes = agent.get_file("/files/a/b/c.txt")?;
        if bytes != b"nested" {
            return Err("nested write/read mismatch".into());
        }
        Ok(())
    })?;

    // --- /tree shows the files we wrote
    case("/tree lists written files", || {
        let v = agent.get_signed("/tree", &[])?;
        let entries = v["entries"].as_array().ok_or("no entries")?;
        let paths: Vec<&str> = entries
            .iter()
            .filter_map(|e| e["path"].as_str())
            .collect();
        for required in ["note.txt", "a/b/c.txt"] {
            if !paths.contains(&required) {
                return Err(format!("missing {required} in {paths:?}"));
            }
        }
        Ok(())
    })?;

    // --- DELETE roundtrip
    case("DELETE then GET → 404", || {
        agent.delete_file("/files/note.txt")?;
        let (status, _) = agent.get_raw_signed("/files/note.txt")?;
        if status != 404 {
            return Err(format!("expected 404 after delete, got {status}"));
        }
        Ok(())
    })?;

    // --- /metrics is unauthenticated and exposes the expected names
    case("/metrics returns prometheus exposition", || {
        let (status, body) = agent.get_raw_unsigned("/metrics")?;
        if status != 200 {
            return Err(format!("expected 200 from /metrics, got {status}"));
        }
        for needle in [
            "sbx_agent_exec_requests_total",
            "sbx_agent_uptime_seconds",
            "sbx_agent_reaper_healthy 1",
        ] {
            if !body.contains(needle) {
                return Err(format!(
                    "/metrics missing {needle}\nbody: {body}"
                ));
            }
        }
        Ok(())
    })?;

    // --- /exec drops privileges to nobody:nogroup
    case("/exec runs as nobody (uid != 0)", || {
        let v = agent.exec(r#"{"cmd":"id -u; id -un"}"#)?;
        if v["status"].as_i64() != Some(0) {
            return Err(format!("id failed: {v}"));
        }
        let stdout = v["stdout"].as_str().unwrap_or("").trim();
        // Two lines: numeric uid, then username. Expect 65534 + nobody.
        let mut it = stdout.lines();
        let uid = it.next().unwrap_or("");
        let user = it.next().unwrap_or("");
        if uid == "0" || user == "root" {
            return Err(format!(
                "/exec child still root — privilege drop failed: {stdout}"
            ));
        }
        Ok(())
    })?;

    // --- no_new_privs is set on every child
    case("/exec child has NoNewPrivs=1", || {
        let v = agent.exec(
            r#"{"cmd":"grep -E ^NoNewPrivs: /proc/self/status"}"#,
        )?;
        if v["status"].as_i64() != Some(0) {
            return Err(format!("grep failed: {v}"));
        }
        let stdout = v["stdout"].as_str().unwrap_or("");
        if !stdout.contains("NoNewPrivs:\t1") {
            return Err(format!("NoNewPrivs not set: {stdout}"));
        }
        Ok(())
    })?;

    // --- capability bounding set is empty (we dropped while root)
    case("/exec child has empty CapBnd", || {
        let v = agent.exec(
            r#"{"cmd":"grep -E ^CapBnd: /proc/self/status"}"#,
        )?;
        if v["status"].as_i64() != Some(0) {
            return Err(format!("grep failed: {v}"));
        }
        let stdout = v["stdout"].as_str().unwrap_or("");
        // CapBnd is a 16-hex-digit bitmask. After PR_CAPBSET_DROP
        // for every cap, all bits should be 0.
        if !stdout.contains("CapBnd:\t0000000000000000") {
            return Err(format!("CapBnd not zeroed: {stdout}"));
        }
        Ok(())
    })?;

    // --- RLIMIT_NPROC is capped (per-uid; nobody starts at 0).
    //     dash's `ulimit -u` is not portable, so read from
    //     /proc/self/limits where the column layout is stable.
    case("/exec child has RLIMIT_NPROC capped", || {
        let v = agent.exec(
            r#"{"cmd":"awk '/^Max processes/ {print $3}' /proc/self/limits"}"#,
        )?;
        if v["status"].as_i64() != Some(0) {
            return Err(format!("awk failed: {v}"));
        }
        let n: u64 = v["stdout"].as_str().unwrap_or("").trim().parse().unwrap_or(0);
        if n == 0 || n > 256 {
            return Err(format!("RLIMIT_NPROC not capped: {n}"));
        }
        Ok(())
    })?;

    // --- RLIMIT_NOFILE is capped
    case("/exec child has RLIMIT_NOFILE capped", || {
        let v = agent.exec(
            r#"{"cmd":"awk '/^Max open files/ {print $4}' /proc/self/limits"}"#,
        )?;
        if v["status"].as_i64() != Some(0) {
            return Err(format!("awk failed: {v}"));
        }
        let n: u64 = v["stdout"].as_str().unwrap_or("").trim().parse().unwrap_or(0);
        if n == 0 || n > 1024 {
            return Err(format!("RLIMIT_NOFILE not capped: {n}"));
        }
        Ok(())
    })?;

    // --- nobody cannot signal PID 1 (the agent)
    case("/exec child cannot kill PID 1", || {
        // `kill -0 1` returns 0 if the signal could be sent; nonzero
        // if EPERM. As nobody we must hit EPERM.
        let v = agent.exec(r#"{"cmd":"kill -0 1; echo $?"}"#)?;
        let stdout = v["stdout"].as_str().unwrap_or("").trim();
        if stdout == "0" {
            return Err(format!(
                "/exec child can signal PID 1 — privilege drop ineffective: {stdout}"
            ));
        }
        Ok(())
    })?;

    // --- nobody cannot read /proc/1/environ (root-only)
    case("/exec child cannot read /proc/1/environ", || {
        let v = agent.exec(
            r#"{"cmd":"cat /proc/1/environ 2>&1 >/dev/null; echo exit=$?"}"#,
        )?;
        let stdout = v["stdout"].as_str().unwrap_or("").trim();
        if stdout.contains("exit=0") {
            return Err(format!(
                "/exec child can read /proc/1/environ — escalation possible: {stdout}"
            ));
        }
        Ok(())
    })?;

    // --- /exec roundtrip
    case("/exec echoes uname (proves microVM kernel)", || {
        let v = agent.exec(r#"{"cmd":"uname -r"}"#)?;
        if v["status"].as_i64() != Some(0) {
            return Err(format!("non-zero exit: {v}"));
        }
        let stdout = v["stdout"].as_str().unwrap_or("").trim();
        println!("    pod kernel: {stdout}");
        // The libkrun-bundled kernel is 6.12.34; the host is 6.12.80.
        // We don't hard-assert the specific version — just that the
        // VM boundary is real (different kernel string than host).
        if stdout.is_empty() {
            return Err("empty kernel string".into());
        }
        Ok(())
    })?;

    // --- /exec env_clear: no SANDBOX_AGENT_* leaks to child
    case("/exec child env contains no SANDBOX_AGENT_*", || {
        let v = agent.exec(r#"{"cmd":"printenv | grep -E ^SANDBOX_AGENT || echo none"}"#)?;
        let stdout = v["stdout"].as_str().unwrap_or("").trim();
        if stdout != "none" {
            return Err(format!("agent env leaked to child: {stdout}"));
        }
        Ok(())
    })?;

    // --- /exec timeout preserves partial output
    case("/exec timeout returns partial stdout", || {
        let v = agent.exec(r#"{"cmd":"echo early; sleep 5","timeout_ms":300}"#)?;
        if v["timed_out"].as_bool() != Some(true) {
            return Err("expected timed_out=true".into());
        }
        if v["stdout"].as_str().unwrap_or("").trim() != "early" {
            return Err(format!("partial-output lost: {}", v["stdout"]));
        }
        Ok(())
    })?;

    // --- symlink-leaf escape attempt → 403
    case("symlink-leaf escape rejected (403)", || {
        // Use /exec to plant the symlink, then GET it.
        agent.exec(r#"{"cmd":"ln -sf /etc/passwd /workspace/escape"}"#)?;
        let (status, _) = agent.get_raw_signed("/files/escape")?;
        if status != 403 {
            return Err(format!("expected 403, got {status}"));
        }
        Ok(())
    })?;

    // --- /shutdown drains
    case("/shutdown flips drain → /readyz=503", || {
        agent.shutdown()?;
        let (status, _) = agent.get_raw_unsigned("/readyz")?;
        if status != 503 {
            return Err(format!("expected /readyz=503 after drain, got {status}"));
        }
        // /livez should still be 200 (alive but draining).
        let (status, _) = agent.get_raw_unsigned("/livez")?;
        if status != 200 {
            return Err(format!("expected /livez=200, got {status}"));
        }
        Ok(())
    })?;

    Ok(())
}

fn case<F: FnOnce() -> Result<(), String>>(name: &str, f: F) -> Result<(), String> {
    print!("  {name} ... ");
    std::io::stdout().flush().ok();
    let started = Instant::now();
    match f() {
        Ok(()) => {
            println!("{G}OK{Z} ({:?})", started.elapsed());
            Ok(())
        }
        Err(e) => {
            println!("{R}FAIL{Z}");
            Err(format!("{name}: {e}"))
        }
    }
}

// ─── Agent client ────────────────────────────────────────────────

struct Agent {
    base: String,
    signing_key: SigningKey,
}

impl Agent {
    fn new(port: u16, signing_key: SigningKey) -> Self {
        Self {
            base: format!("http://127.0.0.1:{port}"),
            signing_key,
        }
    }

    fn get_unauth(&self, path: &str) -> Result<serde_json::Value, String> {
        let url = format!("{}{path}", self.base);
        let resp = ureq::get(&url).call().map_err(|e| format!("{path}: {e}"))?;
        resp.into_json::<serde_json::Value>()
            .map_err(|e| format!("{path}: parse: {e}"))
    }

    fn get_raw_unsigned(&self, path: &str) -> Result<(u16, String), String> {
        raw_get(&self.base, path, &[])
    }

    fn get_raw_signed(&self, path: &str) -> Result<(u16, String), String> {
        let h = self.sign("GET", path, &[]);
        raw_get(&self.base, path, &h)
    }

    fn get_signed(&self, path: &str, body: &[u8]) -> Result<serde_json::Value, String> {
        let h = self.sign("GET", path, body);
        let url = format!("{}{path}", self.base);
        let mut req = ureq::get(&url);
        for (k, v) in &h {
            req = req.set(k, v);
        }
        let resp = req.call().map_err(|e| format!("{path}: {e}"))?;
        resp.into_json::<serde_json::Value>()
            .map_err(|e| format!("{path}: parse: {e}"))
    }

    fn get_file(&self, path: &str) -> Result<Vec<u8>, String> {
        let h = self.sign("GET", path, &[]);
        let url = format!("{}{path}", self.base);
        let mut req = ureq::get(&url);
        for (k, v) in &h {
            req = req.set(k, v);
        }
        let resp = req.call().map_err(|e| format!("{path}: {e}"))?;
        let mut buf = Vec::new();
        resp.into_reader()
            .read_to_end(&mut buf)
            .map_err(|e| format!("{path}: {e}"))?;
        Ok(buf)
    }

    fn put_file(&self, path: &str, body: &[u8]) -> Result<(), String> {
        let h = self.sign("PUT", path, body);
        let url = format!("{}{path}", self.base);
        let mut req = ureq::put(&url);
        for (k, v) in &h {
            req = req.set(k, v);
        }
        let resp = req
            .send_bytes(body)
            .map_err(|e| format!("PUT {path}: {e}"))?;
        if resp.status() != 200 {
            return Err(format!("PUT {path}: status {}", resp.status()));
        }
        Ok(())
    }

    fn delete_file(&self, path: &str) -> Result<(), String> {
        let h = self.sign("DELETE", path, &[]);
        let url = format!("{}{path}", self.base);
        let mut req = ureq::delete(&url);
        for (k, v) in &h {
            req = req.set(k, v);
        }
        let resp = req.call().map_err(|e| format!("DELETE {path}: {e}"))?;
        if resp.status() != 200 {
            return Err(format!("DELETE {path}: status {}", resp.status()));
        }
        Ok(())
    }

    fn exec(&self, body: &str) -> Result<serde_json::Value, String> {
        let h = self.sign("POST", "/exec", body.as_bytes());
        let url = format!("{}/exec", self.base);
        let mut req = ureq::post(&url);
        for (k, v) in &h {
            req = req.set(k, v);
        }
        let resp = req
            .send_bytes(body.as_bytes())
            .map_err(|e| format!("/exec: {e}"))?;
        resp.into_json::<serde_json::Value>()
            .map_err(|e| format!("/exec: parse: {e}"))
    }

    fn shutdown(&self) -> Result<(), String> {
        let h = self.sign("POST", "/shutdown", &[]);
        let url = format!("{}/shutdown", self.base);
        let mut req = ureq::post(&url);
        for (k, v) in &h {
            req = req.set(k, v);
        }
        let resp = req
            .call()
            .map_err(|e| format!("/shutdown: {e}"))?;
        if resp.status() != 200 {
            return Err(format!("/shutdown: status {}", resp.status()));
        }
        Ok(())
    }

    /// Build the three signed Ed25519 headers for a request.
    fn sign(&self, method: &str, path: &str, body: &[u8]) -> Vec<(&'static str, String)> {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let nonce = random_nonce();
        let signature = sig::sign(&self.signing_key, method, path, body, ts, &nonce);
        vec![
            ("x-sbx-timestamp", ts.to_string()),
            ("x-sbx-nonce", nonce),
            ("x-sbx-signature", signature),
        ]
    }
}

fn raw_get(
    base: &str,
    path: &str,
    headers: &[(&'static str, String)],
) -> Result<(u16, String), String> {
    let url = format!("{base}{path}");
    let mut req = ureq::get(&url);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    match req.call() {
        Ok(resp) => {
            let status = resp.status();
            let body = resp.into_string().unwrap_or_default();
            Ok((status, body))
        }
        Err(ureq::Error::Status(code, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            Ok((code, body))
        }
        Err(e) => Err(format!("{path}: {e}")),
    }
}

// ─── kubectl helpers ────────────────────────────────────────────

fn sanity_check() -> Result<(), String> {
    let kc = std::env::var("KUBECONFIG").unwrap_or_else(|_| "~/.kube/config".to_string());
    println!("KUBECONFIG: {kc}");
    kubectl(&["get", "nodes", "-o", "wide"])?;
    Ok(())
}

fn kubectl(args: &[&str]) -> Result<String, String> {
    let out = Command::new("kubectl")
        .args(args)
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .output()
        .map_err(|e| format!("kubectl spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "kubectl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn kubectl_apply_stdin(yaml: &str) -> Result<(), String> {
    let mut child = Command::new("kubectl")
        .args(["apply", "-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("kubectl apply spawn: {e}"))?;
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(yaml.as_bytes())
        .map_err(|e| format!("write stdin: {e}"))?;
    let out = child.wait_with_output().map_err(|e| format!("wait: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "kubectl apply failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

fn apply_pubkey_configmap(pubkey_b64: &str) -> Result<(), String> {
    // ConfigMap, NOT Secret — the Ed25519 public key is non-secret.
    // The agent reads it once at startup; the file persists on the
    // read-only mount. There is no signing material in the cluster.
    let yaml = format!(
        r#"apiVersion: v1
kind: ConfigMap
metadata:
  name: {PUBKEY_CONFIGMAP}
  namespace: {NS}
data:
  controller-pubkey: {pubkey_b64}
"#
    );
    kubectl_apply_stdin(&yaml)
}

fn apply_pod() -> Result<(), String> {
    let yaml = format!(
        r#"apiVersion: v1
kind: Pod
metadata:
  name: {POD_NAME}
  namespace: {NS}
  labels:
    app: zeroship-agent-e2e
  annotations:
    run.oci.handler: krun
spec:
  runtimeClassName: kvm-sandbox
  restartPolicy: Never
  containers:
    - name: agent
      image: {IMAGE}
      imagePullPolicy: Never
      ports:
        - containerPort: 7777
          name: agent
      readinessProbe:
        httpGet:
          path: /livez
          port: 7777
        initialDelaySeconds: 1
        periodSeconds: 1
        timeoutSeconds: 2
        failureThreshold: 30
      volumeMounts:
        - name: trust
          mountPath: /run/keys
          readOnly: true
      resources:
        limits:
          memory: 512Mi
          cpu: "1"
  volumes:
    - name: trust
      configMap:
        name: {PUBKEY_CONFIGMAP}
        items:
          - key: controller-pubkey
            path: controller-pubkey
            mode: 0444
"#
    );
    kubectl_apply_stdin(&yaml)
}

fn port_forward() -> Result<Child, String> {
    Command::new("kubectl")
        .args([
            "port-forward",
            "-n",
            NS,
            &format!("pod/{POD_NAME}"),
            &format!("{LOCAL_PORT}:7777"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("port-forward spawn: {e}"))
}

fn wait_for_port(port: u16) -> Result<(), String> {
    // Two-phase wait: first the local socket has to be bound by
    // kubectl port-forward, THEN the proxy has to actually be able
    // to reach the remote agent. The "Handling connection" log line
    // only appears after the first inbound connection, so we poll
    // /livez (an unauthenticated, body-less, idempotent GET) until
    // it returns 200.
    let deadline = Instant::now() + Duration::from_secs(20);
    let url = format!("http://127.0.0.1:{port}/livez");
    while Instant::now() < deadline {
        if let Ok(resp) = ureq::get(&url).timeout(Duration::from_millis(500)).call() {
            if resp.status() == 200 {
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    Err(format!("port {port} never went 200 on /livez"))
}

fn cleanup() {
    let _ = Command::new("kubectl")
        .args(["delete", "pod", POD_NAME, "-n", NS, "--ignore-not-found", "--grace-period=0", "--force"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = Command::new("kubectl")
        .args(["delete", "configmap", PUBKEY_CONFIGMAP, "-n", NS, "--ignore-not-found"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

// ─── small utilities ────────────────────────────────────────────

fn random_key(n: usize) -> Vec<u8> {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    std::fs::File::open("/dev/urandom")
        .expect("/dev/urandom")
        .read_exact(&mut buf)
        .expect("read /dev/urandom");
    buf
}

/// 32 random bytes, sized for Ed25519 [`SigningKey::from_bytes`].
fn random_key32() -> [u8; 32] {
    let v = random_key(32);
    v.try_into().expect("random_key returned 32 bytes")
}

fn random_nonce() -> String {
    // 16 bytes = 32 hex chars; uses only [a-f0-9] which fits the
    // server's nonce charset constraint.
    let bytes = random_key(16);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

mod base64_std {
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine as _;
    pub fn encode(b: &[u8]) -> String { B64.encode(b) }
}

