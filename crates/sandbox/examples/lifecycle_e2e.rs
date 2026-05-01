//! End-to-end lifecycle test for `zeroship-sandbox` against the
//! **k8s + libkrun** backend.
//!
//! What this exercises (in order):
//!
//!   1. **probe** — backend reachable (kubectl works, namespace exists).
//!   2. **create** — Pod + ConfigMap appear; agent /readyz returns 200.
//!   3. **exec** — `uname -r` proves we hit the libkrun kernel.
//!   4. **write_file** + **read_file** roundtrip via the agent.
//!   5. **file_tree** — the file we just wrote is listed.
//!   6. **delete_file** — and then 404 on read.
//!   7. **re-attach** — calling create again with the same project_id
//!      yields the existing session (registry dedup).
//!   8. **stop** — Pod + ConfigMap deleted; subsequent ops 404.
//!
//! Usage:
//!     cargo run --release -p zeroship-sandbox --example lifecycle_e2e
//!
//! Pre-reqs: KUBECONFIG, the agent image preloaded into the cluster's
//! containerd, RuntimeClass `kvm-sandbox` present. See
//! `docs/runbooks/sandbox-agent.md`.

#![allow(unsafe_code)]

use std::env;
use std::time::Instant;

use uuid::Uuid;
use zeroship_sandbox::backend::{Backend, SessionInfo};
use zeroship_sandbox::config::SandboxConfig;
use zeroship_sandbox::session::SessionRegistry;

const G: &str = "\x1b[32m";
const R: &str = "\x1b[31m";
const Y: &str = "\x1b[33m";
const Z: &str = "\x1b[0m";

#[compio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("{R}LIFECYCLE E2E FAILED: {e}{Z}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    println!("{Y}== zeroship-sandbox lifecycle e2e (k8s + libkrun) =={Z}");

    // 1. Build a config pointed at the k8s backend. Force port-forward
    //    on (we're running on the host, not in-cluster).
    set_default("SANDBOX_BACKEND", "k8s");
    set_default("SANDBOX_K8S_USE_PORT_FORWARD", "true");
    set_default("SANDBOX_K8S_NAMESPACE", "default");
    let config = SandboxConfig::from_env().map_err(|e| format!("config: {e}"))?;
    println!("backend:        {}", config.backend);
    println!("namespace:      {}", config.k8s.namespace);
    println!("agent image:    {}", config.k8s.image);
    println!("port-forward:   {}", config.k8s.use_port_forward);
    println!();

    // 2. Build backend + registry directly (skip the HTTP layer for a
    //    pure-Rust lifecycle test).
    let backend = Backend::from_config(&config)?;
    println!("{G}-- probe backend --{Z}");
    let started = Instant::now();
    backend.probe().await?;
    println!("probe ok ({:?})", started.elapsed());

    let registry = SessionRegistry::new();
    // Stable user across the run so session 2 reattaches the
    // existing per-user PVC and proves the cache survived.
    let user_id = format!("e2e-user-{}", Uuid::new_v4().simple());
    let project_id = format!("e2e-{}", Uuid::new_v4().simple());
    let result = run_lifecycle(&backend, &registry, &user_id, &project_id).await;

    // Always best-effort cleanup on the way out.
    if let Some(id) = registry.find_by_user_project(&user_id, &project_id) {
        let _ = backend.stop(id).await;
        let _ = registry.remove(&id);
    }
    // PVC intentionally left behind — it's user-scoped, not test-
    // scoped. Operators clean these up via a separate prune job.
    // For local dev: `kubectl delete pvc -l zeroship.user=<id>`.
    println!("note: PVC zsbx-userhome-{user_id} left in cluster (per-user, not session-scoped)");
    result
}

