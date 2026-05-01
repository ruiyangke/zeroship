//! Nomad + Cloud Hypervisor backend.
//!
//! Submits a `raw_exec` Nomad job per sandbox; the job invokes a
//! wrapper script (shipped at `crates/sandbox/scripts/nomad-vm-wrapper.sh`,
//! installed on the host out-of-band) that spawns:
//!
//!   - 3 × `virtiofsd` processes (`keys`, `workspace`, `userhome` shares)
//!   - 1 × `cloud-hypervisor` foreground process
//!
//! The CH VM boots a tiny Linux kernel (CONFIG_IP_PNP=y) + raw ext4
//! rootfs containing `/sbin/init` (a shell script that mounts the
//! virtio-fs shares + execs `zeroship-sandbox-agent`). The agent's
//! wire-protocol-v1 auth (Ed25519 signed requests, 5 s skew + 30 s
//! nonce LRU) is **identical** to the K8s backend; only the runtime
//! plumbing differs.
//!
//! ## Why this exists
//!
//! Kubernetes is heavyweight to operate on bare metal — kubelet,
//! coredns, CNI, RuntimeClass + crun + libkrun, PVC + StorageClass.
//! For single-node / small-cluster operators who already run Nomad,
//! `raw_exec + cloud-hypervisor` is the bare minimum: no cluster
//! networking abstraction, no CSI, just a shell wrapper that ties
//! one VM's lifetime to one Nomad alloc.
//!
//! ## Per-sandbox host layout
//!
//! ```text
//! /var/zeroship/ch/                                 # host_state_dir
//!   <sandbox-id>/
//!     keys/controller-pubkey                        # virtiofs tag=keys → /run/keys
//!     workspace/                                    # virtiofs tag=workspace → /workspace
//!   users/<user_id>/home/                           # virtiofs tag=userhome → /home/u
//!                                                   # (persists across sandboxes)
//! ```
//!
//! ## Network
//!
//! Each sandbox gets a `vm_index` from a free-list allocator
//! (`VmIndexAllocator`). Index → `/30` subnet:
//!
//! ```text
//!   tap device  : zsbx-nm-<idx>     (host operator pre-creates)
//!   host IP     : 10.99.<100+idx>.1
//!   VM IP       : 10.99.<100+idx>.2
//!   MAC         : 12:34:56:78:9b:<idx hex>
//! ```
//!
//! The controller computes only the **VM IP** (to reach the agent at
//! `http://10.99.<100+idx>.2:7777`); the wrapper script computes
//! everything else from `ZSBX_VM_INDEX`.
//!
//! ## Agent reachability
//!
//! The controller (running on the same host as the Nomad agent in
//! the demo, eventually on its own node with routing into the tap
//! subnets) talks **directly** to `10.99.<100+idx>.2:7777`. Unlike
//! the K8s backend's `kubectl port-forward` dev mode, we do not
//! tunnel — the assumption is the controller has L3 connectivity to
//! the tap subnets. A future single-binary `zeroship-sandbox-router`
//! sidecar would proxy this when the controller is off-host.
//!
//! ## Cleanup contract
//!
//! - `create` is wrapped in a `CreateGuard` (RAII Drop) that tears
//!   down the partial state — Nomad job, host_dir, vm_index — on any
//!   failure mid-flight.
//! - `stop` is idempotent (returns Ok if the sandbox isn't in the
//!   in-memory map). The Nomad job is purged, vm_index returned to
//!   the pool, and the per-sandbox host_dir is `rm -rf`'d. The
//!   per-user home dir is **never** deleted by `stop` — it's user-
//!   scoped state.
//!
//! ## Note on rootfs init.sh
//!
//! The demo rootfs ships an `init.sh` that mounts only the `keys`
//! and `workspace` shares. The 3-share design (`+userhome`) requires
//! a re-baked rootfs whose init.sh also runs:
//!
//! ```sh
//! mkdir -p /home/u
//! mount -t virtiofs userhome /home/u
//! ```
//!
//! Until that lands, `vm_index` is allocated and the share is
//! advertised, but the in-VM mount is a no-op — package caches
//! reside on the per-alloc rootfs and don't persist.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use ed25519_dalek::SigningKey;
use uuid::Uuid;
use zeroship_sandbox_agent::sig;
use zeroship_sandbox_agent::AGENT_PORT;

use super::{ExecOutput, SandboxInfo, TreeEntry};
use crate::config::SandboxConfig;

#[derive(Debug)]
pub struct NomadCHBackend {
    cfg: SandboxConfig,
    state: Arc<RwLock<HashMap<Uuid, NomadChSandbox>>>,
    /// Per-VM-index pool, free-list backed.
    vm_indices: Arc<Mutex<VmIndexAllocator>>,
    /// Per-user serialization gate (one in-flight `create` per user).
    creating_users: Arc<Mutex<HashSet<String>>>,
    healthy: Arc<AtomicBool>,
}

/// Per-sandbox bookkeeping. Lives only in process memory; on
/// controller restart the in-memory map is rebuilt from scratch and
/// any orphan jobs are (optionally) cleaned up at startup — see
/// [`NomadCHBackend::cleanup_orphans_at_startup`].
struct NomadChSandbox {
    user_id: String,
    /// Nomad job ID — `zsbx-<sandbox-id-simple>`.
    job_id: String,
    /// Index allocated from `VmIndexAllocator`. Released on `stop`.
    vm_index: u16,
    /// Root host directory for this sandbox's virtio-fs shares.
    /// `<host_state_dir>/<sandbox-id>/` — `keys/` and `workspace/`
    /// subdirs.
    host_dir: PathBuf,
    /// Base URL the controller uses to reach the agent. Derived from
    /// `vm_index`: `http://10.99.<100+idx>.2:7777`.
    agent_url: String,
    /// Per-sandbox signing key. Generated at create-time, lives only
    /// in this process. The corresponding **public** key is the only
    /// thing that ships into the VM (mounted via virtio-fs `keys`
    /// share at `/run/keys/controller-pubkey`).
    ///
    /// **Wrapped in Arc** so signed-RPC dispatch can clone a refcount
    /// (cheap) instead of the 32-byte secret bytes (which would mean
    /// two heap copies of the secret coexisting during every signed
    /// request, since `ed25519_dalek::SigningKey` doesn't zeroize on
    /// drop).
    signing_key: Arc<SigningKey>,
}

impl std::fmt::Debug for NomadChSandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NomadChSandbox")
            .field("user_id", &self.user_id)
            .field("job_id", &self.job_id)
            .field("vm_index", &self.vm_index)
            .field("host_dir", &self.host_dir)
            .field("agent_url", &self.agent_url)
            .finish_non_exhaustive()
    }
}

// ─── VmIndexAllocator ────────────────────────────────────────────

