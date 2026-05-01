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
//! - `create` is wrapped in a `CreateGuard` whose Drop spawns a
//!   detached compio task that tears down partial state. The task:
//!   (a) purges the Nomad job, (b) on confirmed-purge releases the
//!   vm_index back to the pool, (c) on confirmed-purge `rm -rf`s the
//!   host_dir. **The vm_index is intentionally NOT released until
//!   the Nomad purge confirms** — releasing it earlier risks a
//!   retry-`create` for the same user grabbing the same index and
//!   racing the still-alive prior wrapper for `tap=zsbx-nm-<idx>`.
//!   Same policy as the `stop` path: "release on confirmed purge;
//!   leak otherwise; orphan-prune mops up later."
//! - On controller crash or runtime-shutdown the cleanup task may
//!   not run; any leaked Nomad jobs persist until
//!   [`NomadCHBackend::cleanup_orphans_at_startup`] reclaims them at
//!   next boot. **That cleanup defaults OFF and is opt-in via
//!   `SANDBOX_NOMAD_CH_STARTUP_ORPHAN_CLEANUP=true`** — single-
//!   replica operators should turn it on. host_dir cleanup is best-
//!   effort.
//! - `stop` is idempotent (returns Ok if the sandbox isn't in the
//!   in-memory map). The Nomad job is purged, vm_index returned to
//!   the pool **only on confirmed purge** (else leaked), and the
//!   per-sandbox host_dir is `rm -rf`'d. The per-user home dir is
//!   **never** deleted by `stop` — it's user-scoped state.
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
    /// Per-VM-index pool, free-list backed. Renamed from
    /// `vm_indices` in round 3 (m1) — the new name reads as a
    /// component (an allocator) rather than a plural noun
    /// (a collection of indices).
    vm_index_allocator: Arc<Mutex<VmIndexAllocator>>,
    /// Per-user serialization gate (one in-flight `create` per user).
    creating_users: Arc<Mutex<HashSet<String>>>,
    healthy: Arc<AtomicBool>,
    /// M7 circuit-breaker: most recent probe error, captured by
    /// [`probe`]. Read by [`create`] to assemble the operator-
    /// readable "backend unhealthy" message. Mutex (not RwLock)
    /// because the only writer is the periodic probe and reads are
    /// rare; Mutex is simpler and the contention is irrelevant.
    last_probe_err: Arc<Mutex<Option<String>>>,
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