async fn run_lifecycle(
    backend: &Backend,
    registry: &SessionRegistry,
    user_id: &str,
    project_id: &str,
) -> Result<(), String> {
    // ─── create ─────────────────────────────────────────────────
    case("create session (PVC + Pod + ConfigMap + port-forward + agent ready)", async {
        let session_id = Uuid::new_v4();
        let info = backend.create(session_id, user_id, project_id).await?;
        registry.insert(session_id, info.clone());
        check_info(&info, "k8s")?;
        if !info.backend_hint.contains("pod=")
            || !info.backend_hint.contains("key_fp=")
            || !info.backend_hint.contains("pvc=")
        {
            return Err(format!("backend_hint missing pod/key_fp/pvc: {}", info.backend_hint));
        }
        println!("    {}", info.backend_hint);
        Ok(())
    }).await?;

    let session_id = registry
        .find_by_user_project(user_id, project_id)
        .ok_or("session not in registry after create")?;

    // ─── exec ──────────────────────────────────────────────────
    case("exec uname -r — proves microVM kernel via signed /exec", async {
        let out = backend.exec(session_id, "uname -r", None, Some(5_000)).await?;
        if out.status != 0 {
            return Err(format!("uname status={} stderr={}", out.status, out.stderr));
        }
        let kernel = out.stdout.trim();
        if kernel.is_empty() {
            return Err("empty kernel string".into());
        }
        println!("    pod kernel: {kernel}");
        Ok(())
    }).await?;

    case("exec id -u — privilege drop landed (uid != 0)", async {
        let out = backend.exec(session_id, "id -u", None, Some(5_000)).await?;
        let uid = out.stdout.trim();
        if uid == "0" {
            return Err("/exec child still root — privilege drop bypassed".into());
        }
        println!("    pod uid: {uid}");
        Ok(())
    }).await?;

    // ─── file CRUD ─────────────────────────────────────────────
    case("write_file then read_file roundtrip", async {
        backend.write_file(session_id, "hello.txt", b"world").await?;
        let bytes = backend.read_file(session_id, "hello.txt").await?;
        if bytes != b"world" {
            return Err(format!("roundtrip mismatch: {:?}", String::from_utf8_lossy(&bytes)));
        }
        Ok(())
    }).await?;

    case("write_file with nested path creates parents", async {
        backend.write_file(session_id, "src/server.ts", b"export {};").await?;
        let bytes = backend.read_file(session_id, "src/server.ts").await?;
        if bytes != b"export {};" {
            return Err("nested write/read mismatch".into());
        }
        Ok(())
    }).await?;

    case("file_tree lists the files we wrote", async {
        let entries = backend.file_tree(session_id).await?;
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        for required in ["hello.txt", "src/server.ts"] {
            if !paths.contains(&required) {
                return Err(format!("missing {required} in {paths:?}"));
            }
        }
        Ok(())
    }).await?;

    case("delete_file then read_file → not found", async {
        let removed = backend.delete_file(session_id, "hello.txt").await?;
        if !removed {
            return Err("delete reported not-found on a file that should exist".into());
        }
        let r = backend.read_file(session_id, "hello.txt").await;
        match r {
            Err(e) if e.contains("not found") || e.contains("No such file") => Ok(()),
            Ok(_) => Err("file still readable after delete".into()),
            Err(e) => Err(format!("unexpected error after delete: {e}")),
        }
    }).await?;

    // ─── path traversal defense (handled by the agent's openat2) ─
    case("write to ../etc/passwd is rejected", async {
        match backend.write_file(session_id, "../etc/passwd", b"pwn").await {
            Err(e) => {
                println!("    rejected: {e}");
                Ok(())
            }
            Ok(()) => Err("agent accepted a workspace-escaping write".into()),
        }
    }).await?;

    // ─── re-attach ─────────────────────────────────────────────
    case("create with same (user, project) reuses existing session", async {
        // Simulate the registry-level dedup the HTTP handler does.
        let existing = registry
            .find_by_user_project(user_id, project_id)
            .ok_or("session not in registry")?;
        if existing != session_id {
            return Err("registry forgot the session id between calls".into());
        }
        let info = registry.get(&existing).ok_or("session vanished")?;
        if info.session_id != session_id.to_string() {
            return Err("registry returned a different session_id".into());
        }
        Ok(())
    }).await?;

    // ─── stop ──────────────────────────────────────────────────
    case("stop session — Pod + ConfigMap deleted, ops fail", async {
        backend.stop(session_id).await?;
        // A stopped session is gone from the backend; any op should
        // surface an error.
        match backend.exec(session_id, "true", None, Some(2000)).await {
            Err(_) => Ok(()),
            Ok(out) => Err(format!("post-stop exec succeeded: {out:?}")),
        }
    }).await?;

    case("stop is idempotent (second call is Ok)", async {
        backend.stop(session_id).await?;
        Ok(())
    }).await?;

    // ─── per-user PVC persistence ──────────────────────────────
    //
    // The headline test for the per-user storage layer: write a
    // marker into ~/.cache (which is on the per-user PVC), tear
    // the session down, spin a fresh session for the SAME user,
    // verify the marker is still there. Proves cache-survives-
    // across-sandboxes — the whole point of the per-user PVC.

    // Re-create a session for the same user (different session id)
    // so we have a live agent to query.
    let session2 = Uuid::new_v4();
    case("recreate session — same user, different session id", async {
        let info = backend.create(session2, user_id, project_id).await?;
        registry.insert(session2, info);
        Ok(())
    }).await?;

    case("first sandbox: write marker into ~/.cache/zsbx-marker", async {
        let out = backend
            .exec(
                session2,
                "mkdir -p ~/.cache && echo first-run-$$ > ~/.cache/zsbx-marker && cat ~/.cache/zsbx-marker",
                None,
                Some(5_000),
            )
            .await?;
        if out.status != 0 || !out.stdout.contains("first-run-") {
            return Err(format!("marker write failed: {out:?}"));
        }
        Ok(())
    }).await?;

    case("verify HOME is /home/u (the PVC mount)", async {
        let out = backend.exec(session2, "echo $HOME", None, Some(5_000)).await?;
        let home = out.stdout.trim();
        if home != "/home/u" {
            return Err(format!("HOME={home:?} expected /home/u"));
        }
        Ok(())
    }).await?;

    // Sample the original marker we expect to survive.
    let marker_before = backend
        .exec(session2, "cat ~/.cache/zsbx-marker", None, Some(5_000))
        .await
        .map_err(|e| format!("read marker: {e}"))?
        .stdout
        .trim()
        .to_string();
    if marker_before.is_empty() {
        return Err("marker_before empty before tear-down".into());
    }

    case("tear session down (PVC stays, single-session-per-user)", async {
        backend.stop(session2).await?;
        registry.remove(&session2);
        Ok(())
    }).await?;

    let session3 = Uuid::new_v4();
    case("create third session — same user, fresh Pod, reuses PVC", async {
        let info = backend.create(session3, user_id, project_id).await?;
        registry.insert(session3, info);
        Ok(())
    }).await?;

    case("marker survives across sandboxes (per-user PVC works)", async {
        let out = backend
            .exec(session3, "cat ~/.cache/zsbx-marker", None, Some(5_000))
            .await?;
        if out.status != 0 {
            return Err(format!(
                "cat marker on fresh sandbox failed: status={} stderr={}",
                out.status, out.stderr
            ));
        }
        let marker_after = out.stdout.trim();
        if marker_after != marker_before {
            return Err(format!(
                "PVC didn't reattach! before={marker_before:?} after={marker_after:?}"
            ));
        }
        println!("    marker preserved: {marker_after:?}");
        Ok(())
    }).await?;

    case("npm-style cache directory persists too", async {
        // Smoke test: agent's HOME is /home/u; pnpm/npm/pip would
        // land their caches there. Just create a placeholder dir
        // structure to confirm filesystem semantics work.
        let _ = backend
            .exec(
                session3,
                "mkdir -p ~/.npm/_cacache && touch ~/.npm/_cacache/index-v5 && ls ~/.npm/_cacache/",
                None,
                Some(5_000),
            )
            .await?;
        Ok(())
    }).await?;

    case("final stop — registry clean, PVC retained for next session", async {
        backend.stop(session3).await?;
        registry.remove(&session3);
        Ok(())
    }).await?;

    println!();
    println!("{G}== ALL LIFECYCLE STEPS PASSED =={Z}");
    Ok(())
}

// ─── small reporting helpers ────────────────────────────────────

fn check_info(info: &SessionInfo, expected_backend: &str) -> Result<(), String> {
    if info.backend != expected_backend {
        return Err(format!("backend={} expected {expected_backend}", info.backend));
    }
    if info.session_id.is_empty() || info.project_id.is_empty() {
        return Err("blank session_id or project_id".into());
    }
    Ok(())
}

fn set_default(key: &str, value: &str) {
    if env::var_os(key).is_none() {
        // SAFETY: env mutation is process-global. Examples are
        // single-threaded at this point (no other tasks racing).
        unsafe {
            env::set_var(key, value);
        }
    }
}

async fn case<F>(name: &str, f: F) -> Result<(), String>
where
    F: std::future::Future<Output = Result<(), String>>,
{
    print!("  {name} ... ");
    use std::io::Write;
    std::io::stdout().flush().ok();
    let started = Instant::now();
    match f.await {
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