/// Free-list-backed allocator for the per-sandbox VM index. Hands
/// out the smallest free index ≥ `floor`; reclaims released indices
/// so we don't run off the end of the (host-pre-provisioned) tap
/// pool. Bounded above by `ceil` (inclusive). Behaviour and rationale
/// mirror `PortAllocator` in `k8s.rs`.
#[derive(Debug)]
pub(crate) struct VmIndexAllocator {
    floor: u16,
    ceil: u16,
    /// Highest index we've ever handed out (well, `next` is "the
    /// next index to try if `freed` is empty"). New allocs prefer
    /// `freed`, fall back to `next`, fail when `next > ceil`.
    next: u16,
    /// Returned indices, sorted ascending — smallest is reused first
    /// so we keep allocation density high near `floor`.
    freed: BTreeSet<u16>,
}

impl VmIndexAllocator {
    pub(crate) fn new(floor: u16, ceil: u16) -> Self {
        Self {
            floor,
            ceil,
            next: floor,
            freed: BTreeSet::new(),
        }
    }

    pub(crate) fn alloc(&mut self) -> Result<u16, String> {
        if let Some(&i) = self.freed.iter().next() {
            self.freed.remove(&i);
            return Ok(i);
        }
        if self.next > self.ceil {
            return Err(format!(
                "vm-index allocator exhausted (floor={}, ceil={})",
                self.floor, self.ceil
            ));
        }
        let i = self.next;
        self.next = self.next.saturating_add(1);
        Ok(i)
    }

    pub(crate) fn release(&mut self, i: u16) {
        if i >= self.floor && i <= self.ceil {
            self.freed.insert(i);
        }
    }
}