// SECRET-HYGIENE: this Debug impl is **load-bearing**. The
// `signing_key: Arc<SigningKey>` field MUST NEVER appear in any
// debug output, even via the transitive chain
//   Backend::Debug → state.read() → HashMap → NomadChSandbox::fmt
// Adding `#[derive(Debug)]` to NomadChSandbox would dump the secret
// into any panic backtrace / error log / `dbg!()` call. The
// finish_non_exhaustive() below is what closes that hole — keep
// that final clause and do NOT switch to a derive even when adding
// a new field. ed25519-dalek::SigningKey doesn't have a redacting
// Debug impl of its own.
impl std::fmt::Debug for NomadChSandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NomadChSandbox")
            .field("user_id", &self.user_id)
            .field("job_id", &self.job_id)
            .field("vm_index", &self.vm_index)
            .field("host_dir", &self.host_dir)
            .field("agent_url", &self.agent_url)
            // signing_key intentionally omitted — see above.
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
            vm_index_allocator: Arc::new(Mutex::new(alloc)),
            creating_users: Arc::new(Mutex::new(HashSet::new())),
            healthy: Arc::new(AtomicBool::new(false)),
            last_probe_err: Arc::new(Mutex::new(None)),
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
                *self.last_probe_err.lock().unwrap_or_else(|p| p.into_inner()) = None;
                Ok(())
            }
            Ok(r) => {
                self.healthy.store(false, Ordering::Relaxed);
                let msg = format!(
                    "nomad /v1/status/leader → status {}: {}",
                    r.status,
                    r.body.trim()
                );
                *self.last_probe_err.lock().unwrap_or_else(|p| p.into_inner()) =
                    Some(msg.clone());
                Err(msg)
            }
            Err(e) => {
                self.healthy.store(false, Ordering::Relaxed);
                let msg = format!("nomad probe failed: {e}");
                *self.last_probe_err.lock().unwrap_or_else(|p| p.into_inner()) =
                    Some(msg.clone());
                Err(msg)
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
                eprintln!(
                    "[sandbox/nomad-ch] orphan-cleanup: purge {id} failed: {e}"
                );
                continue;
            }
            deleted += 1;
        }
        if deleted > 0 {
            eprintln!(
                "[sandbox/nomad-ch] orphan-cleanup: purged {deleted} orphan job(s)"
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

        // M7 circuit-breaker. Pool exhaustion under partial-failure
        // storm: 50 concurrent stalled Nomad RPCs would saturate
        // the spawn_blocking pool, queueing every other backend op
        // controller-wide. The probe loop sets `healthy=false` on
        // any 5xx / transport error from Nomad; bail BEFORE we
        // submit the next RPC into a backend known to be down. A
        // *terminal* error — by contract this is configuration /
        // infra, not transient (the probe is what flips healthy
        // back to true), so callers should not retry.
        if !self.is_healthy() {
            let last = self
                .last_probe_err
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
                .unwrap_or_else(|| "<no probe error captured>".to_string());
            return Err(format!(
                "nomad-ch backend unhealthy; refusing new sandboxes \
                 (most recent probe error: {last}). This is a config \
                 or infra problem; the probe loop will flip the bit \
                 back when Nomad recovers."
            ));
        }

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
                "[sandbox/nomad-ch] create: user {user_id} already has sandbox {old_id}; stopping first"
            );
            if let Err(e) = self.stop(old_id).await {
                eprintln!(
                    "[sandbox/nomad-ch] create: stop({old_id}) failed: {e}"
                );
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
            self.vm_index_allocator.clone(),
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

    /// Inner body of [`Self::create`] — split out so the
    /// `CreateGuard` Drop runs on every error path without the
    /// caller having to remember `?` discipline. Not part of the
    /// public API; called only from `create()`.
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
        let create_started = Instant::now();
        // 1. Mint Ed25519 keypair. Public half is the only thing
        //    that leaves this process; the private half stays in
        //    `signing_key` for the lifetime of the sandbox.
        //
        //    Wrap in Arc immediately so step 7's livez+fingerprint
        //    probe can sign /version without taking ownership; we
        //    take a fresh `Arc::clone` (cheap refcount bump) at the
        //    commit step so the in-state-map sandbox owns its own
        //    handle.
        let sk_bytes = random_key32()?;
        let signing_key = Arc::new(SigningKey::from_bytes(&sk_bytes));
        let pubkey = signing_key.verifying_key();
        let pubkey_b64 = B64.encode(pubkey.as_bytes());
        let key_fp = sig::pubkey_fingerprint(&pubkey);
        eprintln!(
            "[sandbox/nomad-ch] create: sandbox={sandbox_id} user={user_id} \
             project={project_id} key_fp={key_fp}"
        );

        // 2. Allocate VM index from the pool. Track in the guard so
        //    cleanup-on-failure releases it.
        let vm_index = self
            .vm_index_allocator
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
        .await
        .map_err(|e| {
            eprintln!(
                "[sandbox/nomad-ch] create: error sandbox={sandbox_id} \
                 step=wait_for_alloc_running error={e}"
            );
            e
        })?;
        eprintln!(
            "[sandbox/nomad-ch] create: alloc=running sandbox={sandbox_id} \
             vm_index={vm_index} job={job_id} \
             elapsed_ms={}",
            create_started.elapsed().as_millis()
        );

        // 7. Wait for the in-VM agent to come up. The wrapper boots
        //    CH; CH boots Linux; init.sh execs sandbox-agent. Bound
        //    this with its own budget (agent_livez_timeout_secs) so
        //    operators can tell apart "Nomad slow to schedule" from
        //    "VM/kernel/agent slow to boot".
        // M6: second octet is configurable so an operator with a
        // corp 10.99/16 collision can shift to a different private
        // /16. Both the controller and the wrapper read the same
        // value (controller from `cfg.nomad_ch.subnet_second_octet`,
        // wrapper from `ZSBX_SUBNET_BASE_OCTET` env var passed by
        // build_nomad_job_json).
        //
        // FM-A: also pass `key_fp` + signing_key so wait_for_agent_livez
        // verifies the agent answering /livez is OUR agent (verifies
        // our pubkey on /version), not a stale tenant whose CH is
        // still alive after Nomad already reported the prior alloc
        // terminal. Without this, a fresh create() racing the prior
        // wrapper's process tree returns 201 in 0.25 s pointing at
        // an agent that dies seconds later → "No route to host" on
        // every subsequent /exec.
        let agent_url = format!(
            "http://10.{}.{}.2:{AGENT_PORT}",
            self.cfg.nomad_ch.subnet_second_octet,
            100u16 + vm_index
        );
        let livez_started = Instant::now();
        wait_for_agent_livez(
            &agent_url,
            &key_fp,
            &signing_key,
            Duration::from_secs(self.cfg.nomad_ch.agent_livez_timeout_secs),
        )
        .await
        .map_err(|e| {
            eprintln!(
                "[sandbox/nomad-ch] create: error sandbox={sandbox_id} \
                 step=wait_for_agent_livez agent_url={agent_url} error={e}"
            );
            e
        })?;
        eprintln!(
            "[sandbox/nomad-ch] create: agent_ready sandbox={sandbox_id} \
             vm_index={vm_index} key_fp={key_fp} elapsed_ms={}",
            livez_started.elapsed().as_millis()
        );

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
                        // signing_key is already Arc<SigningKey> at
                        // step 1 (so wait_for_agent_livez can borrow
                        // it for its /version probe); move into the
                        // state map verbatim.
                        signing_key,
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
        // Channel split: `errs` accumulates per-step failures the
        // caller needs to see (joined into the returned Result) so
        // the API surface lines up with the K8s backend; `eprintln`
        // is reserved for operator-only diagnostics that don't
        // belong in the API response (vm_index leak warnings,
        // host_dir-skipped notices). Keeping these distinct means a
        // 200 stop() with operator log noise is observable, and a
        // 5xx stop() carries the actionable error text.
        let mut errs: Vec<String> = Vec::new();

        // 1. Drain the agent. Best-effort — if /shutdown 5xx-s the
        //    Nomad purge in step 2 still tears the VM down. We feed
        //    the error into `errs` (rather than just eprintln) so
        //    the caller sees it; symmetric with steps 2/3/5 below.
        //    The aggregate error is non-fatal — we keep going through
        //    the cleanup tail regardless.
        if let Err(e) = http_signed_async(
            &sandbox.signing_key,
            "POST",
            &format!("{}/shutdown", sandbox.agent_url),
            &[],
        )
        .await
        {
            errs.push(format!(
                "/shutdown to {}: {e} (continuing with Nomad purge)",
                sandbox.job_id
            ));
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
            self.vm_index_allocator
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
        let ctx = self.sandbox_log_ctx(sandbox_id);
        let body = serde_json::json!({
            "cmd": cmd,
            "cwd": cwd,
            "timeout_ms": timeout_ms,
        })
        .to_string();
        let resp = http_signed_async(&sk, "POST", &format!("{url}/exec"), body.as_bytes())
            .await
            .map_err(|e| format!("{ctx} agent /exec: {e}"))?;
        if resp.status != 200 {
            return Err(format!(
                "{ctx} agent /exec status {}: {}",
                resp.status, resp.body
            ));
        }
        let v: serde_json::Value = serde_json::from_str(&resp.body)
            .map_err(|e| format!("{ctx} agent /exec response not JSON: {e}"))?;
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
        let ctx = self.sandbox_log_ctx(sandbox_id);
        let p = sanitize_path(path)?;
        let resp = http_signed_async(&sk, "GET", &format!("{url}/files/{p}"), &[])
            .await
            .map_err(|e| format!("{ctx} agent /files GET: {e}"))?;
        if resp.status == 404 {
            return Err(format!("{ctx} file not found: {p}"));
        }
        if resp.status != 200 {
            return Err(format!(
                "{ctx} agent /files GET status {}: {}",
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
        let ctx = self.sandbox_log_ctx(sandbox_id);
        let p = sanitize_path(path)?;
        let resp = http_signed_async(&sk, "PUT", &format!("{url}/files/{p}"), body)
            .await
            .map_err(|e| format!("{ctx} agent /files PUT: {e}"))?;
        if resp.status != 200 {
            return Err(format!(
                "{ctx} agent /files PUT status {}: {}",
                resp.status, resp.body
            ));
        }
        Ok(())
    }

    pub async fn delete_file(&self, sandbox_id: Uuid, path: &str) -> Result<bool, String> {
        let (sk, url) = self.sandbox_keys(sandbox_id)?;
        let ctx = self.sandbox_log_ctx(sandbox_id);
        let p = sanitize_path(path)?;
        let resp = http_signed_async(&sk, "DELETE", &format!("{url}/files/{p}"), &[])
            .await
            .map_err(|e| format!("{ctx} agent /files DELETE: {e}"))?;
        match resp.status {
            200 => Ok(true),
            404 => Ok(false),
            s => Err(format!(
                "{ctx} agent /files DELETE status {s}: {}",
                resp.body
            )),
        }
    }

    pub async fn file_tree(&self, sandbox_id: Uuid) -> Result<Vec<TreeEntry>, String> {
        let (sk, url) = self.sandbox_keys(sandbox_id)?;
        let ctx = self.sandbox_log_ctx(sandbox_id);
        let resp = http_signed_async(&sk, "GET", &format!("{url}/tree"), &[])
            .await
            .map_err(|e| format!("{ctx} agent /tree: {e}"))?;
        if resp.status != 200 {
            return Err(format!(
                "{ctx} agent /tree status {}: {}",
                resp.status, resp.body
            ));
        }
        let v: serde_json::Value = serde_json::from_str(&resp.body)
            .map_err(|e| format!("{ctx} agent /tree response not JSON: {e}"))?;
        let entries = v["entries"]
            .as_array()
            .ok_or_else(|| format!("{ctx} agent /tree: missing 'entries' array"))?;
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

    /// Build a short prefix for agent-error log lines so a fleet-
    /// wide log search can pivot on sandbox / vm_index / job (M1).
    /// Format: `[sandbox=<id> vm_index=<idx> job=<job>]`. Returns
    /// just `[sandbox=<id>]` when the entry is missing — the
    /// callers already handle "sandbox not found" via
    /// `sandbox_keys`, so this only fires on the wide-window after
    /// a stop() racing with an in-flight RPC.
    fn sandbox_log_ctx(&self, id: Uuid) -> String {
        let guard = self.state.read().unwrap_or_else(|p| p.into_inner());
        match guard.get(&id) {
            Some(s) => format!(
                "[sandbox={} vm_index={} job={}]",
                id, s.vm_index, s.job_id
            ),
            None => format!("[sandbox={id}]"),
        }
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
/// **vm_index ordering (C1).** The vm_index is released ONLY after
/// the Nomad purge HTTP call confirms (status 200/404). Mirrors the
/// `stop` path's policy: a follow-up `create` for the same user
/// could otherwise reuse the index and race the still-alive prior
/// `raw_exec` wrapper for `tap=zsbx-nm-<idx>`. Up to ~10s elapse
/// between "Drop fires" and "purge confirms"; releasing the index
/// inline (as a previous revision did) reopened the same window the
/// `stop`-path I1 fix was guarding against. On purge failure (5xx,
/// timeout) the index is leaked; `cleanup_orphans_at_startup` (or
/// the next-boot orphan prune) reclaims it indirectly by deleting
/// the surviving job.
///
/// **Limitation:** if the runtime is already shutting down (process
/// exit, panic in main), `compio::runtime::spawn` may panic — we
/// catch that so a tearing-down process doesn't abort, and rely on
/// `cleanup_orphans_at_startup` (or a periodic prune) on the next
/// controller boot to mop up. This is the same best-effort contract
/// the prior "blocking ureq in Drop" had.
struct CreateGuard {
    vm_index_allocator: Arc<Mutex<VmIndexAllocator>>,
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
        vm_index_allocator: Arc<Mutex<VmIndexAllocator>>,
        nomad_addr: String,
        job_id: String,
        host_dir: PathBuf,
    ) -> Self {
        Self {
            vm_index_allocator,
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
        // Move ALL cleanup state into the detached task. The
        // vm_index release is intentionally part of the detached
        // task too — see the C1 note on the type. Releasing the
        // index inline (before the Nomad purge confirms) is a race
        // window: a retry-`create` for the same user could grab the
        // same index and bind a tap device the still-alive prior
        // wrapper is using.
        let job_submitted = self.job_submitted;
        let nomad_addr = std::mem::take(&mut self.nomad_addr);
        let job_id = std::mem::take(&mut self.job_id);
        let host_dir_created = self.host_dir_created;
        let host_dir = std::mem::take(&mut self.host_dir);
        let vm_index_allocator = self.vm_index_allocator.clone();
        let vm_index_opt = self.vm_index.take();

        // Captures for the runtime-down (no-spawn) fallback branch
        // below. The clones inside the spawn closure are separate
        // from these — the closure may run on another thread and
        // may run after this `drop` returns.
        let job_id_for_fallback = job_id.clone();
        let host_dir_for_fallback = host_dir.clone();
        let vm_index_allocator_for_fallback = vm_index_allocator.clone();

        // `compio::runtime::spawn` panics if there is no current
        // runtime (e.g., this Drop fires during process teardown
        // *after* the runtime has already stopped). Catch that so we
        // don't turn a teardown into an abort. The work is best-
        // effort by contract — `cleanup_orphans_at_startup` (or a
        // periodic prune) covers leaked Nomad jobs on next boot.
        let spawn_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            compio::runtime::spawn(async move {
                // Local equivalent of the runtime crate's
                // `panic_util::guard` (which is pub(crate) to
                // `zeroship-runtime` and not reachable from here
                // without taking a heavy crate-graph edge). Wraps
                // the body in `catch_unwind` so a panic inside the
                // detached task doesn't get swallowed silently.
                guard_detached("nomad_ch_create_guard_cleanup", async move {
                    // 1. Nomad purge — gates the vm_index release.
                    let purge_ok = if job_submitted {
                        let url =
                            format!("{nomad_addr}/v1/job/{job_id}?purge=true");
                        match http_delete_unsigned(&url, Duration::from_secs(10))
                            .await
                        {
                            Ok(r) if r.status == 200 || r.status == 404 => true,
                            Ok(r) => {
                                eprintln!(
                                    "[sandbox/nomad-ch] guard cleanup: purge \
                                     {job_id} non-2xx status={} body={} \
                                     (best-effort; vm_index will be leaked)",
                                    r.status,
                                    r.body.trim()
                                );
                                false
                            }
                            Err(e) => {
                                eprintln!(
                                    "[sandbox/nomad-ch] guard cleanup: purge \
                                     {job_id} failed: {e} (best-effort; \
                                     vm_index will be leaked)"
                                );
                                false
                            }
                        }
                    } else {
                        // Job was never submitted, so there's
                        // nothing on the Nomad side to race; the
                        // vm_index is safe to release immediately.
                        true
                    };

                    // 2. vm_index release — only on confirmed purge.
                    //    Same policy as the `stop` path (lines
                    //    644-649 of the file's stable doc-comment).
                    if purge_ok {
                        if let Some(i) = vm_index_opt {
                            vm_index_allocator
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .release(i);
                        }
                    } else if let Some(i) = vm_index_opt {
                        eprintln!(
                            "[sandbox/nomad-ch] guard cleanup: leaking \
                             vm_index={i} for job {job_id} (Nomad purge \
                             not confirmed; orphan-prune will reclaim on \
                             next boot)"
                        );
                    }

                    // 3. host_dir rm -rf — wrapped in spawn_blocking
                    //    because std::fs::remove_dir_all on an
                    //    active workspace tree (think 100k+
                    //    node_modules inodes) is uncomfortable on
                    //    the compio worker; at 50 concurrent
                    //    CreateGuard drops it would serialize
                    //    against every other compio task. Skip on
                    //    purge-failure for the same reason `stop`
                    //    skips: virtiofsd may still hold the share
                    //    open, and pulling the dir from under it
                    //    just produces confusing logs.
                    if purge_ok && host_dir_created {
                        let host_dir_clone = host_dir.clone();
                        let blocking = compio::runtime::spawn_blocking(move || {
                            if host_dir_clone.exists() {
                                std::fs::remove_dir_all(&host_dir_clone)
                                    .map_err(|e| {
                                        format!(
                                            "rm -rf {}: {e}",
                                            host_dir_clone.display()
                                        )
                                    })
                            } else {
                                Ok(())
                            }
                        })
                        .await;
                        match blocking {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => eprintln!(
                                "[sandbox/nomad-ch] guard cleanup: {e} \
                                 (best-effort)"
                            ),
                            Err(e) => eprintln!(
                                "[sandbox/nomad-ch] guard cleanup: \
                                 host_dir spawn_blocking panic: {e:?}"
                            ),
                        }
                    } else if !purge_ok && host_dir_created {
                        eprintln!(
                            "[sandbox/nomad-ch] guard cleanup: leaking \
                             host_dir {} (Nomad purge not confirmed)",
                            host_dir.display()
                        );
                    }
                })
                .await;
            })
            .detach();
        }));
        if spawn_res.is_err() {
            // Best-effort sync vm_index reclaim on the runtime-down
            // path. There's no Nomad call to gate against here —
            // the runtime is gone, the controller is shutting down,
            // there can't be a concurrent retry-`create` racing us.
            if let Some(i) = vm_index_opt {
                vm_index_allocator_for_fallback
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .release(i);
            }
            eprintln!(
                "[sandbox/nomad-ch] guard cleanup: compio::spawn failed (no \
                 current runtime?); job {job_id_for_fallback} and dir {} left \
                 for next-boot orphan prune",
                host_dir_for_fallback.display(),
            );
        }
    }
}

/// Local equivalent of `zeroship-runtime`'s `panic_util::guard` —
/// runs `fut` under `catch_unwind` so a panic inside a `.detach()`-ed
/// compio task gets a stderr log line instead of being silently
/// swallowed. The runtime crate's helper is `pub(crate)` to that
/// crate; rather than expose it cross-crate (which would force
/// `zeroship-sandbox` to take an edge on `zeroship-runtime` for one
/// helper) we keep a tiny local copy here.
async fn guard_detached<F, T>(site: &'static str, fut: F) -> Option<T>
where
    F: std::future::Future<Output = T>,
{
    use futures::FutureExt as _;
    use std::panic::AssertUnwindSafe;
    match AssertUnwindSafe(fut).catch_unwind().await {
        Ok(v) => Some(v),
        Err(p) => {
            let msg = p
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "<non-string panic>".to_string());
            eprintln!(
                "[sandbox/nomad-ch] panic in detached task ({site}): {msg}"
            );
            None
        }
    }
}

/// Map fractional `SandboxConfig.cpus` (e.g. 2.0, 1.5) to the
/// integer `boot=N` count Cloud Hypervisor needs at command-line.
/// Round up — cfg.cpus is the *target* allocation; never starve the
/// VM by rounding down a 1.5 to 1. Floor at 1 so a misconfigured
/// cpus=0 (or NaN) still produces a bootable VM (the validate at
/// config load rejects cpus≤0, but defense-in-depth).
fn cpus_boot(cpus: f32) -> u32 {
    if !cpus.is_finite() {
        return 1;
    }
    let n = cpus.ceil() as i64;
    if n < 1 { 1 } else { n as u32 }
}

/// Map `SandboxConfig.cpus` to the Nomad `Resources.CPU` advisory
/// (MHz). Same floor philosophy as [`cpus_boot`]: we never want to
/// emit `CPU=0` (rejected by some Nomad configs) when we're about
/// to boot a real VM. Floor at 500 MHz (≈ 0.25 vCPU). NaN /
/// negative → floor.
pub(crate) fn resources_cpu_mhz(cpus: f32) -> u32 {
    if !cpus.is_finite() {
        return 500;
    }
    let mhz = cpus * 2000.0;
    if mhz < 500.0 { 500 } else { mhz as u32 }
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
                        // Artifact directory holding kernel + rootfs.
                        // Renamed from ZSBX_HERE in round 3 (M4) —
                        // the previous name was meaningless on the
                        // bash side; this matches the Rust struct
                        // field `runtime_dir`'s intent.
                        "ZSBX_ARTIFACT_DIR": cfg.nomad_ch.runtime_dir.display().to_string(),
                        // ZSBX_RUNTIME is the per-allocation working
                        // dir Nomad provisions per task; the literal
                        // `${NOMAD_TASK_DIR}` here is a Nomad
                        // template variable that the agent expands
                        // before invoking the wrapper, NOT a bash
                        // expansion at our level. See
                        // https://developer.hashicorp.com/nomad/docs/runtime/environment
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
                        // M6: pair the second octet with the
                        // controller-side computation of `agent_url`.
                        // Both sides MUST read the same value so the
                        // tap/IP the wrapper provisions matches the IP
                        // the controller dials.
                        "ZSBX_SUBNET_BASE_OCTET":
                            cfg.nomad_ch.subnet_second_octet.to_string(),
                    },
                    "Resources": {
                        // CPU is in MHz units in the Nomad API.
                        // 1 vCPU ≈ 2000 MHz advisory; our
                        // SandboxConfig.cpus is fractional so
                        // multiply. Floor at 500 MHz (0.25 vCPU) for
                        // the same reason cpus_boot floors at 1: a
                        // misconfigured `cpus=0.0` (or a NaN slipping
                        // past validation) would otherwise produce
                        // CPU=0, which Nomad rejects on some configs
                        // and is anyway nonsensical when we're about
                        // to boot a VM with at least one vCPU.
                        "CPU": resources_cpu_mhz(cfg.cpus),
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
/// JSON parse errors **and HTTP transport errors** are tracked +
/// log-rate-limited (~once per 5s) and surfaced in the timeout
/// message. Without the HTTP-error track, a Nomad-unreachable
/// outage and an alloc-never-scheduled outage produce the same
/// "alloc never reached running ... last status=<no allocs>"
/// message — completely different triage paths collapsed into one
/// (C3 fix).
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
    let mut last_http_err: Option<String> = None;
    let mut last_http_log_at: Option<Instant> = None;
    while Instant::now() < deadline {
        let resp = http_get_unsigned(&url, Duration::from_secs(5)).await;
        match resp {
            Ok(r) if r.status == 200 => {
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
            Ok(r) => {
                // 200 is the only happy status; 4xx/5xx surface as
                // an HTTP error track too — operators need to see
                // 401/403 (auth misconfig) and 5xx separately from
                // "no allocs yet".
                let msg = format!(
                    "status {} body={}",
                    r.status,
                    r.body.trim()
                );
                let now = Instant::now();
                let stale = last_http_log_at
                    .map(|t| now.duration_since(t) > Duration::from_secs(5))
                    .unwrap_or(true);
                if stale {
                    eprintln!(
                        "[sandbox/nomad-ch] alloc poll: HTTP non-200 \
                         (will retry): {msg}"
                    );
                    last_http_log_at = Some(now);
                }
                last_http_err = Some(msg);
            }
            Err(e) => {
                // Transport-level failure (connection refused,
                // DNS, TLS, timeout). Track this distinctly so
                // the timeout message can say "Nomad unreachable"
                // rather than the misleading "alloc never reached
                // running, last status=<no allocs>".
                let now = Instant::now();
                let stale = last_http_log_at
                    .map(|t| now.duration_since(t) > Duration::from_secs(5))
                    .unwrap_or(true);
                if stale {
                    eprintln!(
                        "[sandbox/nomad-ch] alloc poll: HTTP transport \
                         error (will retry): {e}"
                    );
                    last_http_log_at = Some(now);
                }
                last_http_err = Some(e);
            }
        }
        compio::time::sleep(Duration::from_millis(250)).await;
    }
    let mut msg = format!(
        "nomad alloc never reached running for job {job_id} (last status={:?})",
        last_status.unwrap_or_else(|| "<no allocs>".to_string())
    );
    if let Some(e) = last_http_err {
        msg.push_str(&format!(
            "; last HTTP error (Nomad reachability): {e}"
        ));
    }
    if let Some(e) = last_parse_err {
        msg.push_str(&format!("; last parse error: {e}"));
    }
    Err(msg)
}

/// True when every alloc in the array has a terminal client status.
/// An empty / missing array is treated as terminal (no allocs to
/// wait on). Pulled out as a free helper for unit testing —
/// [`wait_for_job_gone`] is HTTP-bound and not unit-testable end to
/// end without a fake.
fn allocs_all_terminal(allocs: Option<&Vec<serde_json::Value>>) -> bool {
    let arr = match allocs {
        Some(a) => a,
        None => return true,
    };
    if arr.is_empty() {
        return true;
    }
    arr.iter().all(|a| {
        matches!(
            a["ClientStatus"].as_str().unwrap_or(""),
            "complete" | "failed" | "lost",
        )
    })
}

/// Block until the job's allocations are all in a terminal client
/// state (the wrapper script has exited → tap device + IP + virtiofsd
/// sockets released). Returns Ok on:
///
///   - `GET /v1/job/<id>` → 404 (Nomad GC removed the record), OR
///   - `GET /v1/job/<id>/allocations` → every alloc has
///     `ClientStatus ∈ {complete, failed, lost}` (or the array is
///     empty / 404).
///
/// We previously short-circuited on `Status == "dead" && Stop == true`
/// at the *job* level, but that races the wrapper-script teardown:
/// the job record is dead while individual alloc tasks (the
/// `raw_exec` wrapper, virtiofsd children) are still reaping. A
/// follow-up `create` reusing the released vm_index can collide on
/// the still-bound tap device. Polling the allocation client-status
/// instead catches the actual underlying-process termination.
///
/// Surfaces the Nomad status + any sustained JSON parse errors on
/// timeout so operators can investigate.
async fn wait_for_job_gone(
    nomad_addr: &str,
    job_id: &str,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let job_url = format!("{nomad_addr}/v1/job/{job_id}");
    let allocs_url = format!("{nomad_addr}/v1/job/{job_id}/allocations");
    let mut last_parse_err: Option<String> = None;
    let mut last_parse_log_at: Option<Instant> = None;
    let mut last_status: Option<String> = None;
    // C3: track HTTP transport / non-2xx errors distinctly so a
    // Nomad-unreachable outage doesn't masquerade as "alloc still
    // reaping" in the timeout message.
    let mut last_http_err: Option<String> = None;
    let mut last_http_log_at: Option<Instant> = None;

    fn note_parse_err(
        last_parse_err: &mut Option<String>,
        last_parse_log_at: &mut Option<Instant>,
        scope: &str,
        e: serde_json::Error,
    ) {
        let msg = format!("{e}");
        let now = Instant::now();
        let stale = last_parse_log_at
            .map(|t| now.duration_since(t) > Duration::from_secs(5))
            .unwrap_or(true);
        if stale {
            eprintln!(
                "[sandbox/nomad-ch] job-gone poll {scope}: JSON parse \
                 error (will retry): {msg}"
            );
            *last_parse_log_at = Some(now);
        }
        *last_parse_err = Some(msg);
    }

    fn note_http_err(
        last_http_err: &mut Option<String>,
        last_http_log_at: &mut Option<Instant>,
        scope: &str,
        msg: String,
    ) {
        let now = Instant::now();
        let stale = last_http_log_at
            .map(|t| now.duration_since(t) > Duration::from_secs(5))
            .unwrap_or(true);
        if stale {
            eprintln!(
                "[sandbox/nomad-ch] job-gone poll {scope}: HTTP error \
                 (will retry): {msg}"
            );
            *last_http_log_at = Some(now);
        }
        *last_http_err = Some(msg);
    }

    while Instant::now() < deadline {
        // First check if the job record is gone entirely.
        let job_resp = http_get_unsigned(&job_url, Duration::from_secs(5)).await;
        match &job_resp {
            Ok(r) if r.status == 404 => return Ok(()),
            Ok(r) if r.status == 200 => {
                // Job still present; fall through to alloc poll.
            }
            Ok(r) => note_http_err(
                &mut last_http_err,
                &mut last_http_log_at,
                "job",
                format!("status {} body={}", r.status, r.body.trim()),
            ),
            Err(e) => note_http_err(
                &mut last_http_err,
                &mut last_http_log_at,
                "job",
                e.clone(),
            ),
        }

        // Otherwise look at the allocations: a job in Status=dead
        // can still have allocations whose underlying processes are
        // mid-reap. We require every alloc be in a terminal client
        // state before declaring the job "gone".
        let allocs_resp =
            http_get_unsigned(&allocs_url, Duration::from_secs(5)).await;
        match allocs_resp {
            Ok(r) if r.status == 404 => return Ok(()),
            Ok(r) if r.status == 200 => {
                match serde_json::from_str::<serde_json::Value>(&r.body) {
                    Ok(v) => {
                        let arr = v.as_array();
                        if allocs_all_terminal(arr) {
                            return Ok(());
                        }
                        // Surface the latest non-terminal status for
                        // the timeout error message.
                        if let Some(a) = arr.and_then(|a| a.last()) {
                            last_status = a["ClientStatus"]
                                .as_str()
                                .map(str::to_string);
                        }
                    }
                    Err(e) => {
                        note_parse_err(
                            &mut last_parse_err,
                            &mut last_parse_log_at,
                            "allocations",
                            e,
                        );
                    }
                }
            }
            Ok(r) => note_http_err(
                &mut last_http_err,
                &mut last_http_log_at,
                "allocations",
                format!("status {} body={}", r.status, r.body.trim()),
            ),
            Err(e) => note_http_err(
                &mut last_http_err,
                &mut last_http_log_at,
                "allocations",
                e,
            ),
        }
        compio::time::sleep(Duration::from_millis(250)).await;
    }
    let mut msg = format!(
        "job {job_id} allocs did not reach terminal state within timeout \
         (last alloc client_status={:?})",
        last_status.unwrap_or_else(|| "<unknown>".to_string())
    );
    if let Some(e) = last_http_err {
        msg.push_str(&format!(
            "; last HTTP error (Nomad reachability): {e}"
        ));
    }
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
    // `send_ureq`'s error already carries the URL via the underlying
    // `ureq::Error: Display` impl; prefixing the method+url again
    // here just produced doubled-up
    // "POST http://...: connection refused: POST http://...:" log
    // lines. Propagate `send_ureq` directly.
    send_ureq(req, body)
}

/// Poll `/livez` until 200 AND the agent's `/version` reports the
/// **expected pubkey fingerprint**, or the deadline expires.
///
/// **Why both checks?** During N=8 rapid-recycle stress testing the
/// host-side process tree (cloud-hypervisor + 3× virtiofsd + the
/// bash wrapper + the tap binding) was observed to lag Nomad's view
/// of alloc-terminal by 0.5–2 s. A fresh `create()` for the same VM
/// index could land while a *previous* tenant's agent was still
/// answering `/livez=200` on the same IP — the controller would
/// return 201 in 0.25 s (vs the ~6 s healthy baseline), then every
/// subsequent `/exec` would 502 with "No route to host" once the old
/// VM finally died.
///
/// The fingerprint is the kernel of "is this OUR agent?" — the
/// controller mints a fresh Ed25519 keypair per sandbox; the agent
/// publishes the pubkey-SHA256[..8] under `pubkey_fingerprint` on
/// `/version`. A stale-tenant agent has a *different* fingerprint
/// (different keypair → different pubkey → different hash), so we
/// keep polling until either:
///   1. `/version.pubkey_fingerprint` matches `expected_fp` → ready, OR
///   2. Deadline expires → "stale agent at <ip>: expected <fp>, got <fp>"
///      — operator gets actionable text instead of a silent racy 201.
///
/// The `/version` endpoint is **auth-gated**, so we sign the probe
/// with the controller-side `signing_key` we just minted. That
/// reinforces the same-tenant guarantee: an agent that doesn't have
/// our pubkey at `/run/keys/controller-pubkey` 401s the probe; we
/// retry until either the right agent comes up or we time out.
///
/// **Backward compatibility:** older agents (pre-`pubkey_fingerprint`
/// in `/version`) will return JSON without the field. We treat a
/// missing/empty fingerprint as "this agent is too old to attest";
/// emit a one-shot warning and fall back to /livez-only behaviour.
/// Removing this fallback once the agent fleet is fully upgraded is
/// a one-line change.
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
        // 1. Cheap unsigned /livez probe — gates the more expensive
        //    signed /version call. An agent that's not yet listening
        //    won't even answer /livez, so we save a sign+RPC round
        //    on every poll where the agent simply hasn't booted yet.
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
            // 2. Agent is answering. Now confirm it's OUR agent by
            //    asking /version for its pubkey fingerprint.
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
                            Some(fp) if fp == expected_fp => {
                                return Ok(());
                            }
                            Some(fp) => {
                                last_fp = Some(fp);
                                // Stale tenant. Keep polling — either
                                // it dies and our agent comes up, or
                                // the deadline expires and we surface
                                // the mismatch.
                            }
                            None => {
                                // Backward-compat: legacy agent without
                                // pubkey_fingerprint in /version. The
                                // /version call already passed our
                                // signed-auth check, so the agent IS
                                // verifying with our pubkey → it's
                                // ours. Warn and accept.
                                eprintln!(
                                    "[sandbox/nomad-ch] wait_for_agent: legacy agent at \
                                     {base_url} returned no pubkey_fingerprint on /version; \
                                     falling back to signed-auth-only attestation \
                                     (upgrade the agent to close the stale-tenant race \
                                     on /livez=200 before /version is signed-auth gated)"
                                );
                                return Ok(());
                            }
                        }
                    }
                    // 401 means a stale-tenant agent that's verifying
                    // a *different* pubkey. Keep polling; same
                    // rationale as the fp-mismatch branch.
                }
                Err(_e) => {
                    // Transport error on /version — agent just came
                    // up answering /livez but isn't fully ready, or
                    // the connection raced a tear-down. Retry.
                }
            }
        }
        // 150 ms livez poll cadence — matches k8s.rs.
        compio::time::sleep(Duration::from_millis(150)).await;
    }
    // Timeout. Distinguish:
    //   - never saw /livez=200 → "agent at <url> never returned 200"
    //   - saw /livez=200 but fingerprint mismatched → stale-tenant
    //   - saw /livez=200 but /version 401'd → wrong-pubkey agent
    if let Some(actual_fp) = last_fp {
        Err(format!(
            "stale agent at {base_url}: expected pubkey_fingerprint={expected_fp}, \
             got {actual_fp}; previous tenant's wrapper still owns the IP"
        ))
    } else if last_version_status == Some(401) {
        Err(format!(
            "stale agent at {base_url}: /version returned 401 (agent is verifying with a \
             different controller pubkey); expected fp={expected_fp}"
        ))
    } else {
        Err(format!("agent at {base_url} never returned 200 on /livez (expected fp={expected_fp})"))
    }
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