impl NomadCHBackend {
    pub fn new(cfg: SandboxConfig) -> Result<Self, String> {
        let alloc = VmIndexAllocator::new(
            cfg.nomad_ch.vm_index_floor,
            cfg.nomad_ch.vm_index_ceil,
        );
        Ok(Self {
            cfg,
            state: Arc::new(RwLock::new(HashMap::new())),
            vm_indices: Arc::new(Mutex::new(alloc)),
            creating_users: Arc::new(Mutex::new(HashSet::new())),
            healthy: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    pub async fn probe(&self) -> Result<(), String> {
        // `/v1/status/leader` is the cheapest reachability probe —
        // returns the leader's `host:port` as a JSON-quoted string,
        // or 500 if no quorum. We don't parse the body; status==200
        // is enough to flip the `healthy` bit.
        let url = format!("{}/v1/status/leader", self.cfg.nomad_ch.nomad_addr);
        let resp = http_get_unsigned(&url, Duration::from_secs(5)).await;
        match resp {
            Ok(r) if r.status == 200 => {
                self.healthy.store(true, Ordering::Relaxed);
                Ok(())
            }
            Ok(r) => {
                self.healthy.store(false, Ordering::Relaxed);
                Err(format!(
                    "nomad /v1/status/leader → status {}: {}",
                    r.status,
                    r.body.trim()
                ))
            }
            Err(e) => {
                self.healthy.store(false, Ordering::Relaxed);
                Err(format!("nomad probe failed: {e}"))
            }
        }
    }

    /// Best-effort cleanup of leftover `zsbx-` jobs from a previous
    /// controller process. Same gating + HA caveat as the K8s
    /// equivalent: defaults off; opt-in via
    /// `SANDBOX_NOMAD_CH_STARTUP_ORPHAN_CLEANUP=true` for single-
    /// replica operators.
    pub async fn cleanup_orphans_at_startup(&self) -> Result<usize, String> {
        if !self.cfg.nomad_ch.startup_orphan_cleanup {
            return Ok(0);
        }
        let base = self.cfg.nomad_ch.nomad_addr.clone();
        let url = format!("{base}/v1/jobs?prefix=zsbx-");
        let resp = http_get_unsigned(&url, Duration::from_secs(10)).await?;
        if resp.status != 200 {
            return Err(format!(
                "nomad list jobs prefix=zsbx- → status {}: {}",
                resp.status,
                resp.body.trim()
            ));
        }
        let jobs: serde_json::Value = serde_json::from_str(&resp.body)
            .map_err(|e| format!("nomad list jobs not JSON: {e}"))?;
        let mut deleted = 0usize;
        for j in jobs.as_array().into_iter().flatten() {
            let id = match j["ID"].as_str() {
                Some(s) if s.starts_with("zsbx-") => s.to_string(),
                _ => continue,
            };
            let stop_url = format!("{base}/v1/job/{id}?purge=true");
            if let Err(e) = http_delete_unsigned(&stop_url, Duration::from_secs(10)).await {
                eprintln!("[sandbox/nomad-ch] purge {id} failed: {e}");
                continue;
            }
            deleted += 1;
        }
        if deleted > 0 {
            eprintln!(
                "[sandbox/nomad-ch] cleanup_orphans_at_startup: purged {deleted} orphan job(s)"
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

        // Per-user serialization gate. Two concurrent creates for
        // the same user racing the "one active sandbox per user"
        // check would each see "no existing", both would submit
        // jobs, and both would race the per-user home dir. Fail
        // fast on collision so the caller can retry; mirrors
        // K8sBackend.
        {
            let mut creating = self
                .creating_users
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if !creating.insert(user_id.to_string()) {
                return Err(format!(
                    "concurrent sandbox create in progress for user {user_id:?}; retry"
                ));
            }
        }
        let release_creating =
            ReleaseCreating::new(self.creating_users.clone(), user_id.to_string());

        // One active sandbox per user — stop any existing sandbox
        // for this user before creating the new one. (Today the
        // per-user home dir is on a shared host fs; concurrent
        // mounts would be safe but the next milestone moves it to
        // RWO Ceph RBD — same-user double-attach would error there.
        // Keeping the same one-per-user invariant now means no
        // contract change later.)
        // HashMap value is plain owned data; poison can't break
        // invariants — recover.
        //
        // MUST collect into Vec; do not iterate while holding the read
        // lock — stop() takes write, and a future refactor that drops
        // the .collect() and iterates lazily would deadlock the
        // first time a user has > 0 active sandboxes.
        let existing: Vec<Uuid> = self
            .state
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|(_, s)| s.user_id == user_id)
            .map(|(id, _)| *id)
            .collect();
        for old_id in existing {
            eprintln!(
                "[sandbox/nomad-ch] user {user_id} already has sandbox {old_id}; stopping first"
            );
            if let Err(e) = self.stop(old_id).await {
                eprintln!("[sandbox/nomad-ch] stop({old_id}) failed: {e}");
            }
        }

        let job_id = format!("zsbx-{}", sandbox_id.simple());
        let host_dir = self
            .cfg
            .nomad_ch
            .host_state_dir
            .join(sandbox_id.simple().to_string());
        let user_home_dir = self.cfg.nomad_ch.user_home_dir_root.join(user_id).join("home");

        let mut guard = CreateGuard::new(
            self.vm_indices.clone(),
            self.cfg.nomad_ch.nomad_addr.clone(),
            job_id.clone(),
            host_dir.clone(),
        );

        let result = self
            .try_create(
                sandbox_id,
                user_id,
                project_id,
                &job_id,
                &host_dir,
                &user_home_dir,
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
                Err(e)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn try_create(
        &self,
        sandbox_id: Uuid,
        user_id: &str,
        project_id: &str,
        job_id: &str,
        host_dir: &Path,
        user_home_dir: &Path,
        guard: &mut CreateGuard,
    ) -> Result<SandboxInfo, String> {
        // 1. Mint Ed25519 keypair. Public half is the only thing
        //    that leaves this process; the private half stays in
        //    `signing_key` for the lifetime of the sandbox.
        let sk_bytes = random_key32()?;
        let signing_key = SigningKey::from_bytes(&sk_bytes);
        let pubkey = signing_key.verifying_key();
        let pubkey_b64 = B64.encode(pubkey.as_bytes());
        let key_fp = sig::pubkey_fingerprint(&pubkey);
        eprintln!(
            "[sandbox/nomad-ch] create sandbox={sandbox_id} user={user_id} \
             project={project_id} key_fp={key_fp}"
        );

        // 2. Allocate VM index from the pool. Track in the guard so
        //    cleanup-on-failure releases it.
        let vm_index = self
            .vm_indices
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .alloc()?;
        guard.vm_index = Some(vm_index);

        // 3. Materialize host dirs. The wrapper script + virtiofsd
        //    expect these to exist; per-sandbox dirs are unique
        //    (sandbox_id), per-user is shared across all of this
        //    user's sandboxes (and intentionally NOT cleaned up on
        //    sandbox stop).
        let keys_dir = host_dir.join("keys");
        let workspace_dir = host_dir.join("workspace");
        std::fs::create_dir_all(&keys_dir)
            .map_err(|e| format!("mkdir {}: {}", keys_dir.display(), e))?;
        std::fs::create_dir_all(&workspace_dir)
            .map_err(|e| format!("mkdir {}: {}", workspace_dir.display(), e))?;
        std::fs::create_dir_all(user_home_dir)
            .map_err(|e| format!("mkdir {}: {}", user_home_dir.display(), e))?;
        guard.host_dir_created = true;

        // 4. Write public key. The agent's verifier loads this from
        //    /run/keys/controller-pubkey at boot — which is the
        //    virtiofs-mounted view of `keys_dir`.
        //
        //    create + write_all + chmod 0444 + fsync, in that order:
        //    - 0444 because the file is a non-secret read-only
        //      attestation; we'd rather not let a buggy in-VM uid
        //      truncate it.
        //    - fsync so a host crash between the write and CH boot
        //      doesn't serve a 0-byte pubkey to the agent (which
        //      would 401 every signed request forever).
        let pubkey_path = keys_dir.join("controller-pubkey");
        write_pubkey_file(&pubkey_path, pubkey_b64.as_bytes())
            .map_err(|e| format!("write {}: {}", pubkey_path.display(), e))?;

        // 5. Build + submit the Nomad job spec.
        let job_json = build_nomad_job_json(
            job_id,
            &self.cfg,
            vm_index,
            &keys_dir,
            &workspace_dir,
            user_home_dir,
            user_id,
            project_id,
            &sandbox_id.to_string(),
        );
        submit_nomad_job(&self.cfg.nomad_ch.nomad_addr, &job_json).await?;
        guard.job_submitted = true;

        // 6. Poll until at least one alloc reaches running. Bounded
        //    by the Nomad-scheduling budget (alloc_running_timeout_secs);
        //    "running" here means the wrapper script started, NOT that
        //    the VM is up — the agent /livez wait below covers the
        //    in-VM boot path.
        wait_for_alloc_running(
            &self.cfg.nomad_ch.nomad_addr,
            job_id,
            Duration::from_secs(self.cfg.nomad_ch.alloc_running_timeout_secs),
        )
        .await?;

        // 7. Wait for the in-VM agent to come up. The wrapper boots
        //    CH; CH boots Linux; init.sh execs sandbox-agent. Bound
        //    this with its own budget (agent_livez_timeout_secs) so
        //    operators can tell apart "Nomad slow to schedule" from
        //    "VM/kernel/agent slow to boot".
        let agent_url =
            format!("http://10.99.{}.2:{AGENT_PORT}", 100u16 + vm_index);
        wait_for_agent_livez(
            &agent_url,
            Duration::from_secs(self.cfg.nomad_ch.agent_livez_timeout_secs),
        )
        .await?;

        // 8. Commit state. Refuse to overwrite an existing entry —
        //    a duplicate sandbox_id is a controller-bug or caller-bug,
        //    and silently overwriting would leak the prior entry's
        //    vm_index, host_dir, and Nomad job (the values still
        //    referenced by the old NomadChSandbox would never run
        //    through stop()). Bail with an error and let
        //    CreateGuard's Drop tear down the partial state we
        //    just built. DO NOT disarm the guard on this branch.
        {
            use std::collections::hash_map::Entry;
            let mut guard_state = self
                .state
                .write()
                .unwrap_or_else(|p| p.into_inner());
            match guard_state.entry(sandbox_id) {
                Entry::Vacant(slot) => {
                    slot.insert(NomadChSandbox {
                        user_id: user_id.to_string(),
                        job_id: job_id.to_string(),
                        vm_index,
                        host_dir: host_dir.to_path_buf(),
                        agent_url: agent_url.clone(),
                        signing_key: Arc::new(signing_key),
                    });
                }
                Entry::Occupied(_) => {
                    return Err(format!(
                        "[sandbox/nomad-ch] commit: sandbox_id {sandbox_id} \
                         already present in state map; refusing to overwrite \
                         (CreateGuard will tear down the just-built partial state)"
                    ));
                }
            }
        }

        let now = unix_now();
        Ok(SandboxInfo {
            sandbox_id: sandbox_id.to_string(),
            user_id: user_id.to_string(),
            project_id: project_id.to_string(),
            backend: "nomad-ch".to_string(),
            backend_hint: format!("job={job_id} vm_index={vm_index} key_fp={key_fp}"),
            created_at_secs: now,
            last_used_at_secs: now,
        })
    }

    /// Stop the sandbox.
    ///
    /// **Concurrent-stop semantics:** when two callers race `stop` on
    /// the same sandbox_id, the first one removes the entry from the
    /// in-memory state map and proceeds with Nomad-job-purge +
    /// vm_index release; the second one finds nothing in the map and
    /// returns `Ok(())` immediately, even though the underlying Nomad
    /// job teardown is still in flight from the first caller. This is
    /// intentional — `stop` is a "best-effort, idempotent" contract.
    /// Callers that need strict "fully gone" semantics (e.g. wait
    /// until the tap device is freed) should poll [`list`] or
    /// equivalent until the sandbox no longer appears.
    ///
    /// **vm_index leak on Nomad failure:** if `wait_for_job_gone`
    /// errors (Nomad API down, timeout, etc.) we do NOT release the
    /// vm_index back to the pool — a follow-up `create` for the same
    /// user could otherwise grab the same index and bind a tap device
    /// that the still-alive previous job is using. Better a slowly-
    /// shrinking pool than a tap collision; orphan-prune at next
    /// controller boot reclaims indices indirectly (by deleting the
    /// jobs that were holding them). The host_dir is left in place
    /// for the same reason — virtiofsd may still hold its socket open.
    pub async fn stop(&self, sandbox_id: Uuid) -> Result<(), String> {
        let sandbox = match self
            .state
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&sandbox_id)
        {
            Some(s) => s,
            None => return Ok(()), // idempotent
        };
        let mut errs: Vec<String> = Vec::new();

        // 1. Drain the agent. Best-effort — if /shutdown 5xx-s the
        //    Nomad purge in step 2 still tears the VM down.
        if let Err(e) = http_signed_async(
            &sandbox.signing_key,
            "POST",
            &format!("{}/shutdown", sandbox.agent_url),
            &[],
        )
        .await
        {
            eprintln!(
                "[sandbox/nomad-ch] /shutdown to {} failed (continuing): {e}",
                sandbox.job_id
            );
        }

        // 2. Stop + purge the Nomad job.
        if let Err(e) =
            stop_nomad_job(&self.cfg.nomad_ch.nomad_addr, &sandbox.job_id, true).await
        {
            errs.push(format!("stop_nomad_job({}): {e}", sandbox.job_id));
        }

        // 3. Wait for the job to actually be gone before we hand the
        //    vm_index back to the pool. Otherwise a follow-up
        //    `create` for the same user races a still-running
        //    wrapper script binding the same tap device + IP.
        let job_gone = wait_for_job_gone(
            &self.cfg.nomad_ch.nomad_addr,
            &sandbox.job_id,
            Duration::from_secs(30),
        )
        .await;
        let job_confirmed_gone = match job_gone {
            Ok(()) => true,
            Err(e) => {
                errs.push(format!("wait_for_job_gone({}): {e}", sandbox.job_id));
                false
            }
        };

        // 4. Release the VM index ONLY if the job is confirmed gone.
        //    If the Nomad API is down or the alloc is still reaping,
        //    a fresh `create` reusing this index could land on the
        //    same tap device the still-alive wrapper script is using.
        //    Leaking the index now and reclaiming it on next-boot
        //    orphan-prune is the safer trade-off.
        if job_confirmed_gone {
            self.vm_indices
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .release(sandbox.vm_index);
        } else {
            eprintln!(
                "[sandbox/nomad-ch] stop({}): wait_for_job_gone failed; \
                 leaking vm_index={} to avoid tap collision (orphan-prune \
                 will reclaim on next boot)",
                sandbox.job_id, sandbox.vm_index,
            );
        }

        // 5. Remove per-sandbox host dir. Per-user home dir is
        //    intentionally **not** touched. Skip the rm if the job
        //    teardown didn't confirm — virtiofsd may still hold the
        //    socket / share open, and pulling the dir from under it
        //    would just produce confusing logs.
        if job_confirmed_gone && sandbox.host_dir.exists() {
            if let Err(e) = std::fs::remove_dir_all(&sandbox.host_dir) {
                errs.push(format!(
                    "rm -rf {}: {}",
                    sandbox.host_dir.display(),
                    e
                ));
            }
        } else if !job_confirmed_gone && sandbox.host_dir.exists() {
            eprintln!(
                "[sandbox/nomad-ch] stop({}): leaking host_dir {} (job not \
                 confirmed gone)",
                sandbox.job_id,
                sandbox.host_dir.display(),
            );
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
        let resp =
            http_signed_async(&sk, "POST", &format!("{url}/exec"), body.as_bytes()).await?;
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
            return Err(format!(
                "agent /files GET status {}: {}",
                resp.status, resp.body
            ));
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
            return Err(format!(
                "agent /files PUT status {}: {}",
                resp.status, resp.body
            ));
        }
        Ok(())
    }

    pub async fn delete_file(&self, sandbox_id: Uuid, path: &str) -> Result<bool, String> {
        let (sk, url) = self.sandbox_keys(sandbox_id)?;
        let p = sanitize_path(path)?;
        let resp =
            http_signed_async(&sk, "DELETE", &format!("{url}/files/{p}"), &[]).await?;
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
        let guard = self.state.read().unwrap_or_else(|p| p.into_inner());
        let s = guard
            .get(&id)
            .ok_or_else(|| "sandbox not found in nomad-ch backend".to_string())?;
        // Arc clone is a refcount bump — cheap. Cloning the SigningKey
        // by value would heap-copy the 32-byte secret on every signed
        // RPC, doubling the in-memory key count for the duration of
        // the request (ed25519-dalek::SigningKey doesn't zeroize on
        // drop).
        Ok((s.signing_key.clone(), s.agent_url.clone()))
    }
}

// ─── create-time bookkeeping ────────────────────────────────────

/// RAII for the `creating_users` set. Same poison-recovery pattern
/// as the K8s backend: a panic that poisons the mutex would otherwise
/// lock the user out forever.
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
        let mut g = self.set.lock().unwrap_or_else(|p| p.into_inner());
        g.remove(&self.user_id);
    }
}

/// Tracks partial state during `create` so the failure tail can
/// undo whatever the success tail had already done. Drop runs when
/// `armed == true`; `disarm()` flips it on full success. The
/// per-step booleans (`host_dir_created`, `job_submitted`) gate
/// their respective cleanup branches so we don't, e.g., DELETE a
/// job that was never submitted.
///
/// **Drop is sync, but Drop must NOT block the compio worker.** When
/// a panic during `wait_for_alloc_running` (mid-`.await`) triggers
/// stack unwind, this `drop` runs *on the compio worker thread* —
/// blocking it on a 10-second `ureq::delete().call()` is the exact
/// stall the codebase is allergic to. Instead we hand the cleanup
/// I/O off to a detached compio task: it owns its own data, runs
/// best-effort, and the worker thread is freed immediately.
///
/// **Limitation:** if the runtime is already shutting down (process
/// exit, panic in main), `compio::runtime::spawn` may panic — we
/// catch that so a tearing-down process doesn't abort, and rely on
/// `cleanup_orphans_at_startup` (or a periodic prune) on the next
/// controller boot to mop up. This is the same best-effort contract
/// the prior "blocking ureq in Drop" had.
struct CreateGuard {
    vm_indices: Arc<Mutex<VmIndexAllocator>>,
    nomad_addr: String,
    job_id: String,
    host_dir: PathBuf,
    pub vm_index: Option<u16>,
    pub host_dir_created: bool,
    pub job_submitted: bool,
    armed: bool,
}

impl CreateGuard {
    fn new(
        vm_indices: Arc<Mutex<VmIndexAllocator>>,
        nomad_addr: String,
        job_id: String,
        host_dir: PathBuf,
    ) -> Self {
        Self {
            vm_indices,
            nomad_addr,
            job_id,
            host_dir,
            vm_index: None,
            host_dir_created: false,
            job_submitted: false,
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
        // Step 3 (vm_index release) is cheap + sync — finish it on
        // this thread so the index is back in the pool before any
        // observer might re-allocate. Use the same poison-recovery
        // pattern as the rest of the file (unwrap_or_else into_inner)
        // so a poisoned mutex doesn't fail the cleanup tail.
        if let Some(i) = self.vm_index.take() {
            self.vm_indices
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .release(i);
        }

        // Steps 1 + 2 (Nomad purge + host_dir rm -rf) are blocking
        // I/O. Detach them to a fire-and-forget compio task so this
        // Drop never blocks the worker thread. See the type-level
        // doc-comment for the runtime-shutdown caveat.
        let job_submitted = self.job_submitted;
        let nomad_addr = std::mem::take(&mut self.nomad_addr);
        let job_id = std::mem::take(&mut self.job_id);
        let host_dir_created = self.host_dir_created;
        let host_dir = std::mem::take(&mut self.host_dir);

        // Capture for the log line in the no-runtime branch — we just
        // moved the originals into the task closure.
        let job_id_log = job_id.clone();
        let host_dir_log = host_dir.clone();

        // `compio::runtime::spawn` panics if there is no current
        // runtime (e.g., this Drop fires during process teardown
        // *after* the runtime has already stopped). Catch that so we
        // don't turn a teardown into an abort. The work is best-
        // effort by contract — `cleanup_orphans_at_startup` (or a
        // periodic prune) covers leaked Nomad jobs on next boot.
        let spawn_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            compio::runtime::spawn(async move {
                if job_submitted {
                    let url = format!("{nomad_addr}/v1/job/{job_id}?purge=true");
                    if let Err(e) =
                        http_delete_unsigned(&url, Duration::from_secs(10)).await
                    {
                        eprintln!(
                            "[sandbox/nomad-ch] guard cleanup: purge {job_id} \
                             failed (best-effort): {e}"
                        );
                    }
                }
                if host_dir_created && host_dir.exists() {
                    if let Err(e) = std::fs::remove_dir_all(&host_dir) {
                        eprintln!(
                            "[sandbox/nomad-ch] guard cleanup: rm -rf {} \
                             failed (best-effort): {e}",
                            host_dir.display()
                        );
                    }
                }
            })
            .detach();
        }));
        if spawn_res.is_err() {
            eprintln!(
                "[sandbox/nomad-ch] guard cleanup: compio::spawn failed (no \
                 current runtime?); job {job_id_log} and dir {} left for \
                 next-boot orphan prune",
                host_dir_log.display(),
            );
        }
    }
}

/// Map fractional `SandboxConfig.cpus` (e.g. 2.0, 1.5) to the
/// integer `boot=N` count Cloud Hypervisor needs at command-line.
/// Round up — cfg.cpus is the *target* allocation; never starve the
/// VM by rounding down a 1.5 to 1. Floor at 1 so a misconfigured
/// cpus=0 still produces a bootable VM (the validate at config load
/// rejects cpus≤0, but defense-in-depth).
fn cpus_boot(cpus: f32) -> u32 {
    let n = cpus.ceil() as i64;
    if n < 1 { 1 } else { n as u32 }
}

// ─── Nomad job spec construction ────────────────────────────────

/// Build the JSON body for `POST /v1/jobs`. Returns the `{"Job": ...}`
/// envelope ready to ship.
///
/// The shape is the absolute minimum that Nomad accepts for a
/// service-type raw_exec job: TaskGroup count=1, RestartPolicy with
/// 0 attempts (the wrapper exits = the alloc dies; we don't want
/// Nomad to retry, the controller is the orchestrator), one Task
/// with `command = wrapper_path` and the env vars the wrapper
/// reads. Resources are advisory — `raw_exec` doesn't enforce them
/// (the cgroup is owned by the Nomad client, but CH ignores CPU
/// quota anyway). KillTimeout=10s is the same window the demo
/// wrapper's cleanup trap uses.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_nomad_job_json(
    job_id: &str,
    cfg: &SandboxConfig,
    vm_index: u16,
    keys_dir: &Path,
    workspace_dir: &Path,
    user_home_dir: &Path,
    user_id: &str,
    project_id: &str,
    sandbox_id: &str,
) -> serde_json::Value {
    serde_json::json!({
        "Job": {
            "ID": job_id,
            "Name": job_id,
            "Type": "service",
            "Datacenters": [cfg.nomad_ch.datacenter],
            "Meta": {
                "zeroship.user": user_id,
                "zeroship.project": project_id,
                "zeroship.sandbox": sandbox_id,
                "zeroship.vm_index": vm_index.to_string(),
            },
            "TaskGroups": [{
                "Name": "vm",
                "Count": 1,
                "RestartPolicy": {
                    "Attempts": 0,
                    "Mode": "fail",
                    "Interval": 30_000_000_000u64,    // 30s, ns
                    "Delay":     5_000_000_000u64,    //  5s, ns
                },
                "ReschedulePolicy": {
                    "Attempts": 0,
                    "Unlimited": false,
                },
                "Tasks": [{
                    "Name": "ch",
                    "Driver": "raw_exec",
                    "Config": {
                        "command": cfg.nomad_ch.wrapper_path.display().to_string(),
                    },
                    "Env": {
                        "ZSBX_VM_INDEX": vm_index.to_string(),
                        "ZSBX_HERE": cfg.nomad_ch.runtime_dir.display().to_string(),
                        "ZSBX_RUNTIME": "${NOMAD_TASK_DIR}",
                        "ZSBX_KEYS_DIR": keys_dir.display().to_string(),
                        "ZSBX_WORKSPACE_DIR": workspace_dir.display().to_string(),
                        "ZSBX_USER_HOME_DIR": user_home_dir.display().to_string(),
                        // Memory / CPU. The wrapper substitutes these
                        // into CH's `--memory size=${N}M,shared=on` and
                        // `--cpus boot=${N}` flags. Without these the
                        // wrapper would have no way to honour
                        // SandboxConfig.{memory_mb,cpus} — the Resources
                        // block is advisory-only on raw_exec.
                        "ZSBX_VM_MEMORY_MB": cfg.memory_mb.to_string(),
                        "ZSBX_VM_CPUS_BOOT": cpus_boot(cfg.cpus).to_string(),
                    },
                    "Resources": {
                        // CPU is in MHz units in the Nomad API.
                        // 1 vCPU ≈ 2000 MHz advisory; our
                        // SandboxConfig.cpus is fractional so
                        // multiply.
                        "CPU": (cfg.cpus * 2000.0) as u32,
                        "MemoryMB": cfg.memory_mb as u32,
                    },
                    "KillTimeout": 10_000_000_000u64,  // 10s, ns
                }],
            }],
        }
    })
}