/// Wall-clock seconds since UNIX_EPOCH.
///
/// Crash-loud on clock-before-epoch instead of silently falling back
/// to 0 — a `ts=0` would put every signed RPC's timestamp 56 years
/// in the past, the agent's 5-second skew window would 401
/// permanently, the idle reaper would believe every sandbox was
/// born at the dawn of UNIX and cull them all, and operators would
/// be staring at a UI that says "your sandbox was last used 56
/// years ago." The right behaviour for a clock that's gone backwards
/// to before 1970 is to panic and let the orchestrator surface the
/// problem; the silent-zero fallback hides catastrophic state.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX_EPOCH")
        .as_secs()
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
    // Defense-in-depth: the empty check above already guarantees
    // chars.next() is Some, but using `?` propagates the empty-id
    // error cleanly if a future refactor moves the length check
    // around. Cheaper than `unwrap()` to reason about.
    let first = chars
        .next()
        .ok_or_else(|| format!("{what} unexpectedly empty"))?;
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

    #[test]
    fn vm_alloc_single_element_at_boundaries() {
        // M6: pool of size 1 at the floor (1,1) and ceil (155,155)
        // boundaries — the same edge case at both ends of the
        // controller-validated index range.
        for boundary in [1u16, 155u16] {
            let mut a = VmIndexAllocator::new(boundary, boundary);
            assert_eq!(
                a.alloc().unwrap(),
                boundary,
                "boundary={boundary} first alloc"
            );
            assert!(
                a.alloc().is_err(),
                "boundary={boundary} second alloc must fail"
            );
            a.release(boundary);
            assert_eq!(
                a.alloc().unwrap(),
                boundary,
                "boundary={boundary} reuse after release"
            );
        }
    }

    // ─── CreateGuard cleanup ordering (C1) ──────────────────
    //
    // The fix for C1 moves vm_index release into the detached
    // cleanup task and gates it on Nomad-purge confirmation. We
    // can exercise the no-Nomad-call branch (job_submitted=false ⇒
    // purge_ok=true ⇒ release fires) end-to-end inside a compio
    // runtime — which proves the index makes it back to the pool
    // after Drop runs the detached task.
    #[compio::test]
    async fn create_guard_releases_vm_index_when_no_job_submitted() {
        // Pool of a single index — easiest way to detect leak
        // (next alloc would fail) vs. correct release (next alloc
        // succeeds). Pre-allocate so the pool is empty at the
        // start of the test, then assert the detached cleanup
        // refills it.
        let pool = Arc::new(Mutex::new(VmIndexAllocator::new(7, 7)));
        let allocated =
            pool.lock().unwrap().alloc().expect("first alloc");
        assert_eq!(allocated, 7);
        // Pool is now empty — alloc would fail.
        assert!(pool.lock().unwrap().alloc().is_err());

        {
            let mut g = CreateGuard::new(
                pool.clone(),
                "http://127.0.0.1:1".to_string(), // unreachable
                "zsbx-test-no-purge".to_string(),
                PathBuf::from("/tmp/zsbx-c1-test"),
            );
            g.vm_index = Some(allocated);
            g.job_submitted = false; // skip http_delete entirely
            g.host_dir_created = false; // skip rm -rf entirely
            // Drop here triggers the detached cleanup task.
        }
        // The detached task may not have run yet — yield until
        // the index reappears in the pool. Bound by a generous
        // timeout so a regression of "release happens, but on the
        // wrong path" still fails the test rather than hanging.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut released = false;
        while Instant::now() < deadline {
            // Try to alloc; if we get the index back, release was
            // performed by the detached cleanup.
            if let Ok(i) = pool.lock().unwrap().alloc() {
                assert_eq!(i, 7);
                released = true;
                break;
            }
            compio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            released,
            "CreateGuard did not release vm_index back to the pool — \
             C1 regression: a retry-create() for the same user would \
             see a phantom-exhausted pool until orphan-prune"
        );
    }

    #[compio::test]
    async fn create_guard_leaks_vm_index_on_purge_failure() {
        // Pool of a single index. Job submitted but Nomad addr is
        // a guaranteed-unroutable port — the http_delete will fail
        // within the per-call 10s timeout. We assert the index is
        // NOT released within the first ~250ms (the detached task
        // is still mid-http_delete). C1 policy: leak on purge
        // failure rather than risk a tap collision on the retry
        // path. Orphan-prune at next boot reclaims it indirectly.
        //
        // We don't wait for the full failure path — the
        // observable distinction from the no-purge branch is
        // exactly that the index does NOT come back immediately.
        let pool = Arc::new(Mutex::new(VmIndexAllocator::new(13, 13)));
        let allocated =
            pool.lock().unwrap().alloc().expect("first alloc");
        assert_eq!(allocated, 13);
        assert!(pool.lock().unwrap().alloc().is_err());

        {
            let mut g = CreateGuard::new(
                pool.clone(),
                // Non-routable: port 1, no listener.
                "http://127.0.0.1:1".to_string(),
                "zsbx-test-leak".to_string(),
                PathBuf::from("/tmp/zsbx-c1-leak"),
            );
            g.vm_index = Some(allocated);
            g.job_submitted = true;
            g.host_dir_created = false;
            // Drop fires; the detached cleanup will spend up to
            // 10s on the http_delete before deciding to leak.
        }
        // Yield once so the detached task actually starts running.
        compio::time::sleep(Duration::from_millis(50)).await;
        // The cleanup task is mid-http_delete with a 10s timeout
        // → the index is still allocated (pool empty). That's the
        // C1 invariant: don't release until the Nomad side
        // confirms the job is gone.
        let immediate = pool.lock().unwrap().alloc();
        assert!(
            immediate.is_err(),
            "CreateGuard released vm_index BEFORE the Nomad purge \
             completed — that's the exact race C1 is guarding against"
        );
    }

    #[test]
    fn vm_alloc_release_is_idempotent() {
        // M8: releasing an index that's already in `freed` should
        // not corrupt state — the same index must NOT be handed out
        // twice on the next two allocs.
        let mut a = VmIndexAllocator::new(1, 5);
        let i = a.alloc().unwrap();
        a.release(i);
        a.release(i); // double-release: BTreeSet dedups, no panic
        a.release(i); // triple, for good measure
        let j1 = a.alloc().unwrap();
        let j2 = a.alloc().unwrap();
        assert_eq!(j1, i, "first realloc reuses the freed index");
        assert_ne!(
            j2, j1,
            "second alloc must NOT hand back the same index — \
             double-release must not double-insert"
        );
    }

    // ─── wait_for_job_gone alloc-terminal predicate (I2) ─────

    fn alloc(status: &str) -> serde_json::Value {
        serde_json::json!({"ClientStatus": status})
    }

    #[test]
    fn allocs_terminal_empty_or_none_is_ok() {
        assert!(allocs_all_terminal(None));
        let empty: Vec<serde_json::Value> = Vec::new();
        assert!(allocs_all_terminal(Some(&empty)));
    }

    #[test]
    fn allocs_terminal_all_terminal_states_pass() {
        let all = vec![alloc("complete"), alloc("failed"), alloc("lost")];
        assert!(allocs_all_terminal(Some(&all)));
    }

    #[test]
    fn allocs_terminal_running_blocks() {
        let mixed = vec![alloc("complete"), alloc("running")];
        assert!(!allocs_all_terminal(Some(&mixed)));
    }

    #[test]
    fn allocs_terminal_pending_blocks() {
        let v = vec![alloc("pending")];
        assert!(!allocs_all_terminal(Some(&v)));
    }

    #[test]
    fn allocs_terminal_unknown_status_blocks() {
        // Defensive: an unrecognised string is not treated as
        // terminal (avoids races on future Nomad alloc-status
        // additions).
        let v = vec![alloc("future-status-we-dont-know")];
        assert!(!allocs_all_terminal(Some(&v)));
    }

    // ─── C3: HTTP error tracked in poll-loop timeout messages ──
    //
    // Without these the timeout message lies: a Nomad-unreachable
    // outage produces the same "alloc never reached running …
    // last status=<no allocs>" message as a real scheduling
    // problem, collapsing two completely different triage paths
    // into one. The fix tracks `last_http_err` distinctly and
    // appends it to the timeout text.

    #[compio::test]
    async fn wait_for_alloc_running_surfaces_unreachability() {
        // 127.0.0.1:1 is reserved/unbound on standard hosts → ureq
        // returns a transport error within the per-call 5s budget.
        // Use a tiny outer timeout so the test finishes fast.
        let err = wait_for_alloc_running(
            "http://127.0.0.1:1",
            "zsbx-c3-test",
            Duration::from_millis(400),
        )
        .await
        .expect_err("must time out");
        // The error MUST mention reachability so an operator
        // doesn't go hunting for an alloc-scheduling bug when the
        // real problem is that Nomad is down.
        assert!(
            err.contains("reachability") || err.contains("HTTP error"),
            "C3 regression: timeout error did not surface HTTP \
             reachability hint; got {err:?}"
        );
    }

    #[compio::test]
    async fn wait_for_job_gone_surfaces_unreachability() {
        let err = wait_for_job_gone(
            "http://127.0.0.1:1",
            "zsbx-c3-jg-test",
            Duration::from_millis(400),
        )
        .await
        .expect_err("must time out");
        assert!(
            err.contains("reachability") || err.contains("HTTP error"),
            "C3 regression: wait_for_job_gone timeout error did not \
             surface HTTP reachability hint; got {err:?}"
        );
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
        let cfg = SandboxConfig {
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
                subnet_second_octet: 99,
            },
        };
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
    fn cpus_boot_handles_nan_and_inf() {
        // M7: NaN / Inf must not produce 0 or saturating-cast garbage.
        assert_eq!(cpus_boot(f32::NAN), 1);
        assert_eq!(cpus_boot(f32::INFINITY), 1);
        assert_eq!(cpus_boot(f32::NEG_INFINITY), 1);
    }

    #[test]
    fn resources_cpu_mhz_floors_at_500() {
        // I3: Resources.CPU floor matches cpus_boot's floor — no 0.
        assert_eq!(resources_cpu_mhz(0.0), 500);
        assert_eq!(resources_cpu_mhz(0.1), 500); // 200 MHz < 500 floor
        assert_eq!(resources_cpu_mhz(0.25), 500);
        assert_eq!(resources_cpu_mhz(0.5), 1000);
        assert_eq!(resources_cpu_mhz(2.0), 4000);
        assert_eq!(resources_cpu_mhz(-1.0), 500);
        assert_eq!(resources_cpu_mhz(f32::NAN), 500);
        assert_eq!(resources_cpu_mhz(f32::INFINITY), 500);
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
        // M4: ZSBX_HERE renamed to ZSBX_ARTIFACT_DIR; verify the
        // new name is in the env block AND the legacy name is NOT
        // (so a future ad-hoc deploy that references the old name
        // fails fast instead of silently picking up nothing).
        assert_eq!(
            task["Env"]["ZSBX_ARTIFACT_DIR"],
            "/var/lib/zeroship/ch"
        );
        assert!(
            task["Env"]["ZSBX_HERE"].is_null(),
            "ZSBX_HERE should be gone (renamed to ZSBX_ARTIFACT_DIR)"
        );
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
        // M6: subnet base octet is paired between Rust and bash.
        // Default 99 keeps the historical 10.99/16 layout.
        assert_eq!(task["Env"]["ZSBX_SUBNET_BASE_OCTET"], "99");
        // 2.0 vCPU advisory → 4000 MHz.
        assert_eq!(task["Resources"]["CPU"], 4000);
        assert_eq!(task["Resources"]["MemoryMB"], 1024);
        // KillTimeout is 10 seconds in nanoseconds.
        assert_eq!(task["KillTimeout"], 10_000_000_000u64);
    }

    #[test]
    fn nomad_job_json_uses_configured_subnet_base_octet() {
        // M6: changing the config's subnet_second_octet must flow
        // into the env var the wrapper reads. The unit test for the
        // controller-side IP computation lives in
        // create_guard_uses_subnet_octet (further down) — these
        // two together pin the pairing.
        let mut cfg = make_cfg();
        cfg.nomad_ch.subnet_second_octet = 50;
        let v = build_nomad_job_json(
            "zsbx-y", &cfg, 1,
            Path::new("/k"), Path::new("/w"), Path::new("/u"),
            "u", "p", "s",
        );
        assert_eq!(
            v["Job"]["TaskGroups"][0]["Tasks"][0]["Env"]["ZSBX_SUBNET_BASE_OCTET"],
            "50"
        );
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

    // ─── FM-A: stale-tenant fingerprint check in wait_for_agent_livez ────
    //
    // Stress finding (N=8 rapid recycle): a fresh `create()` for the
    // same vm_index / VM IP returned 201 in 0.25 s while pointing at
    // the *previous* tenant's still-alive agent — Nomad's view said
    // alloc=terminal, but the host-side cloud-hypervisor process tree
    // hadn't reaped yet. /livez=200 was the wrong attestation; the
    // controller needs to verify the agent is signing with OUR
    // pubkey before declaring the sandbox ready.
    //
    // The mock here is a stdlib TcpListener that speaks just enough
    // HTTP to mimic the agent's /livez and /version handlers. It
    // deliberately doesn't verify the request signature (we're not
    // testing the agent; we're testing the controller's behaviour
    // when the agent reports a particular fingerprint).

    /// Spin up a tiny `std::net::TcpListener`-backed mock that
    /// answers /livez=200 and /version=`version_body` (raw bytes,
    /// caller controls the JSON).
    ///
    /// Returns the bound port + a shutdown flag. The thread exits
    /// when the flag is flipped (caller does this in Drop, or the
    /// test ends and we leak the thread — fine for unit tests).
    fn spawn_mock_agent(
        version_body: String,
        version_status: u16,
    ) -> (u16, std::sync::Arc<std::sync::atomic::AtomicBool>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::atomic::{AtomicBool, Ordering};
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let port = listener.local_addr().expect("addr").port();
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        std::thread::spawn(move || {
            while !stop2.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_millis(200)))
                            .ok();
                        stream
                            .set_write_timeout(Some(Duration::from_millis(200)))
                            .ok();
                        // Read the request line + headers (don't bother
                        // with the body; we only switch on path).
                        let mut buf = [0u8; 1024];
                        let n = stream.read(&mut buf).unwrap_or(0);
                        let req = String::from_utf8_lossy(&buf[..n]);
                        let path = req
                            .lines()
                            .next()
                            .and_then(|l| l.split_whitespace().nth(1))
                            .unwrap_or("");
                        let resp = if path == "/livez" {
                            "HTTP/1.1 200 OK\r\nContent-Length: 16\r\n\r\n{\"status\":\"ok\"}"
                                .to_string()
                        } else if path == "/version" {
                            let status_text = match version_status {
                                200 => "200 OK",
                                401 => "401 Unauthorized",
                                _ => "500 Internal Server Error",
                            };
                            format!(
                                "HTTP/1.1 {}\r\nContent-Type: application/json\r\n\
                                 Content-Length: {}\r\n\r\n{}",
                                status_text,
                                version_body.len(),
                                version_body,
                            )
                        } else {
                            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_string()
                        };
                        let _ = stream.write_all(resp.as_bytes());
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        (port, stop)
    }

    fn make_sk() -> Arc<SigningKey> {
        let mut bytes = [0u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = i as u8;
        }
        Arc::new(SigningKey::from_bytes(&bytes))
    }

    #[compio::test]
    async fn wait_for_agent_livez_returns_ok_when_fp_matches() {
        let sk = make_sk();
        let our_fp = sig::pubkey_fingerprint(&sk.verifying_key());
        let body = format!(
            r#"{{"agent_version":"x","pubkey_fingerprint":"{our_fp}"}}"#
        );
        let (port, stop) = spawn_mock_agent(body, 200);
        let url = format!("http://127.0.0.1:{port}");
        let res = wait_for_agent_livez(
            &url,
            &our_fp,
            &sk,
            Duration::from_secs(2),
        )
        .await;
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(res.is_ok(), "expected Ok on fp match, got {res:?}");
    }

    #[compio::test]
    async fn wait_for_agent_livez_times_out_on_fp_mismatch() {
        let sk = make_sk();
        let our_fp = sig::pubkey_fingerprint(&sk.verifying_key());
        // Mock returns a DIFFERENT fingerprint — simulates stale
        // tenant whose CH is still alive on the same IP.
        let stale_fp = "deadbeef00112233";
        assert_ne!(our_fp, stale_fp);
        let body = format!(
            r#"{{"agent_version":"x","pubkey_fingerprint":"{stale_fp}"}}"#
        );
        let (port, stop) = spawn_mock_agent(body, 200);
        let url = format!("http://127.0.0.1:{port}");
        // Short timeout — we want to verify the function gives up
        // and surfaces the "stale agent" error, not just hangs.
        let res = wait_for_agent_livez(
            &url,
            &our_fp,
            &sk,
            Duration::from_millis(800),
        )
        .await;
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let err = res.expect_err("must surface stale-agent error");
        assert!(
            err.contains("stale agent"),
            "FM-A regression: error did not surface stale-agent text; got {err:?}"
        );
        assert!(
            err.contains(stale_fp) && err.contains(&our_fp),
            "error must include both expected and actual fp for triage; got {err:?}"
        );
    }

    #[compio::test]
    async fn wait_for_agent_livez_legacy_agent_no_fp_field_accepted() {
        // Backward-compat: a /version response without the
        // pubkey_fingerprint field falls back to "signed-auth-only"
        // attestation. The /version request was signed with the
        // controller's key; if the agent answered 200 it's verifying
        // with our pubkey (i.e. it IS our agent). This branch keeps
        // a gradual rollout path open — old agents still work.
        let sk = make_sk();
        let our_fp = sig::pubkey_fingerprint(&sk.verifying_key());
        let body = r#"{"agent_version":"legacy"}"#.to_string();
        let (port, stop) = spawn_mock_agent(body, 200);
        let url = format!("http://127.0.0.1:{port}");
        let res = wait_for_agent_livez(
            &url,
            &our_fp,
            &sk,
            Duration::from_secs(2),
        )
        .await;
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(
            res.is_ok(),
            "legacy /version (no fp field) must fall back to ok; got {res:?}"
        );
    }

    #[compio::test]
    async fn wait_for_agent_livez_times_out_when_unreachable() {
        // No mock — point at an unbound port. /livez never returns
        // 200 → timeout path with "never returned 200" message.
        let sk = make_sk();
        let our_fp = sig::pubkey_fingerprint(&sk.verifying_key());
        let res = wait_for_agent_livez(
            "http://127.0.0.1:1",
            &our_fp,
            &sk,
            Duration::from_millis(400),
        )
        .await;
        let err = res.expect_err("must time out");
        assert!(
            err.contains("never returned 200"),
            "expected /livez-unreachable timeout text; got {err:?}"
        );
        assert!(
            err.contains(&our_fp),
            "timeout error must include expected fp for triage; got {err:?}"
        );
    }

    #[compio::test]
    async fn wait_for_agent_livez_times_out_on_persistent_401() {
        // /livez=200 but /version=401 — agent is verifying with a
        // *different* pubkey (stale tenant whose key file at
        // /run/keys/controller-pubkey predates our create()).
        // Polling never resolves; deadline expires.
        let sk = make_sk();
        let our_fp = sig::pubkey_fingerprint(&sk.verifying_key());
        let (port, stop) = spawn_mock_agent("{\"error\":\"unauthorized\"}".to_string(), 401);
        let url = format!("http://127.0.0.1:{port}");
        let res = wait_for_agent_livez(
            &url,
            &our_fp,
            &sk,
            Duration::from_millis(600),
        )
        .await;
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let err = res.expect_err("must time out");
        assert!(
            err.contains("401") || err.contains("different controller pubkey"),
            "FM-A regression: persistent-401 timeout did not surface \
             'verifying with a different controller pubkey'; got {err:?}"
        );
    }
}