// ─── Nomad HTTP helpers ─────────────────────────────────────────

#[derive(Debug)]
struct AgentResponse {
    status: u16,
    body: String,
    bytes: Vec<u8>,
}

/// Submit a job spec to Nomad. The body is the JSON returned by
/// [`build_nomad_job_json`].
async fn submit_nomad_job(
    nomad_addr: &str,
    job_json: &serde_json::Value,
) -> Result<(), String> {
    let url = format!("{nomad_addr}/v1/jobs");
    let body = serde_json::to_vec(job_json)
        .map_err(|e| format!("serialize Nomad job JSON: {e}"))?;
    let resp = http_post_json_unsigned(&url, &body, Duration::from_secs(15)).await?;
    if resp.status != 200 {
        return Err(format!(
            "POST {url} → status {}: {}",
            resp.status,
            resp.body.trim()
        ));
    }
    // Body is a JobRegisterResponse; we don't need to parse it for
    // success — Nomad returns 200 only after enqueue.
    Ok(())
}

async fn stop_nomad_job(
    nomad_addr: &str,
    job_id: &str,
    purge: bool,
) -> Result<(), String> {
    let url = format!(
        "{nomad_addr}/v1/job/{job_id}?purge={}",
        if purge { "true" } else { "false" }
    );
    let resp = http_delete_unsigned(&url, Duration::from_secs(15)).await?;
    if resp.status != 200 && resp.status != 404 {
        return Err(format!(
            "DELETE {url} → status {}: {}",
            resp.status,
            resp.body.trim()
        ));
    }
    Ok(())
}

/// Poll the job's allocations until at least one has
/// `ClientStatus == "running"`, or the deadline expires.
///
/// JSON parse errors are tracked + log-rate-limited (~once per 5s)
/// and surfaced in the timeout message, so an HTML proxy interstitial
/// or a 200-with-garbage from a misconfigured Nomad doesn't disappear
/// into a silent retry loop.
async fn wait_for_alloc_running(
    nomad_addr: &str,
    job_id: &str,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let url = format!("{nomad_addr}/v1/job/{job_id}/allocations");
    let mut last_status: Option<String> = None;
    let mut last_parse_err: Option<String> = None;
    let mut last_parse_log_at: Option<Instant> = None;
    while Instant::now() < deadline {
        let resp = http_get_unsigned(&url, Duration::from_secs(5)).await;
        if let Ok(r) = resp {
            if r.status == 200 {
                let allocs = match serde_json::from_str::<serde_json::Value>(&r.body) {
                    Ok(v) => v,
                    Err(e) => {
                        let msg = format!("{e}");
                        // Rate-limit the eprintln so a sustained
                        // garbage stream doesn't flood the log.
                        let now = Instant::now();
                        let stale = last_parse_log_at
                            .map(|t| now.duration_since(t) > Duration::from_secs(5))
                            .unwrap_or(true);
                        if stale {
                            eprintln!(
                                "[sandbox/nomad-ch] alloc poll: JSON parse \
                                 error (will retry): {msg}"
                            );
                            last_parse_log_at = Some(now);
                        }
                        last_parse_err = Some(msg);
                        compio::time::sleep(Duration::from_millis(250)).await;
                        continue;
                    }
                };
                let mut latest: Option<String> = None;
                for a in allocs.as_array().into_iter().flatten() {
                    let cs = a["ClientStatus"].as_str().unwrap_or("").to_string();
                    if cs == "running" {
                        return Ok(());
                    }
                    // Surface terminal failures fast — no point
                    // sitting through the timeout if the alloc
                    // already died.
                    if cs == "failed" || cs == "lost" {
                        let desc = a["ClientDescription"]
                            .as_str()
                            .unwrap_or("")
                            .to_string();
                        return Err(format!(
                            "nomad alloc terminal status={cs}: {desc}"
                        ));
                    }
                    latest = Some(cs);
                }
                last_status = latest.or(last_status);
            }
        }
        compio::time::sleep(Duration::from_millis(250)).await;
    }
    let mut msg = format!(
        "nomad alloc never reached running for job {job_id} (last status={:?})",
        last_status.unwrap_or_else(|| "<no allocs>".to_string())
    );
    if let Some(e) = last_parse_err {
        msg.push_str(&format!("; last parse error: {e}"));
    }
    Err(msg)
}

/// Block until `GET /v1/job/<id>` returns 404 (or the job is in a
/// terminal Status like `dead` with `Stop=true`). Bounded; surfaces
/// the Nomad status (and any sustained JSON parse errors) on timeout
/// so operators can investigate.
async fn wait_for_job_gone(
    nomad_addr: &str,
    job_id: &str,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let url = format!("{nomad_addr}/v1/job/{job_id}");
    let mut last_parse_err: Option<String> = None;
    let mut last_parse_log_at: Option<Instant> = None;
    while Instant::now() < deadline {
        let resp = http_get_unsigned(&url, Duration::from_secs(5)).await;
        match resp {
            Ok(r) if r.status == 404 => return Ok(()),
            Ok(r) if r.status == 200 => {
                match serde_json::from_str::<serde_json::Value>(&r.body) {
                    Ok(v) => {
                        let status = v["Status"].as_str().unwrap_or("");
                        let stop = v["Stop"].as_bool().unwrap_or(false);
                        if status == "dead" && stop {
                            // After purge=true, Nomad GC takes a beat
                            // to remove the record entirely — but the
                            // job is already terminated; vm_index is
                            // safe to release.
                            return Ok(());
                        }
                    }
                    Err(e) => {
                        let msg = format!("{e}");
                        let now = Instant::now();
                        let stale = last_parse_log_at
                            .map(|t| now.duration_since(t) > Duration::from_secs(5))
                            .unwrap_or(true);
                        if stale {
                            eprintln!(
                                "[sandbox/nomad-ch] job-gone poll: JSON parse \
                                 error (will retry): {msg}"
                            );
                            last_parse_log_at = Some(now);
                        }
                        last_parse_err = Some(msg);
                    }
                }
            }
            _ => {}
        }
        compio::time::sleep(Duration::from_millis(250)).await;
    }
    let mut msg = format!("job {job_id} did not disappear within timeout");
    if let Some(e) = last_parse_err {
        msg.push_str(&format!("; last parse error: {e}"));
    }
    Err(msg)
}

// ─── Generic HTTP (unsigned — Nomad API) ─────────────────────────

async fn http_get_unsigned(url: &str, timeout: Duration) -> Result<AgentResponse, String> {
    let url = url.to_string();
    compio::runtime::spawn_blocking(move || {
        let req = ureq::get(&url).timeout(timeout);
        send_ureq(req, &[])
    })
    .await
    .map_err(|e| format!("blocking task panic: {e:?}"))?
}

async fn http_delete_unsigned(
    url: &str,
    timeout: Duration,
) -> Result<AgentResponse, String> {
    let url = url.to_string();
    compio::runtime::spawn_blocking(move || {
        let req = ureq::delete(&url).timeout(timeout);
        send_ureq(req, &[])
    })
    .await
    .map_err(|e| format!("blocking task panic: {e:?}"))?
}

async fn http_post_json_unsigned(
    url: &str,
    body: &[u8],
    timeout: Duration,
) -> Result<AgentResponse, String> {
    let url = url.to_string();
    let body = body.to_vec();
    compio::runtime::spawn_blocking(move || {
        let req = ureq::post(&url)
            .timeout(timeout)
            .set("content-type", "application/json");
        send_ureq(req, &body)
    })
    .await
    .map_err(|e| format!("blocking task panic: {e:?}"))?
}

fn send_ureq(req: ureq::Request, body: &[u8]) -> Result<AgentResponse, String> {
    let send = if body.is_empty() {
        req.call()
    } else {
        req.send_bytes(body)
    };
    match send {
        Ok(resp) => {
            let status = resp.status();
            let mut bytes = Vec::new();
            let _ = resp
                .into_reader()
                .take(64 * 1024 * 1024)
                .read_to_end(&mut bytes);
            let body = String::from_utf8_lossy(&bytes).into_owned();
            Ok(AgentResponse {
                status,
                body,
                bytes,
            })
        }
        Err(ureq::Error::Status(code, resp)) => {
            // 8 KiB cap on error bodies — matches k8s.rs. Nomad +
            // agent error responses are tiny JSON; anything larger
            // is almost certainly an HTML interstitial we don't want
            // copied verbatim into our log lines.
            let mut bytes = Vec::new();
            let _ = resp.into_reader().take(8 * 1024).read_to_end(&mut bytes);
            let body = String::from_utf8_lossy(&bytes).into_owned();
            Ok(AgentResponse {
                status: code,
                body,
                bytes,
            })
        }
        Err(e) => Err(format!("{e}")),
    }
}

// ─── Signed agent HTTP (mirror of K8s `http_signed_async`) ───────

/// Async wrapper that signs + sends to the in-VM agent without
/// blocking the ntex worker. **Signing happens inside the closure**
/// (after the spawn_blocking queue drains) so the agent's 5-second
/// skew window doesn't fire on a queued request. See
/// `crates/sandbox/src/backend/k8s.rs::http_signed_async` for the
/// full rationale; this is a verbatim copy with the same semantics.
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

    // Arc clone — refcount bump, NOT a 32-byte secret copy.
    let signing_key = Arc::clone(signing_key);
    let method = method.to_string();
    let url = url.to_string();
    let body = body.to_vec();
    compio::runtime::spawn_blocking(move || {
        let ts = unix_now();
        let nonce = random_nonce()?;
        let signature = sig::sign(&signing_key, &method, &path, &body, ts, &nonce);
        signed_blocking_call(&method, &url, &body, ts, &nonce, &signature)
    })
    .await
    .map_err(|e| format!("blocking task panic: {e:?}"))?
}

fn signed_blocking_call(
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
    send_ureq(req, body).map_err(|e| format!("{method} {url}: {e}"))
}

/// Poll `/livez` until 200 or the deadline expires. Async wrapper
/// over a blocking ureq call (mirrors the K8s helper of the same name).
async fn wait_for_agent_livez(base_url: &str, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let url = format!("{base_url}/livez");
    while Instant::now() < deadline {
        let probe_url = url.clone();
        let status = compio::runtime::spawn_blocking(move || {
            ureq::get(&probe_url)
                .timeout(Duration::from_millis(500))
                .call()
                .map(|r| r.status())
                .ok()
        })
        .await
        .ok()
        .flatten();
        if status == Some(200) {
            return Ok(());
        }
        // 150 ms livez poll cadence — matches k8s.rs.
        compio::time::sleep(Duration::from_millis(150)).await;
    }
    Err(format!("agent at {base_url} never returned 200 on /livez"))
}

// ─── small utilities (mirrored from k8s.rs) ─────────────────────

/// Write `controller-pubkey`: create + write_all + chmod 0444 +
/// sync_all. fsync so a host crash between write and CH boot doesn't
/// serve a 0-byte pubkey to the agent.
fn write_pubkey_file(path: &Path, body: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;
    f.write_all(body)?;
    let mut perms = f.metadata()?.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o444);
    }
    #[cfg(not(unix))]
    {
        perms.set_readonly(true);
    }
    std::fs::set_permissions(path, perms)?;
    f.sync_all()?;
    Ok(())
}

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

/// Pre-validate request paths before forwarding to the agent. The
/// agent has its own (stricter) checks via openat2; this gives a
/// cleaner 4xx without consuming a nonce on a doomed request.
/// Identical to the K8s helper — the contract is per-agent, not
/// per-backend, so the rules must match.
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

/// Validate user_id / project_id at the backend boundary.
/// **Mirror of `k8s.rs::validate_id` — keep them in sync.** Both
/// repeat the HTTP-handler rule as defense-in-depth; cross-module
/// sharing is intentionally avoided in this PR (the deduplication
/// belongs in a follow-up that consolidates the validate helpers
/// once we have ≥ 3 backends needing them).
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

#[cfg(test)]
mod tests {
    use super::*;

    // ─── VmIndexAllocator ────────────────────────────────────

    #[test]
    fn vm_alloc_starts_at_floor() {
        let mut a = VmIndexAllocator::new(1, 5);
        assert_eq!(a.alloc().unwrap(), 1);
        assert_eq!(a.alloc().unwrap(), 2);
        assert_eq!(a.alloc().unwrap(), 3);
    }

    #[test]
    fn vm_alloc_reuses_freed_smallest_first() {
        let mut a = VmIndexAllocator::new(10, 20);
        let i1 = a.alloc().unwrap();
        let i2 = a.alloc().unwrap();
        let i3 = a.alloc().unwrap();
        assert_eq!((i1, i2, i3), (10, 11, 12));

        // Free out of order — smallest reused first.
        a.release(i2);
        a.release(i1);
        assert_eq!(a.alloc().unwrap(), 10);
        assert_eq!(a.alloc().unwrap(), 11);
        // After exhausting freed, fall back to next monotonic.
        assert_eq!(a.alloc().unwrap(), 13);
    }

    #[test]
    fn vm_alloc_exhaustion() {
        let mut a = VmIndexAllocator::new(1, 3);
        assert_eq!(a.alloc().unwrap(), 1);
        assert_eq!(a.alloc().unwrap(), 2);
        assert_eq!(a.alloc().unwrap(), 3);
        let err = a.alloc().expect_err("should be exhausted");
        assert!(err.contains("exhausted"), "{err}");
    }

    #[test]
    fn vm_alloc_release_outside_range_is_noop() {
        let mut a = VmIndexAllocator::new(10, 20);
        // Below floor: ignored.
        a.release(5);
        // Above ceil: ignored.
        a.release(25);
        // Allocator is still in its initial state.
        assert_eq!(a.alloc().unwrap(), 10);
    }

    #[test]
    fn vm_alloc_single_element_range() {
        let mut a = VmIndexAllocator::new(7, 7);
        assert_eq!(a.alloc().unwrap(), 7);
        assert!(a.alloc().is_err());
        a.release(7);
        assert_eq!(a.alloc().unwrap(), 7);
    }

    // ─── State-map collision (C1) ────────────────────────────
    //
    // The fix in `try_create` step 8 uses `HashMap::entry` to refuse
    // overwriting an existing sandbox_id. We can't drive the full
    // `try_create` path from a unit test (no Nomad), but we can
    // verify the entry-API contract directly: a duplicate insert
    // must NOT overwrite the prior value.
    #[test]
    fn state_map_entry_api_refuses_overwrite() {
        use std::collections::hash_map::Entry;
        let mut m: HashMap<Uuid, &'static str> = HashMap::new();
        let id = Uuid::nil();
        // First insert: vacant → take.
        match m.entry(id) {
            Entry::Vacant(slot) => {
                slot.insert("first");
            }
            Entry::Occupied(_) => panic!("first insert should be vacant"),
        }
        // Second insert with same id: occupied → must NOT overwrite.
        let mut would_overwrite = false;
        match m.entry(id) {
            Entry::Vacant(_) => {
                would_overwrite = true;
            }
            Entry::Occupied(o) => {
                // Existing entry is preserved unchanged.
                assert_eq!(*o.get(), "first");
            }
        }
        assert!(
            !would_overwrite,
            "entry-API must report Occupied on duplicate id"
        );
        // Final state is the original — proves no leak of prior
        // bookkeeping (vm_index/host_dir/job_id in the real type).
        assert_eq!(m.get(&id), Some(&"first"));
    }

    // ─── Nomad job spec ──────────────────────────────────────

    fn make_cfg() -> SandboxConfig {
        let mut cfg = SandboxConfig {
            port: 9091,
            token: crate::config::ApiToken::new("x"),
            backend: "nomad-ch".into(),
            image: "img".into(),
            workspace_root: PathBuf::from("/var/zeroship/projects"),
            network: "n".into(),
            memory_mb: 1024,
            cpus: 2.0,
            idle_timeout_secs: 1800,
            max_lifetime_secs: 28800,
            auto_pull: false,
            k8s: crate::config::K8sConfig {
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
            nomad_ch: crate::config::NomadCHConfig {
                nomad_addr: "http://127.0.0.1:4646".into(),
                datacenter: "dc1".into(),
                wrapper_path: PathBuf::from("/etc/zeroship/nomad-vm-wrapper.sh"),
                runtime_dir: PathBuf::from("/var/lib/zeroship/ch"),
                host_state_dir: PathBuf::from("/var/zeroship/ch"),
                user_home_dir_root: PathBuf::from("/var/zeroship/ch/users"),
                vm_index_floor: 1,
                vm_index_ceil: 155,
                alloc_running_timeout_secs: 60,
                agent_livez_timeout_secs: 30,
                startup_orphan_cleanup: false,
            },
        };
        cfg.cpus = 2.0;
        cfg
    }

    #[test]
    fn cpus_boot_rounds_up_and_floors_at_1() {
        assert_eq!(cpus_boot(2.0), 2);
        assert_eq!(cpus_boot(1.5), 2);
        assert_eq!(cpus_boot(0.1), 1);
        assert_eq!(cpus_boot(0.0), 1);
        assert_eq!(cpus_boot(-1.0), 1);
        assert_eq!(cpus_boot(8.0), 8);
    }

    #[test]
    fn nomad_job_json_basic_shape() {
        let cfg = make_cfg();
        let v = build_nomad_job_json(
            "zsbx-abc",
            &cfg,
            7,
            Path::new("/var/zeroship/ch/abc/keys"),
            Path::new("/var/zeroship/ch/abc/workspace"),
            Path::new("/var/zeroship/ch/users/alice/home"),
            "alice",
            "proj1",
            "abc",
        );
        let job = &v["Job"];
        assert_eq!(job["ID"], "zsbx-abc");
        assert_eq!(job["Name"], "zsbx-abc");
        assert_eq!(job["Type"], "service");
        assert_eq!(job["Datacenters"][0], "dc1");
        assert_eq!(job["Meta"]["zeroship.user"], "alice");
        assert_eq!(job["Meta"]["zeroship.vm_index"], "7");

        let group = &job["TaskGroups"][0];
        assert_eq!(group["Count"], 1);
        assert_eq!(group["RestartPolicy"]["Attempts"], 0);
        assert_eq!(group["RestartPolicy"]["Mode"], "fail");
        assert_eq!(group["ReschedulePolicy"]["Attempts"], 0);

        let task = &group["Tasks"][0];
        assert_eq!(task["Driver"], "raw_exec");
        assert_eq!(
            task["Config"]["command"],
            "/etc/zeroship/nomad-vm-wrapper.sh"
        );
        assert_eq!(task["Env"]["ZSBX_VM_INDEX"], "7");
        assert_eq!(task["Env"]["ZSBX_HERE"], "/var/lib/zeroship/ch");
        assert_eq!(task["Env"]["ZSBX_RUNTIME"], "${NOMAD_TASK_DIR}");
        assert_eq!(
            task["Env"]["ZSBX_KEYS_DIR"],
            "/var/zeroship/ch/abc/keys"
        );
        assert_eq!(
            task["Env"]["ZSBX_WORKSPACE_DIR"],
            "/var/zeroship/ch/abc/workspace"
        );
        assert_eq!(
            task["Env"]["ZSBX_USER_HOME_DIR"],
            "/var/zeroship/ch/users/alice/home"
        );
        // The wrapper reads memory + cpu count from these two env vars.
        // Resources.{CPU,MemoryMB} are advisory-only on raw_exec; the
        // wrapper would otherwise hardcode 1024M/2vCPU and lie to
        // bin-packing.
        assert_eq!(task["Env"]["ZSBX_VM_MEMORY_MB"], "1024");
        assert_eq!(task["Env"]["ZSBX_VM_CPUS_BOOT"], "2");
        // 2.0 vCPU advisory → 4000 MHz.
        assert_eq!(task["Resources"]["CPU"], 4000);
        assert_eq!(task["Resources"]["MemoryMB"], 1024);
        // KillTimeout is 10 seconds in nanoseconds.
        assert_eq!(task["KillTimeout"], 10_000_000_000u64);
    }

    #[test]
    fn nomad_job_json_serializes_to_valid_json() {
        let cfg = make_cfg();
        let v = build_nomad_job_json(
            "zsbx-x",
            &cfg,
            1,
            Path::new("/k"),
            Path::new("/w"),
            Path::new("/u"),
            "u",
            "p",
            "s",
        );
        let s = serde_json::to_string(&v).expect("serialize");
        // Round-trip — Nomad parses as JSON, so we should too.
        let _: serde_json::Value =
            serde_json::from_str(&s).expect("round-trip parse");
    }

    // ─── path / id sanitizers (mirror k8s.rs unit tests) ────

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
    }

    #[test]
    fn validate_id_accepts_lowercase_dns_subset() {
        assert!(validate_id("alice", "user_id").is_ok());
        assert!(validate_id("alice-1", "user_id").is_ok());
        assert!(validate_id("0u", "user_id").is_ok());
    }

    #[test]
    fn validate_id_rejects_bad_chars() {
        assert!(validate_id("Alice", "user_id").is_err());
        assert!(validate_id("alice_1", "user_id").is_err());
        assert!(validate_id("", "user_id").is_err());
        assert!(validate_id(&"a".repeat(51), "user_id").is_err());
        assert!(validate_id("-alice", "user_id").is_err());
    }
}
