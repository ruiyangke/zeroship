//! Sandbox process configuration. Read once at startup from env / argv;
//! the resulting struct is cloned into the shared `AppState`.

use std::path::PathBuf;

use zeroize::Zeroizing;

/// Wrapper around the bearer token string. Two properties:
///   1. **`Debug`** prints `<redacted, len=N>` instead of the bytes,
///      so any panic backtrace / debug log / error chain that
///      formats `SandboxConfig` with `{:?}` doesn't dump the token
///      to stderr.
///   2. **`Zeroizing`** scrubs the heap allocation on drop, so a
///      core dump or `/proc/<pid>/mem` read after process exit
///      doesn't trivially recover the token. (Live-process memory
///      reads are still a concern, but at least the post-mortem
///      surface is closed.)
#[derive(Clone)]
pub struct ApiToken(Zeroizing<String>);

impl ApiToken {
    pub fn new<S: Into<String>>(s: S) -> Self {
        Self(Zeroizing::new(s.into()))
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl std::fmt::Debug for ApiToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.is_empty() {
            f.write_str("<unset>")
        } else {
            write!(f, "<redacted, len={}>", self.0.len())
        }
    }
}

#[derive(Clone, Debug)]
pub struct SandboxConfig {
    /// Port the HTTP API listens on. `SANDBOX_PORT` (default 9091).
    pub port: u16,

    /// Bearer token for the HTTP API. `SANDBOX_TOKEN`. Empty value
    /// disables auth and is **rejected at startup** unless the
    /// operator also set `SANDBOX_ALLOW_NO_AUTH=true` (dev opt-in).
    /// Production deployments without a token simply refuse to
    /// boot, so an unconfigured pod can't accidentally become a
    /// public RCE.
    ///
    /// Wrapped in [`ApiToken`] so debug output is redacted and the
    /// heap allocation is zeroed on drop.
    ///
    /// A7 (deferred): restricted to `pub(crate)` because this is the
    /// creator-side bearer that authenticates every `/sandbox/*`
    /// request (see `crates/sandbox/src/auth.rs`). A
    /// `cfg.token = ApiToken::new("known")` swap from out-of-crate
    /// code could plant an attacker-known token. Out-of-crate
    /// callers (notably integration tests in
    /// `crates/sandbox/tests/`) use [`SandboxConfig::new_fixture`]
    /// + [`SandboxConfig::with_token`] to set the field; direct
    /// struct-literal construction is blocked by the visibility.
    pub(crate) token: ApiToken,

    /// Backend selector. `SANDBOX_BACKEND=docker|k8s` (default docker).
    pub backend: String,

    /// Docker image tag spawned for new sessions. `SANDBOX_IMAGE`
    /// (default `zeroship/sandbox-base:latest`).
    /// Used by the **docker** backend.
    pub image: String,

    /// Host directory where per-project workspaces live. Each session's
    /// `/workspace` is bind-mounted from `{workspace_root}/{project_id}/`.
    /// `SANDBOX_WORKSPACE_ROOT` (default `/var/zeroship/projects`).
    /// Used by the **docker** backend only — k8s sessions store their
    /// workspace inside the Pod via emptyDir.
    pub workspace_root: PathBuf,

    /// Docker network the sandbox containers join. `SANDBOX_NETWORK`
    /// (default `zeroship-sandbox-net`). Must be created out-of-band
    /// (`docker network create zeroship-sandbox-net`).
    /// Used by the **docker** backend only.
    pub network: String,

    /// Per-container memory limit in MiB. `SANDBOX_MEMORY_MB` (default
    /// 1024).
    pub memory_mb: u32,

    /// Per-container CPU quota. `SANDBOX_CPUS` (default 2.0).
    pub cpus: f32,

    /// Idle session GC threshold. `SANDBOX_IDLE_TIMEOUT_SECS`
    /// (default 1800 — 30 min).
    pub idle_timeout_secs: u64,

    /// Hard ceiling regardless of activity. `SANDBOX_MAX_LIFETIME_SECS`
    /// (default 28800 — 8h).
    pub max_lifetime_secs: u64,

    /// Pull the image at startup if missing. `SANDBOX_AUTO_PULL`
    /// (default false — admins should pre-pull for predictable boot).
    /// Docker backend only.
    pub auto_pull: bool,

    /// K8s-backend settings. Read from env even when `backend=docker`
    /// (cheap; lets you switch backends without restart-time config
    /// gymnastics).
    pub k8s: K8sConfig,

    /// Nomad + Cloud Hypervisor backend settings. Same loose-loading
    /// rationale as `k8s`: parsed unconditionally so the operator can
    /// flip `SANDBOX_BACKEND=nomad-ch` without re-templating env.
    pub nomad_ch: NomadCHConfig,

    /// FM-E: how many extra `backend.create()` attempts the create
    /// handler will make on a stale-tenant or `/livez`-timeout error.
    /// 0 disables retry; default 2 (= up to 3 attempts total). With
    /// FM-F's host-fence in place a stale-tenant error should be
    /// rare — the retry is one layer of defense-in-depth for a truly
    /// broken host. `SANDBOX_CREATE_RETRY_MAX` (default 2).
    pub create_retry_max: u32,

    /// FM-E: hard wall-time budget for the entire create + retry
    /// chain. A pathologically broken backend that consumes the full
    /// 30 s `agent_livez_timeout_secs` per attempt × `retry_max+1`
    /// would otherwise tie up an ntex worker for ~90 s+; this caps
    /// it. Past this budget the handler bails with 503 even if
    /// retries remain. `SANDBOX_CREATE_RETRY_TOTAL_TIMEOUT_SECS`
    /// (default 90).
    pub create_retry_total_timeout_secs: u64,

    /// Snapshot/restore master feature flag. When `false`, the
    /// snapshot/wake/cold-boot admin handlers return 501
    /// `feature_disabled` and the idle-eviction sweep is a no-op.
    /// Lease-takeover for transient states (§ 6.1) stays on
    /// regardless — we always need to unwedge stuck rows.
    /// `SANDBOX_SNAPSHOT_ENABLED` (default `false`).
    /// Source-of-truth: docs/proposals/sandbox-snapshot-restore.md
    /// § 10.3, § 13.1.
    pub snapshot_enabled: bool,

    /// L1 root directory for snapshot artifacts. Each sandbox's
    /// artifact lives at `<root>/<sandbox-id>/{config,state,memory}.<ext>`.
    /// `SANDBOX_SNAPSHOT_L1_ROOT` (default `/var/zeroship/ch/snapshots`).
    pub snapshot_l1_root: PathBuf,

    /// When `true`, the controller wraps `LocalDiskSnapshotStore`
    /// in `TieredSnapshotStore<L1=disk, L2=GCS>` so put writes
    /// fire-and-forget to GCS in addition to L1, and L1-miss reads
    /// fall back to GCS. Requires `snapshot_gcs_bucket` to be set.
    /// `SANDBOX_SNAPSHOT_USE_GCS` (default `false`).
    pub snapshot_use_gcs: bool,

    /// GCS bucket name (no `gs://` prefix). Required when
    /// `snapshot_use_gcs = true`; ignored otherwise.
    /// `SANDBOX_SNAPSHOT_GCS_BUCKET` (default `None`).
    pub snapshot_gcs_bucket: Option<String>,

    /// Path to the root KEK (key-encryption key) file used by the
    /// AEAD wrap layer (snapshot_aead.rs). When `None`, the wrap
    /// layer runs in passthrough mode — snapshots are not
    /// encrypted at rest. Production deployments set this to a
    /// 0o400 file containing the 32-byte key.
    /// `SANDBOX_SNAPSHOT_ROOT_KEK_PATH` (default `None`).
    pub snapshot_root_kek_path: Option<PathBuf>,

    /// Size of the per-sandbox `workspace.img` AND per-user
    /// `home.img` virtio-blk images, in gigabytes. Set at create()
    /// time via `truncate -s <N>G` so the on-disk file is sparse —
    /// actual host bytes used grow with what the guest writes, not
    /// the declared size. The same value caps both images for the
    /// virtio-blk pivot (bug #11); we don't need separate workspace
    /// and home ceilings because they share the same FS layer and
    /// the host_state_dir overall is bounded by operator-level
    /// quota anyway. `SANDBOX_WORKSPACE_IMAGE_SIZE_GB` (default 20).
    /// Source-of-truth: `docs/proposals/sandbox-snapshot-restore.md`
    /// virtio-blk pivot.
    pub workspace_image_size_gb: u32,
}

#[derive(Clone, Debug)]
pub struct K8sConfig {
    /// Namespace where Pods are created. `SANDBOX_K8S_NAMESPACE`
    /// (default `default`).
    pub namespace: String,

    /// Agent OCI image, including tag (or pinned digest in prod).
    /// `SANDBOX_K8S_IMAGE` (default `docker.io/zeroship/sandbox-agent:dev`).
    pub image: String,

    /// `runtimeClassName` to apply to the Pod. Must point at the
    /// crun+libkrun handler. `SANDBOX_K8S_RUNTIME_CLASS` (default
    /// `kvm-sandbox`).
    pub runtime_class: String,

    /// `kubectl wait --for=condition=Ready` timeout in seconds.
    /// `SANDBOX_K8S_READY_TIMEOUT_SECS` (default 120).
    pub ready_timeout_secs: u64,

    /// Use `kubectl port-forward` per session instead of dialing the
    /// Pod IP directly. Required when the controller runs outside
    /// the cluster (typical for local dev). In-cluster controllers
    /// should set this to false. `SANDBOX_K8S_USE_PORT_FORWARD`
    /// (default true — safe for local dev; switch off in cluster).
    pub use_port_forward: bool,

    /// Loopback port allocator base when `use_port_forward=true`.
    /// `SANDBOX_K8S_PORT_FORWARD_START` (default 18000). Allocator
    /// is monotonic per process; never reuses ports.
    pub port_forward_start: u16,

    /// PVC size for the per-user `/home/u` mount. Holds package
    /// caches (pnpm, npm, pip, cargo), dotfiles, ssh config —
    /// non-secret, deduplicated state that survives across every
    /// sandbox the user opens. `SANDBOX_K8S_USER_HOME_SIZE`
    /// (default `5Gi`). k8s parses this as a Quantity.
    pub user_home_size: String,

    /// StorageClass used for per-user PVCs. Empty / unset = use
    /// the cluster default (`storageclass.kubernetes.io/is-default-class: "true"`).
    /// In production pick one with snapshot+clone support
    /// (Longhorn, Ceph RBD, EBS gp3, GCE PD) so the per-project
    /// snapshot / fork features in the storage design are
    /// buildable later. `SANDBOX_K8S_USER_HOME_STORAGE_CLASS`
    /// (default empty).
    pub user_home_storage_class: Option<String>,

    /// On controller startup, delete every Pod + ConfigMap labeled
    /// `app.kubernetes.io/name=sandbox-agent` in the namespace.
    /// Useful for single-replica deployments to clean up after a
    /// crash (per-sandbox signing keys live only in process
    /// memory; orphan Pods would 401 every signed request from the
    /// new controller forever).
    ///
    /// **Disable in HA / multi-replica deployments.** With more
    /// than one controller replica running, the first one to come
    /// up after a deploy nukes every other replica's active
    /// sandboxes — fleet-wide outage on every rolling restart. For
    /// HA, leave this off and run a separate prune job that
    /// considers Pod age / heartbeat Lease.
    ///
    /// `SANDBOX_K8S_STARTUP_ORPHAN_CLEANUP` (default `false`).
    pub startup_orphan_cleanup: bool,
}

/// Configuration for the **Nomad + Cloud Hypervisor** backend.
///
/// In this mode the controller submits a `raw_exec` Nomad job per
/// sandbox; the job spec invokes a wrapper script (shipped alongside
/// the controller, configured via `wrapper_path`) which spawns a CH
/// microVM with three virtio-fs shares (`keys`, `workspace`,
/// `userhome`) bound to host directories the controller has
/// pre-created.
///
/// Network plumbing (tap devices, /30 subnets) is assumed to be
/// pre-provisioned out-of-band — see
/// `docs/runbooks/sandbox-nomad-ch.md` for the host setup runbook.
#[derive(Clone, Debug)]
pub struct NomadCHConfig {
    /// Base URL for the Nomad HTTP API. We talk JSON over HTTP via
    /// `ureq` (inside `compio::runtime::spawn_blocking`); the `nomad`
    /// CLI is intentionally **not** used so the controller has no
    /// runtime dependency on the binary.
    /// `SANDBOX_NOMAD_ADDR` (default `http://127.0.0.1:4646`).
    pub nomad_addr: String,

    /// Datacenter name used in the submitted job spec's
    /// `Datacenters: [...]` field. Must match a datacenter the Nomad
    /// agent advertises. `SANDBOX_NOMAD_DATACENTER` (default `dc1`).
    pub datacenter: String,

    /// Absolute path to the wrapper script the `raw_exec` task
    /// invokes. The script is shipped at
    /// `crates/sandbox/scripts/nomad-vm-wrapper.sh`; operators copy
    /// it to a stable system path. `SANDBOX_NOMAD_CH_WRAPPER_PATH`
    /// (default `/etc/zeroship/nomad-vm-wrapper.sh`).
    pub wrapper_path: PathBuf,

    /// Host directory holding the kernel image (`vmlinuz`) and the
    /// rootfs template (`rootfs-slim.img`). Equivalent to the
    /// `ZSBX_HERE` env var in the demo wrapper. The wrapper `cd`s to
    /// this dir at startup. `SANDBOX_NOMAD_CH_RUNTIME_DIR` (default
    /// `/var/lib/zeroship/ch`).
    pub runtime_dir: PathBuf,

    /// Root directory for per-sandbox host state. Each sandbox gets
    /// `<host_state_dir>/<sandbox-id>/{keys,workspace}/`; the dirs are
    /// virtio-fs-shared into the VM. `SANDBOX_NOMAD_CH_HOST_STATE_DIR`
    /// (default `/var/zeroship/ch`).
    pub host_state_dir: PathBuf,

    /// Root for per-user persistent home directories — each user
    /// gets `<user_home_dir_root>/<user_id>/home/` shared into the
    /// VM as virtiofs tag `userhome` and mounted at `/home/u`. This
    /// directory persists across sandbox lifetimes (package caches,
    /// dotfiles). The next milestone replaces this with
    /// Ceph-RBD-backed volumes via a CSI plugin; for the single-node
    /// demo it's a host bind-mount.
    /// `SANDBOX_NOMAD_CH_USER_HOME_ROOT` (default `/var/zeroship/ch/users`).
    pub user_home_dir_root: PathBuf,

    /// Inclusive lower bound of the per-VM index pool. Each sandbox
    /// gets a unique index; the wrapper computes `tap=zsbx-nm-<idx>`,
    /// host IP `10.99.<100+idx>.1` and VM IP `10.99.<100+idx>.2`. The
    /// host operator is responsible for pre-creating tap devices in
    /// this range. Must be ≥ 1 (an index of 0 reserves the .100
    /// subnet for what's effectively a sentinel — confusing on
    /// inspection, no upside). `SANDBOX_NOMAD_CH_VM_INDEX_FLOOR`
    /// (default 1).
    pub vm_index_floor: u16,

    /// Inclusive upper bound of the index pool. Allocator hands out
    /// indices in `[floor, ceil]`; `alloc()` returns an error past
    /// `ceil`. Must be ≤ 155 — the IP arithmetic in the wrapper +
    /// controller is `10.99.{100+idx}.2`, and the third octet
    /// overflows past index 155. The cleaner alternative (stretching
    /// the subnet across two octets) costs us a bigger blast radius
    /// for off-by-one bugs and a less readable IP layout; the
    /// 155-VM ceiling is plenty for single-host operators (Cloud
    /// Hypervisor + 4 GiB/VM × 155 = 620 GiB RAM, well past any
    /// realistic single-box deploy). HA operators run multiple
    /// controller hosts. `SANDBOX_NOMAD_CH_VM_INDEX_CEIL` (default
    /// 155).
    pub vm_index_ceil: u16,

    /// How long to wait for an alloc to reach `ClientStatus="running"`
    /// after `POST /v1/jobs`. Bounds Nomad scheduling latency only —
    /// "running" means the wrapper script started, NOT that the VM is
    /// up. Past this we give up and the `CreateGuard` tears the job
    /// down. `SANDBOX_NOMAD_CH_ALLOC_RUNNING_TIMEOUT_SECS` (default
    /// 120). Should be enough to cover Nomad scheduling, plan-evaluate,
    /// and `raw_exec` task launch on a healthy cluster (typically
    /// well under 5s; 120s leaves room for a reschedule under load).
    ///
    /// Was 60s pre-Phase-3 stress run; bumped to 120s after the May-5
    /// cluster stress (31/60 creates timed out before reaching
    /// alloc-running under c=60 on a single n2-standard-32 worker —
    /// concurrent VM density past round-2's 16-cap pushed Nomad
    /// scheduler + raw_exec launch latency well past the 60s budget).
    /// Mirrors the `host_fence_timeout_secs` 30→120 bump from
    /// cad098e6 — same root cause (single-worker saturation under
    /// concurrent ops), same shape of fix.
    pub alloc_running_timeout_secs: u64,

    /// Once the alloc is running, how long to wait for the in-VM
    /// agent to start serving 200s on `/livez`. Bounds CH boot +
    /// kernel + init.sh + agent startup. Separate budget from
    /// `alloc_running_timeout_secs` because the failure mode is
    /// different — slow Nomad means the cluster is unhealthy; slow
    /// `/livez` means CH/kernel/agent inside the VM. Setting a
    /// single combined timeout would conflate these and give
    /// operators worse signal. `SANDBOX_NOMAD_CH_AGENT_LIVEZ_TIMEOUT_SECS`
    /// (default 30). CH typically boots in ~3-4 s on a healthy host;
    /// the agent comes up immediately after init.sh execs it.
    pub agent_livez_timeout_secs: u64,

    /// On controller startup, list every Nomad job whose `ID` starts
    /// with the `zsbx-` prefix and stop+purge it. Same trade-off as
    /// the K8s flag: useful for single-replica deployments to
    /// recover after a crash; **dangerous in HA** because the first
    /// replica nukes every other replica's active sandboxes on
    /// rolling restart. Default off; opt-in for single-node operators.
    /// `SANDBOX_NOMAD_CH_STARTUP_ORPHAN_CLEANUP` (default `false`).
    pub startup_orphan_cleanup: bool,

    /// FM-F: After Nomad reports the alloc terminal we run a
    /// **host-side fence** on the agent IP — poll `/livez` until we
    /// see two consecutive failures (connection refused, timeout, or
    /// 5xx) before releasing `vm_index`. Why: Nomad's "alloc
    /// terminal" lags the host-process tree (cloud-hypervisor + 3×
    /// virtiofsd + the bash wrapper) by 0.5–60 s under N=8 stress.
    /// Releasing the index while the previous tenant's agent is
    /// still listening on `10.99.<100+idx>.2:7777` is the exact
    /// race FM-A's fingerprint check papers over; this is the
    /// **primary** fix.
    ///
    /// If the fence times out (the prior tenant's agent keeps
    /// answering past this budget) we **leak** the vm_index — safer
    /// to shrink the pool than hand out an IP whose agent is still
    /// alive. Operator log will say `vm_index: leak=<n> reason=
    /// host_fence_timeout`. Orphan-prune at next controller boot
    /// reclaims it indirectly. `SANDBOX_NOMAD_CH_HOST_FENCE_TIMEOUT_SECS`
    /// (default 120). Set to 0 to disable the fence entirely (NOT
    /// recommended in production — restores the FM-F race).
    ///
    /// Was 30s pre-Phase-3 stress run; bumped to 120s after measuring
    /// 30-way concurrent stop on a single n2-standard-32 worker:
    /// fence p95=29s, max=31.1s — i.e., 30s is too tight when many CH
    /// processes tear down concurrently (worker IO/CPU contention
    /// during teardown, NOT the controller polling cadence). 60-way
    /// burst can push p99 well past 30s. 120s gives 4× headroom while
    /// still bounding the per-stop budget to "minutes, not hours".
    pub host_fence_timeout_secs: u64,

    /// Second octet of the per-VM /30 subnet. Default 99 keeps the
    /// historical 10.99/16 layout. Operators on hosts with a corp
    /// 10.99/16 collision can shift this — both controller (which
    /// computes `agent_url = http://10.<base>.<100+idx>.2:7777`)
    /// and the wrapper (which lays down the tap + IP) read the same
    /// value. Validated to a non-multicast / non-loopback / non-
    /// link-local prefix so a typo'd `127` or `169` doesn't quietly
    /// produce unreachable IPs.
    /// `SANDBOX_NOMAD_CH_SUBNET_BASE_OCTET` (default 99).
    pub subnet_second_octet: u8,
}

/// r27-S1 Guard A: reject any `nomad_addr` whose host is NOT a
/// loopback literal. Accept-list:
///
/// - `localhost` (resolves to 127.0.0.1 / ::1 on every sane libc)
/// - any IPv4 in `127.0.0.0/8` (validated via `Ipv4Addr::is_loopback`)
/// - the IPv6 loopback `::1` (validated via `Ipv6Addr::is_loopback`)
///
/// **Why a hand-rolled host parse, not the `url` crate**: the
/// sandbox crate does not depend on `url` today and the upstream
/// `nomad_addr` shape is already validated to start with `http://`
/// or `https://`. The host substring is everything between the
/// scheme and the first of `:` (port), `/` (path), `?` (query),
/// `#` (fragment), or end-of-string. IPv6 literals use the bracketed
/// `[::1]:port` form per RFC 3986 § 3.2.2; we handle that
/// specifically before the colon-as-port-separator rule fires so
/// `[::1]:4646` parses as host `::1` not host `[::1` (which would
/// fail the IP literal parse) or host `[` (which would silently
/// match nothing).
///
/// `nomad_addr` MUST already have the scheme prefix validated by the
/// caller; we treat its absence as a programmer error and refuse.
fn validate_nomad_addr_loopback(addr: &str) -> Result<(), String> {
    // Strip scheme. The caller has already asserted one of these
    // prefixes is present.
    let rest = if let Some(r) = addr.strip_prefix("http://") {
        r
    } else if let Some(r) = addr.strip_prefix("https://") {
        r
    } else {
        return Err(format!(
            "SANDBOX_NOMAD_ADDR missing http(s):// scheme (internal: \
             scheme check must run before loopback check); got {addr:?}",
        ));
    };

    // Host extraction. Two shapes per RFC 3986 § 3.2.2:
    //   - Bracketed IPv6: `[<v6>](:port)?(/path)?`
    //   - Everything else: `<host>(:port)?(/path)?`
    let host: &str = if let Some(after_lb) = rest.strip_prefix('[') {
        // Find the closing bracket; everything between is the IPv6
        // literal. An unclosed bracket is malformed.
        match after_lb.find(']') {
            Some(end) => &after_lb[..end],
            None => {
                return Err(format!(
                    "SANDBOX_NOMAD_ADDR has unclosed IPv6 bracket; got {addr:?}",
                ))
            }
        }
    } else {
        // Host body ends at the first of ':' / '/' / '?' / '#' /
        // end-of-string.
        let end = rest
            .find(|c: char| matches!(c, ':' | '/' | '?' | '#'))
            .unwrap_or(rest.len());
        &rest[..end]
    };

    if host.is_empty() {
        return Err(format!(
            "SANDBOX_NOMAD_ADDR missing host; got {addr:?}",
        ));
    }

    // Cheap path: literal `localhost` is always loopback on a sane
    // libc. We do NOT call `getaddrinfo` to follow `/etc/hosts`
    // overrides — an operator who has mapped `localhost` to a
    // remote in `/etc/hosts` has bigger problems than this guard.
    if host.eq_ignore_ascii_case("localhost") {
        return Ok(());
    }

    // IP literal path: parse and let the std library check the
    // loopback bit (covers 127.0.0.0/8 in v4 and ::1 in v6, neither
    // of which we need to spell out octet-by-octet here).
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if ip.is_loopback() {
            return Ok(());
        }
        return Err(format!(
            "SANDBOX_NOMAD_ADDR host {host:?} is not loopback; refusing to \
             boot to prevent cross-cluster placement (r27-S1 Guard A). \
             Accepted: localhost, 127.0.0.0/8, ::1.",
        ));
    }

    // Non-IP, non-localhost hostname (e.g. `nomad.example.com`,
    // `nomad`, `nomad.local`): refuse. We don't `getaddrinfo`
    // because (a) startup-time DNS may not match runtime DNS and
    // (b) operators with a `127.0.0.1 nomad-local` entry in
    // `/etc/hosts` can use that name verbatim only if it resolves
    // — but we'd rather they spell `127.0.0.1` so the config is
    // self-documenting.
    Err(format!(
        "SANDBOX_NOMAD_ADDR host {host:?} is not a loopback literal; \
         refusing to boot to prevent cross-cluster placement (r27-S1 \
         Guard A). Accepted: localhost, 127.0.0.0/8, ::1.",
    ))
}

impl NomadCHConfig {
    /// Validate the parsed config. Called from
    /// [`SandboxConfig::from_env`] before the backend is instantiated
    /// so misconfig surfaces at startup, not on the first sandbox.
    ///
    /// Side-effect: trims a single trailing '/' from `nomad_addr` so
    /// downstream callers can do `format!("{nomad_addr}/v1/...")`
    /// without producing a malformed `//v1/...` URL when an operator
    /// pastes a URL with a trailing slash. Idempotent; only one
    /// slash is trimmed (we don't try to canonicalize beyond that).
    pub fn validate(&mut self) -> Result<(), String> {
        if self.nomad_addr.ends_with('/') {
            self.nomad_addr.pop();
        }
        if self.vm_index_floor < 1 {
            // floor=0 would set MAC `12:34:56:78:9b:00` and IP
            // `10.99.100.2`, pre-empting the .100 subnet for what's
            // effectively a sentinel index. Reject at config load so
            // operators discover the misconfig before the first
            // sandbox tries to boot on it.
            return Err(
                "SANDBOX_NOMAD_CH_VM_INDEX_FLOOR must be ≥ 1 (got 0)".to_string(),
            );
        }
        if self.vm_index_floor > self.vm_index_ceil {
            return Err(format!(
                "SANDBOX_NOMAD_CH_VM_INDEX_FLOOR ({}) > _CEIL ({})",
                self.vm_index_floor, self.vm_index_ceil
            ));
        }
        // IP-safety: the controller derives the agent IP as
        // 10.99.{100+vm_index}.2. The third octet must fit in a u8,
        // so 100 + vm_index_ceil ≤ 255 → vm_index_ceil ≤ 155. Catch
        // misconfig at startup so we don't 500 with "garbage IP" on
        // the first late-pool sandbox.
        const SUBNET_BASE_OCTET: u32 = 100;
        const MAX_OCTET: u32 = 255;
        const MAX_CEIL: u32 = MAX_OCTET - SUBNET_BASE_OCTET;
        if (SUBNET_BASE_OCTET + self.vm_index_ceil as u32) > MAX_OCTET {
            return Err(format!(
                "SANDBOX_NOMAD_CH_VM_INDEX_CEIL ({}) would overflow IP \
                 third octet (10.99.{}.2). Max is {}.",
                self.vm_index_ceil,
                SUBNET_BASE_OCTET + self.vm_index_ceil as u32,
                MAX_CEIL,
            ));
        }
        // SANDBOX_NOMAD_ADDR scheme: an empty / scheme-less value
        // would silently fail at first sandbox create rather than at
        // controller startup. Cheap to check now.
        if !self.nomad_addr.starts_with("http://")
            && !self.nomad_addr.starts_with("https://")
        {
            return Err(format!(
                "SANDBOX_NOMAD_ADDR must start with http:// or https://; got {:?}",
                self.nomad_addr,
            ));
        }
        // r27-S1 Guard A (fail-CLOSED): reject any nomad_addr whose
        // host is NOT a loopback literal. Two attack vectors this
        // closes:
        //
        // 1. **Tampered local Nomad agent** returns a wrong `node_id`
        //    on `/v1/agent/self` → r3-A controller emits a Job-level
        //    Constraints block pinning to the wrong node → cluster-
        //    wide CREATE/WAKE DoS until a restart.
        // 2. **Misconfigured `NOMAD_ADDR`** points at a remote
        //    Nomad → controller stages `workspace.img` on the LOCAL
        //    filesystem then emits Constraints pinning to a node in a
        //    DIFFERENT cluster → allocs land cross-cluster and the
        //    `assert_disk_image_present` driver-stat ENOENTs (the
        //    r3-A failure mode by another route).
        //
        // The fix matches r3-A's strict-equality Constraints choice
        // (Operand = "=", refusing fallback to random placement): we
        // refuse to boot rather than emit a Constraints block keyed
        // off an unverifiable remote node_id. Loopback enforcement is
        // the only check that makes the assumption "the Nomad agent
        // returns this host's node_id" structurally true.
        validate_nomad_addr_loopback(&self.nomad_addr)?;
        // m4: validate the wrapper script exists + is executable.
        // **Caveat:** this only catches misconfig when the
        // controller and the Nomad client share a filesystem
        // (single-node deploys). On split deployments where the
        // Nomad agent runs on different hosts, this check is
        // best-effort — we can confirm the path is wrong, but a
        // path that's correct on the controller may still be
        // wrong on the Nomad client.
        match std::fs::metadata(&self.wrapper_path) {
            Ok(md) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mode = md.permissions().mode();
                    if mode & 0o111 == 0 {
                        return Err(format!(
                            "SANDBOX_NOMAD_CH_WRAPPER_PATH ({}) is not \
                             executable (mode={:o}); chmod +x or fix the \
                             path.",
                            self.wrapper_path.display(),
                            mode
                        ));
                    }
                }
                let _ = md; // silence unused on non-unix
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Not fatal in split-deploy mode: the controller's
                // FS may not be the Nomad client's FS. Log only.
                tracing::info!(
                    wrapper_path = %self.wrapper_path.display(),
                    "sandbox/nomad-ch config: wrapper_path not present on controller fs (best-effort check; irrelevant if Nomad client runs on a different host)"
                );
            }
            Err(_) => {
                // Permission denied / IO error reading metadata —
                // also likely a split-deploy artefact. Don't block.
            }
        }
        // M6: subnet second octet must be a private-range value.
        // 10.0.0.0/8 is RFC1918 private, but the OPERATOR sets the
        // second octet — a typo of `127` would land on loopback
        // and `169` on link-local. Refuse those at startup so the
        // misconfig surfaces here, not on the first sandbox boot.
        match self.subnet_second_octet {
            // Loopback (127.0.0.0/8) — pretty much guaranteed
            // unreachable to the controller.
            127 => return Err(format!(
                "SANDBOX_NOMAD_CH_SUBNET_BASE_OCTET=127 lands on \
                 the loopback /8; this is almost certainly a typo \
                 (default is 99)."
            )),
            // Link-local 169.254.0.0/16 — DHCP failure prefix.
            169 => return Err(format!(
                "SANDBOX_NOMAD_CH_SUBNET_BASE_OCTET=169 lands on \
                 link-local 169.254/16; refusing."
            )),
            // Multicast 224-239 / reserved 240-255.
            224..=255 => return Err(format!(
                "SANDBOX_NOMAD_CH_SUBNET_BASE_OCTET={} lands on \
                 multicast/reserved; refusing.",
                self.subnet_second_octet
            )),
            _ => {}
        }
        // host_state_dir / user_home_dir_root overlap: the canonical
        // layout is `host_state_dir/users/<user>/home`, i.e.
        // user_home_dir_root == host_state_dir.join("users"). Any
        // *other* descendant relationship risks a future operator
        // misconfig where a sandbox-id-named dir under host_state_dir
        // collides with a user-id-named dir under user_home_dir_root.
        // Sandbox IDs are 32-hex UUIDs today (collision-free in
        // practice) but the invariant deserves to be explicit. Allow
        // either the canonical layout or fully disjoint trees.
        if self.user_home_dir_root != self.host_state_dir.join("users")
            && self.user_home_dir_root.starts_with(&self.host_state_dir)
        {
            return Err(format!(
                "SANDBOX_NOMAD_CH_USER_HOME_ROOT ({}) is a descendant of \
                 SANDBOX_NOMAD_CH_HOST_STATE_DIR ({}) but not the canonical \
                 `<host_state_dir>/users` layout — refusing to start to \
                 avoid sandbox-id / user-id path collision.",
                self.user_home_dir_root.display(),
                self.host_state_dir.display(),
            ));
        }
        Ok(())
    }
}

impl SandboxConfig {
    /// A7 (deferred): read-only accessor for the creator-side bearer
    /// [`SandboxConfig::token`]. `pub` (not `pub(crate)`) because the
    /// `zeroship-sandbox` **binary** target (`src/main.rs`) is a
    /// separate crate from the library and needs to log
    /// "endpoints are unauthenticated" when the token is empty
    /// (`SANDBOX_ALLOW_NO_AUTH=true` dev path). In-crate readers
    /// (`auth.rs`, `preview_ws.rs`) bypass the accessor and read
    /// `self.token` directly via the `pub(crate)` field — this
    /// accessor is purely for the out-of-crate bin.
    ///
    /// Callers MUST use constant-time comparison
    /// (`subtle::ConstantTimeEq` via `auth::check_token`) when
    /// matching against user-presented bytes — never `==`.
    pub fn token(&self) -> &ApiToken {
        &self.token
    }

    /// A7 (deferred): safe builder for [`SandboxConfig::token`]. The
    /// field itself is `pub(crate)` so external callers (notably
    /// integration tests in `crates/sandbox/tests/`) cannot construct
    /// or mutate it directly — they go through this builder.
    ///
    /// Unlike `AppState::with_admin_token`, this setter does NOT
    /// reject empty `ApiToken`s. Empty-token semantics are a
    /// **deployment** decision validated at boot in [`Self::from_env`]
    /// (it refuses to start with an empty `SANDBOX_TOKEN` unless the
    /// operator opts in via `SANDBOX_ALLOW_NO_AUTH=true`), not a
    /// per-field invariant. Tests legitimately need to construct
    /// configs with empty / short tokens to exercise the no-auth and
    /// boot-rejection paths; rejecting empty here would block those.
    /// Returning plain `Self` (not `Result<Self, String>`) reflects
    /// that this builder cannot fail — matches the R3-Q2 finding that
    /// always-`Ok` builders are a smell.
    ///
    /// Replaces any prior value.
    pub fn with_token(mut self, token: ApiToken) -> Self {
        self.token = token;
        self
    }

    /// A7 (deferred): public fixture constructor for out-of-crate
    /// integration tests. Returns a `SandboxConfig` populated with
    /// defaults that exercise the `nomad-ch` backend wiring without
    /// touching the network (the network probe runs in
    /// `Backend::from_config`, not here). The `token` field is set
    /// to a non-empty placeholder; tests that need a specific
    /// bearer chain [`Self::with_token`].
    ///
    /// This is the only legal out-of-crate construction path now
    /// that `token` is `pub(crate)`. Production code uses
    /// [`Self::from_env`].
    ///
    /// Non-`token` fields remain `pub`, so tests that need to vary
    /// e.g. `snapshot_enabled` or `port` can still mutate them via
    /// direct field assignment on a `let mut cfg = new_fixture()`.
    pub fn new_fixture() -> Self {
        Self {
            port: 9091,
            token: ApiToken::new("ignored-creator-token"),
            backend: "nomad-ch".into(),
            image: "img".into(),
            workspace_root: PathBuf::from("/var/zeroship/projects"),
            network: "n".into(),
            memory_mb: 1024,
            cpus: 2.0,
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
                vm_index_ceil: 200,
                alloc_running_timeout_secs: 120,
                agent_livez_timeout_secs: 30,
                host_fence_timeout_secs: 30,
                startup_orphan_cleanup: false,
                subnet_second_octet: 99,
            },
            create_retry_max: 2,
            create_retry_total_timeout_secs: 90,
            snapshot_enabled: false,
            snapshot_l1_root: PathBuf::from("/var/zeroship/ch/snapshots"),
            snapshot_use_gcs: false,
            snapshot_gcs_bucket: None,
            snapshot_root_kek_path: None,
            workspace_image_size_gb: 20,
        }
    }

    pub fn from_env() -> Result<Self, String> {
        let port = parse_env("SANDBOX_PORT", 9091u16)?;
        let token_raw = std::env::var("SANDBOX_TOKEN").unwrap_or_default();
        // Fail-closed: an unset/empty token disables auth. We refuse
        // to start in that state unless the operator opts in via
        // SANDBOX_ALLOW_NO_AUTH=true. Tighten further: when a token
        // is set, require ≥32 bytes — anything shorter is brute-
        // forceable on a public endpoint.
        let allow_no_auth = parse_env("SANDBOX_ALLOW_NO_AUTH", false)?;
        if token_raw.is_empty() && !allow_no_auth {
            return Err(
                "SANDBOX_TOKEN is empty; refusing to start. \
                 Set SANDBOX_TOKEN to a strong (≥32 byte) random value, \
                 or set SANDBOX_ALLOW_NO_AUTH=true for explicit dev mode."
                    .to_string(),
            );
        }
        if !token_raw.is_empty() && token_raw.len() < 32 {
            return Err(format!(
                "SANDBOX_TOKEN is too short ({} bytes; need ≥ 32). \
                 Generate with: head -c 32 /dev/urandom | base64",
                token_raw.len()
            ));
        }
        let token = ApiToken::new(token_raw);
        let backend = std::env::var("SANDBOX_BACKEND").unwrap_or_else(|_| "docker".to_string());
        if !matches!(backend.as_str(), "docker" | "k8s" | "nomad-ch") {
            return Err(format!(
                "SANDBOX_BACKEND={backend:?}; expected \"docker\", \"k8s\", or \"nomad-ch\""
            ));
        }
        let image = std::env::var("SANDBOX_IMAGE")
            .unwrap_or_else(|_| "zeroship/sandbox-base:latest".to_string());
        let workspace_root = PathBuf::from(
            std::env::var("SANDBOX_WORKSPACE_ROOT")
                .unwrap_or_else(|_| "/var/zeroship/projects".to_string()),
        );
        let network = std::env::var("SANDBOX_NETWORK")
            .unwrap_or_else(|_| "zeroship-sandbox-net".to_string());
        let memory_mb = parse_env("SANDBOX_MEMORY_MB", 1024u32)?;
        let cpus = parse_env("SANDBOX_CPUS", 2.0f32)?;
        let idle_timeout_secs = parse_env("SANDBOX_IDLE_TIMEOUT_SECS", 1800u64)?;
        let max_lifetime_secs = parse_env("SANDBOX_MAX_LIFETIME_SECS", 28800u64)?;
        let auto_pull = parse_env("SANDBOX_AUTO_PULL", false)?;

        if cpus <= 0.0 || cpus > 64.0 {
            return Err(format!("SANDBOX_CPUS out of range: {cpus}"));
        }
        if memory_mb < 64 {
            return Err(format!("SANDBOX_MEMORY_MB too small: {memory_mb}"));
        }

        let user_home_storage_class = std::env::var("SANDBOX_K8S_USER_HOME_STORAGE_CLASS")
            .ok()
            .filter(|s| !s.is_empty());
        let k8s = K8sConfig {
            namespace: std::env::var("SANDBOX_K8S_NAMESPACE")
                .unwrap_or_else(|_| "default".to_string()),
            image: std::env::var("SANDBOX_K8S_IMAGE")
                .unwrap_or_else(|_| "docker.io/zeroship/sandbox-agent:dev".to_string()),
            runtime_class: std::env::var("SANDBOX_K8S_RUNTIME_CLASS")
                .unwrap_or_else(|_| "kvm-sandbox".to_string()),
            ready_timeout_secs: parse_env("SANDBOX_K8S_READY_TIMEOUT_SECS", 120u64)?,
            use_port_forward: parse_env("SANDBOX_K8S_USE_PORT_FORWARD", true)?,
            port_forward_start: parse_env("SANDBOX_K8S_PORT_FORWARD_START", 18000u16)?,
            user_home_size: std::env::var("SANDBOX_K8S_USER_HOME_SIZE")
                .unwrap_or_else(|_| "5Gi".to_string()),
            user_home_storage_class,
            startup_orphan_cleanup: parse_env("SANDBOX_K8S_STARTUP_ORPHAN_CLEANUP", false)?,
        };

        let nomad_ch = NomadCHConfig {
            nomad_addr: std::env::var("SANDBOX_NOMAD_ADDR")
                .unwrap_or_else(|_| "http://127.0.0.1:4646".to_string()),
            datacenter: std::env::var("SANDBOX_NOMAD_DATACENTER")
                .unwrap_or_else(|_| "dc1".to_string()),
            wrapper_path: PathBuf::from(
                std::env::var("SANDBOX_NOMAD_CH_WRAPPER_PATH")
                    .unwrap_or_else(|_| "/etc/zeroship/nomad-vm-wrapper.sh".to_string()),
            ),
            runtime_dir: PathBuf::from(
                std::env::var("SANDBOX_NOMAD_CH_RUNTIME_DIR")
                    .unwrap_or_else(|_| "/var/lib/zeroship/ch".to_string()),
            ),
            host_state_dir: PathBuf::from(
                std::env::var("SANDBOX_NOMAD_CH_HOST_STATE_DIR")
                    .unwrap_or_else(|_| "/var/zeroship/ch".to_string()),
            ),
            user_home_dir_root: PathBuf::from(
                std::env::var("SANDBOX_NOMAD_CH_USER_HOME_ROOT")
                    .unwrap_or_else(|_| "/var/zeroship/ch/users".to_string()),
            ),
            vm_index_floor: parse_env("SANDBOX_NOMAD_CH_VM_INDEX_FLOOR", 1u16)?,
            vm_index_ceil: parse_env("SANDBOX_NOMAD_CH_VM_INDEX_CEIL", 155u16)?,
            alloc_running_timeout_secs: parse_env(
                "SANDBOX_NOMAD_CH_ALLOC_RUNNING_TIMEOUT_SECS",
                120u64,
            )?,
            agent_livez_timeout_secs: parse_env(
                "SANDBOX_NOMAD_CH_AGENT_LIVEZ_TIMEOUT_SECS",
                30u64,
            )?,
            host_fence_timeout_secs: parse_env(
                "SANDBOX_NOMAD_CH_HOST_FENCE_TIMEOUT_SECS",
                120u64,
            )?,
            startup_orphan_cleanup: parse_env(
                "SANDBOX_NOMAD_CH_STARTUP_ORPHAN_CLEANUP",
                false,
            )?,
            subnet_second_octet: parse_env(
                "SANDBOX_NOMAD_CH_SUBNET_BASE_OCTET",
                99u8,
            )?,
        };

        let mut nomad_ch = nomad_ch;
        nomad_ch.validate()?;

        let create_retry_max = parse_env("SANDBOX_CREATE_RETRY_MAX", 2u32)?;
        let create_retry_total_timeout_secs =
            parse_env("SANDBOX_CREATE_RETRY_TOTAL_TIMEOUT_SECS", 90u64)?;
        let snapshot_enabled = parse_env("SANDBOX_SNAPSHOT_ENABLED", false)?;
        let snapshot_l1_root = PathBuf::from(
            std::env::var("SANDBOX_SNAPSHOT_L1_ROOT")
                .unwrap_or_else(|_| "/var/zeroship/ch/snapshots".to_string()),
        );
        let snapshot_use_gcs = parse_env("SANDBOX_SNAPSHOT_USE_GCS", false)?;
        let snapshot_gcs_bucket = std::env::var("SANDBOX_SNAPSHOT_GCS_BUCKET")
            .ok()
            .filter(|s| !s.is_empty());
        let snapshot_root_kek_path = std::env::var("SANDBOX_SNAPSHOT_ROOT_KEK_PATH")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);

        // Validate: GCS path requires the bucket. Refuse to boot
        // with use_gcs=true and no bucket — the operator's intent
        // is clear and silently falling back to L1-only would
        // surprise them on the first put.
        if snapshot_enabled && snapshot_use_gcs && snapshot_gcs_bucket.is_none() {
            return Err(
                "SANDBOX_SNAPSHOT_USE_GCS=true requires SANDBOX_SNAPSHOT_GCS_BUCKET to be set"
                    .to_string(),
            );
        }

        let workspace_image_size_gb =
            parse_env("SANDBOX_WORKSPACE_IMAGE_SIZE_GB", 20u32)?;
        if workspace_image_size_gb == 0 {
            return Err(
                "SANDBOX_WORKSPACE_IMAGE_SIZE_GB=0; refusing to start \
                 (mkfs.ext4 against a 0-byte sparse file aborts). \
                 Set to ≥ 1."
                    .to_string(),
            );
        }

        Ok(Self {
            port, token, backend, image, workspace_root, network,
            memory_mb, cpus, idle_timeout_secs, max_lifetime_secs, auto_pull,
            k8s, nomad_ch,
            create_retry_max,
            create_retry_total_timeout_secs,
            snapshot_enabled,
            snapshot_l1_root,
            snapshot_use_gcs,
            snapshot_gcs_bucket,
            snapshot_root_kek_path,
            workspace_image_size_gb,
        })
    }
}

fn parse_env<T: std::str::FromStr>(key: &str, default: T) -> Result<T, String>
where
    T::Err: std::fmt::Display,
{
    match std::env::var(key) {
        Ok(v) => v.parse().map_err(|e| format!("{key}: {e}")),
        Err(_) => Ok(default),
    }
}

// ────────────────────────────────────────────────────────────────────
// C-7-LT wake-response mode
// ────────────────────────────────────────────────────────────────────

/// C-7-LT: wake-response contract mode.
///
/// - `Sync` (default): legacy 200 OK with full wake state baked into the
///   response. Subject to C-8c (synchronous contract structurally exhausted
///   at empirical teardown ≥ client deadline; see smoke-r11 review). Kept
///   default during the C-7-LT migration so PR1's scaffolding ships
///   behind a feature flag without disturbing existing tests / smoke runs.
/// - `Async`: 202 Accepted + polling. New shape per
///   `docs/proposals/c7-lt-async-wake.md`. PR2 reads this flag in the wake
///   handler; PR1 only carries the flag.
///
/// Env: `SANDBOX_WAKE_RESPONSE_MODE` (sync|async). Defaults to `sync`.
/// Unrecognised values cause [`from_env`] to return `Err` — the
/// controller refuses to boot. This is fail-CLOSED (R16-S4 mirror of
/// R15-S1's AEAD posture): a misconfigured feature flag is a config
/// bug, not a silent fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeResponseMode {
    /// 200 OK + full wake state baked in (legacy).
    Sync,
    /// 202 Accepted + polling (C-7-LT proposal).
    Async,
}

impl WakeResponseMode {
    /// Resolve the mode from `SANDBOX_WAKE_RESPONSE_MODE`.
    ///
    /// - Unset / empty → `Sync` (default; existing contract).
    /// - `sync` → `Sync`.
    /// - `async` → `Async`.
    /// - Anything else → `Err` (R16-S4 fail-CLOSED). The boot path
    ///   in `AppState::from_config` propagates the error, refusing
    ///   to start with an ambiguous feature-flag value. Mirrors the
    ///   R15-S1 / A1-FOLLOWUP AEAD pattern: misconfig is a config
    ///   bug, not a silent papering-over.
    pub fn from_env() -> Result<Self, String> {
        match std::env::var("SANDBOX_WAKE_RESPONSE_MODE").as_deref() {
            Ok("async") => Ok(Self::Async),
            Ok("sync") | Ok("") | Err(_) => Ok(Self::Sync),
            Ok(other) => Err(format!(
                "SANDBOX_WAKE_RESPONSE_MODE={other:?} not recognized; \
                 expected one of: sync, async, \"\" (empty)"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sync => "sync",
            Self::Async => "async",
        }
    }

    pub fn is_async(self) -> bool {
        matches!(self, Self::Async)
    }
}

/// Configuration knobs for the C-7-LT wake-job lifecycle. All
/// fields are resolved once at boot from env vars and stored on
/// `AppState`; the controller does not support runtime reload (the
/// flag is structural — flipping it mid-flight would strand
/// in-flight wakes between contracts).
#[derive(Debug, Clone, Copy)]
pub struct WakeLifecycleConfig {
    /// Retention period for terminal wake_jobs rows. The GC sweep
    /// (`sweep::run_wake_jobs_gc_once`) deletes terminal
    /// (`ok`/`failed`) rows whose `updated_at` is older than this.
    ///
    /// **Minimum**: must exceed the client's max polling interval —
    /// otherwise a client that observes a row terminal-ok at T and
    /// re-polls at T + retention sees a 404 wake_not_found before
    /// it gets to read the final response. 5 min is the default;
    /// dev / test can set lower via `SANDBOX_WAKE_JOBS_GC_RETENTION_SECS`.
    ///
    /// Env: `SANDBOX_WAKE_JOBS_GC_RETENTION_SECS`. Default 300 s
    /// (5 min). Values < 1 s are rejected (would race the client's
    /// first poll).
    pub wake_jobs_gc_retention_secs: u64,

    /// R19-C1 takeover sweep threshold. The takeover sweep
    /// (`sweep::run_wake_jobs_takeover_once`) claims non-terminal
    /// rows whose `lessee_updated_at` is older than this — those
    /// rows lost their controller mid-wake.
    ///
    /// **Minimum**: must comfortably exceed the longest single state
    /// transition's wall-time (the wake state machine bumps
    /// `lessee_updated_at` on every transition; a too-tight threshold
    /// would steal in-flight rows from a still-progressing
    /// controller). The wake ladder's worst-case is bounded by the
    /// CH-restore + livez-poll + clock-resync + register stages,
    /// each capped at tens of seconds. 60 s is the floor that
    /// matches the documented agent_livez_timeout (30 s) +
    /// host_fence (30 s) + a safety margin.
    ///
    /// Env: `SANDBOX_WAKE_JOBS_TAKEOVER_THRESHOLD_SECS`.
    /// Default 60 s. Values < `MIN_TAKEOVER_THRESHOLD_SECS` rejected
    /// at boot.
    pub takeover_threshold_secs: u64,
}

impl WakeLifecycleConfig {
    pub const DEFAULT_GC_RETENTION_SECS: u64 = 300;
    pub const MIN_GC_RETENTION_SECS: u64 = 1;
    /// R19-C1 default takeover threshold: 60 s. A wake whose lessee
    /// hasn't bumped `lessee_updated_at` in 60 s is presumed
    /// abandoned (the wake ladder's longest single-stage timeout —
    /// the agent /livez poll — is 30 s; doubling that gives one
    /// safety-margin step on either side).
    pub const DEFAULT_TAKEOVER_THRESHOLD_SECS: u64 = 60;
    /// R19-C1 minimum takeover threshold: 30 s. Below this the
    /// in-flight CH-restore / livez-poll stages could race a
    /// healthy controller and steal its row. Boot refuses to start
    /// with a value below this floor.
    pub const MIN_TAKEOVER_THRESHOLD_SECS: u64 = 30;

    /// Resolve from env. Unset → defaults; unparseable / out-of-
    /// range → `Err` (boot refuses to start).
    pub fn from_env() -> Result<Self, String> {
        let retention = match std::env::var("SANDBOX_WAKE_JOBS_GC_RETENTION_SECS") {
            Ok(s) if !s.trim().is_empty() => {
                let n: u64 = s.trim().parse().map_err(|e| {
                    format!(
                        "SANDBOX_WAKE_JOBS_GC_RETENTION_SECS={s:?}: parse: {e}"
                    )
                })?;
                if n < Self::MIN_GC_RETENTION_SECS {
                    return Err(format!(
                        "SANDBOX_WAKE_JOBS_GC_RETENTION_SECS={n} \
                         must be >= {} (the minimum exceeds the client's \
                         max polling interval; a smaller value races \
                         the client's first post-terminal poll)",
                        Self::MIN_GC_RETENTION_SECS
                    ));
                }
                n
            }
            _ => Self::DEFAULT_GC_RETENTION_SECS,
        };
        let takeover = match std::env::var("SANDBOX_WAKE_JOBS_TAKEOVER_THRESHOLD_SECS") {
            Ok(s) if !s.trim().is_empty() => {
                let n: u64 = s.trim().parse().map_err(|e| {
                    format!(
                        "SANDBOX_WAKE_JOBS_TAKEOVER_THRESHOLD_SECS={s:?}: parse: {e}"
                    )
                })?;
                if n < Self::MIN_TAKEOVER_THRESHOLD_SECS {
                    return Err(format!(
                        "SANDBOX_WAKE_JOBS_TAKEOVER_THRESHOLD_SECS={n} \
                         must be >= {} (below this the takeover sweep \
                         could steal rows from a healthy mid-flight \
                         wake — the worst single-stage timeout in the \
                         wake ladder is ~30s)",
                        Self::MIN_TAKEOVER_THRESHOLD_SECS
                    ));
                }
                n
            }
            _ => Self::DEFAULT_TAKEOVER_THRESHOLD_SECS,
        };
        Ok(Self {
            wake_jobs_gc_retention_secs: retention,
            takeover_threshold_secs: takeover,
        })
    }
}

impl Default for WakeLifecycleConfig {
    fn default() -> Self {
        Self {
            wake_jobs_gc_retention_secs: Self::DEFAULT_GC_RETENTION_SECS,
            takeover_threshold_secs: Self::DEFAULT_TAKEOVER_THRESHOLD_SECS,
        }
    }
}

#[cfg(test)]
#[allow(unsafe_code)]
mod wake_response_mode_tests {
    use super::*;

    /// Test fixture: ENV mutation requires a serializing lock because
    /// `std::env::set_var` is process-global. Reuse a local mutex.
    /// Mirrors the `ENV_LOCK` / `with_env_clean` pattern in
    /// `crate::db::tests` (which also relies on `#[allow(unsafe_code)]`
    /// because `std::env::{set,remove}_var` is documented as unsafe in
    /// the 2024-edition stdlib).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_env<F: FnOnce()>(key: &str, value: Option<&str>, f: F) {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: ENV_LOCK serializes env mutation across this module's
        // tests; no other concurrent reader of this specific key exists
        // at test time (the flag is read only at boot by AppState).
        unsafe {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        f();
        // SAFETY: same as above — ENV_LOCK still held; lock guard
        // dropped at end of function.
        unsafe {
            std::env::remove_var(key);
        }
    }

    #[test]
    fn default_when_unset() {
        with_env("SANDBOX_WAKE_RESPONSE_MODE", None, || {
            assert_eq!(
                WakeResponseMode::from_env().unwrap(),
                WakeResponseMode::Sync
            );
        });
    }

    #[test]
    fn explicit_sync() {
        with_env("SANDBOX_WAKE_RESPONSE_MODE", Some("sync"), || {
            assert_eq!(
                WakeResponseMode::from_env().unwrap(),
                WakeResponseMode::Sync
            );
        });
    }

    #[test]
    fn explicit_async() {
        with_env("SANDBOX_WAKE_RESPONSE_MODE", Some("async"), || {
            assert_eq!(
                WakeResponseMode::from_env().unwrap(),
                WakeResponseMode::Async
            );
            assert!(WakeResponseMode::Async.is_async());
            assert!(!WakeResponseMode::Sync.is_async());
            assert_eq!(WakeResponseMode::Async.as_str(), "async");
            assert_eq!(WakeResponseMode::Sync.as_str(), "sync");
        });
    }

    #[test]
    fn empty_defaults_to_sync() {
        with_env("SANDBOX_WAKE_RESPONSE_MODE", Some(""), || {
            assert_eq!(
                WakeResponseMode::from_env().unwrap(),
                WakeResponseMode::Sync
            );
        });
    }

    /// R16-S4: unrecognised values fail-CLOSED. The previous
    /// contract (silent fallback to Sync) was a config-bug-papering
    /// hazard; mirrors R15-S1's AEAD fail-CLOSED.
    #[test]
    fn unrecognised_value_fails_closed() {
        with_env("SANDBOX_WAKE_RESPONSE_MODE", Some("polling"), || {
            let err = WakeResponseMode::from_env().expect_err(
                "unrecognised value must return Err, not silently default",
            );
            assert!(
                err.contains("polling"),
                "error must mention the offending value; got: {err}"
            );
        });
    }

    #[test]
    fn unrecognised_uppercase_async_fails_closed() {
        with_env("SANDBOX_WAKE_RESPONSE_MODE", Some("ASYNC"), || {
            let err = WakeResponseMode::from_env()
                .expect_err("ASYNC (uppercase) must reject");
            assert!(err.contains("ASYNC"));
        });
    }

    #[test]
    fn unrecognised_truthy_strings_fail_closed() {
        for v in ["1", "true", "on", "yes"] {
            with_env("SANDBOX_WAKE_RESPONSE_MODE", Some(v), || {
                assert!(
                    WakeResponseMode::from_env().is_err(),
                    "value {v:?} must NOT silently map to Sync or Async"
                );
            });
        }
    }
}

#[cfg(test)]
#[allow(unsafe_code)]
mod wake_lifecycle_config_tests {
    use super::*;

    /// Distinct ENV_LOCK per-key (the R12-S1 carry — codified by
    /// R16-S3-b). This test module touches only
    /// `SANDBOX_WAKE_JOBS_GC_RETENTION_SECS`.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_env<F: FnOnce()>(key: &str, value: Option<&str>, f: F) {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: ENV_LOCK serialises this module's env mutations.
        unsafe {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        f();
        unsafe {
            std::env::remove_var(key);
        }
    }

    #[test]
    fn default_retention_when_unset() {
        with_env("SANDBOX_WAKE_JOBS_GC_RETENTION_SECS", None, || {
            let cfg = WakeLifecycleConfig::from_env().unwrap();
            assert_eq!(
                cfg.wake_jobs_gc_retention_secs,
                WakeLifecycleConfig::DEFAULT_GC_RETENTION_SECS
            );
            assert_eq!(cfg.wake_jobs_gc_retention_secs, 300);
        });
    }

    #[test]
    fn explicit_retention_value_accepted() {
        with_env("SANDBOX_WAKE_JOBS_GC_RETENTION_SECS", Some("600"), || {
            let cfg = WakeLifecycleConfig::from_env().unwrap();
            assert_eq!(cfg.wake_jobs_gc_retention_secs, 600);
        });
    }

    #[test]
    fn empty_retention_falls_through_to_default() {
        with_env("SANDBOX_WAKE_JOBS_GC_RETENTION_SECS", Some(""), || {
            let cfg = WakeLifecycleConfig::from_env().unwrap();
            assert_eq!(
                cfg.wake_jobs_gc_retention_secs,
                WakeLifecycleConfig::DEFAULT_GC_RETENTION_SECS
            );
        });
    }

    #[test]
    fn unparseable_retention_rejected() {
        with_env("SANDBOX_WAKE_JOBS_GC_RETENTION_SECS", Some("five"), || {
            let err = WakeLifecycleConfig::from_env()
                .expect_err("garbage value must fail-closed");
            assert!(err.contains("five"), "error must mention value: {err}");
        });
    }

    #[test]
    fn zero_retention_rejected() {
        with_env("SANDBOX_WAKE_JOBS_GC_RETENTION_SECS", Some("0"), || {
            let err = WakeLifecycleConfig::from_env().expect_err("0 < minimum");
            assert!(err.contains("minimum") || err.contains("polling"));
        });
    }

    #[test]
    fn default_struct_matches_env_default() {
        // Confirms the `Default` impl agrees with the env-default
        // path (so callers that use `WakeLifecycleConfig::default()`
        // get the same retention as the env-unset path).
        let d = WakeLifecycleConfig::default();
        assert_eq!(d.wake_jobs_gc_retention_secs, 300);
        // R19-C1: takeover threshold default matches the env-default
        // path too.
        assert_eq!(d.takeover_threshold_secs, 60);
    }

    // ─── R19-C1: takeover_threshold_secs env parsing ──────────────

    #[test]
    fn default_takeover_threshold_when_unset() {
        with_env("SANDBOX_WAKE_JOBS_TAKEOVER_THRESHOLD_SECS", None, || {
            let cfg = WakeLifecycleConfig::from_env().unwrap();
            assert_eq!(
                cfg.takeover_threshold_secs,
                WakeLifecycleConfig::DEFAULT_TAKEOVER_THRESHOLD_SECS
            );
            assert_eq!(cfg.takeover_threshold_secs, 60);
        });
    }

    #[test]
    fn explicit_takeover_threshold_accepted() {
        with_env(
            "SANDBOX_WAKE_JOBS_TAKEOVER_THRESHOLD_SECS",
            Some("120"),
            || {
                let cfg = WakeLifecycleConfig::from_env().unwrap();
                assert_eq!(cfg.takeover_threshold_secs, 120);
            },
        );
    }

    #[test]
    fn takeover_threshold_below_minimum_rejected() {
        with_env(
            "SANDBOX_WAKE_JOBS_TAKEOVER_THRESHOLD_SECS",
            Some("10"),
            || {
                let err = WakeLifecycleConfig::from_env()
                    .expect_err("10s threshold must fail (< MIN)");
                assert!(
                    err.contains("30") || err.contains("ladder"),
                    "error must mention the floor or rationale; got: {err}"
                );
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_nomad_cfg() -> NomadCHConfig {
        NomadCHConfig {
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
        }
    }

    #[test]
    fn vm_index_ceil_validation_rejects_overflow() {
        let mut cfg = base_nomad_cfg();
        cfg.vm_index_ceil = 200; // 100+200=300, overflows u8
        let err = cfg.validate().expect_err("must reject");
        assert!(
            err.contains("VM_INDEX_CEIL") && err.contains("overflow"),
            "{err}",
        );
    }

    #[test]
    fn vm_index_ceil_validation_accepts_max_safe() {
        let mut cfg = base_nomad_cfg();
        cfg.vm_index_ceil = 155;
        cfg.validate().expect("155 must be accepted");
    }

    #[test]
    fn vm_index_ceil_validation_rejects_floor_above_ceil() {
        let mut cfg = base_nomad_cfg();
        cfg.vm_index_floor = 100;
        cfg.vm_index_ceil = 50;
        let err = cfg.validate().expect_err("must reject");
        assert!(err.contains("FLOOR") && err.contains("CEIL"), "{err}");
    }

    #[test]
    fn validate_rejects_floor_zero() {
        let mut cfg = base_nomad_cfg();
        cfg.vm_index_floor = 0;
        let err = cfg.validate().expect_err("must reject");
        assert!(err.contains("FLOOR"), "{err}");
    }

    #[test]
    fn validate_rejects_bad_nomad_url() {
        let mut cfg = base_nomad_cfg();
        cfg.nomad_addr = "".into();
        assert!(cfg.validate().is_err());
        cfg.nomad_addr = "127.0.0.1:4646".into();
        let err = cfg.validate().expect_err("must reject");
        assert!(err.contains("NOMAD_ADDR"), "{err}");
    }

    // ─── r27-S1 Guard A: nomad_addr loopback enforcement ──────────────

    /// Canonical IPv4 loopback. The base fixture already uses this
    /// shape, so a passing fixture implies acceptance, but pin it
    /// explicitly so a refactor that flips the default doesn't
    /// silently regress the boot-time gate.
    #[test]
    fn nomad_addr_loopback_127001_accepted() {
        let mut cfg = base_nomad_cfg();
        cfg.nomad_addr = "http://127.0.0.1:4646".into();
        cfg.validate().expect("127.0.0.1 must be accepted");
    }

    /// Any IPv4 in 127.0.0.0/8 — operators sometimes bind agents on
    /// alternate loopback aliases (e.g. 127.0.0.2) for multi-agent
    /// testbeds. `is_loopback()` covers the full /8 per RFC 1122.
    #[test]
    fn nomad_addr_loopback_127_x_x_x_accepted() {
        let mut cfg = base_nomad_cfg();
        cfg.nomad_addr = "http://127.0.0.2:4646".into();
        cfg.validate().expect("127.0.0.2 must be accepted (RFC 1122 /8)");
    }

    /// IPv6 loopback `::1` in bracketed form per RFC 3986 § 3.2.2.
    #[test]
    fn nomad_addr_loopback_ipv6_accepted() {
        let mut cfg = base_nomad_cfg();
        cfg.nomad_addr = "http://[::1]:4646".into();
        cfg.validate().expect("[::1] must be accepted");
    }

    /// `localhost` is the documented operator-friendly default; we
    /// accept it without `getaddrinfo` (see fn rustdoc for why).
    #[test]
    fn nomad_addr_localhost_accepted() {
        let mut cfg = base_nomad_cfg();
        cfg.nomad_addr = "http://localhost:4646".into();
        cfg.validate().expect("localhost must be accepted");
    }

    /// Mixed-case `LocalHost` — operators paste from various sources;
    /// the comparison is ASCII-case-insensitive.
    #[test]
    fn nomad_addr_localhost_mixed_case_accepted() {
        let mut cfg = base_nomad_cfg();
        cfg.nomad_addr = "http://LocalHost:4646".into();
        cfg.validate().expect("LocalHost must be accepted");
    }

    /// Remote DNS name — the main misconfig vector. Refuse boot
    /// rather than emit Constraints pinning to a remote node_id.
    #[test]
    fn nomad_addr_remote_rejected() {
        let mut cfg = base_nomad_cfg();
        cfg.nomad_addr = "https://nomad.example.com:4646".into();
        let err = cfg.validate().expect_err("remote DNS must be rejected");
        assert!(
            err.contains("not a loopback") || err.contains("not loopback"),
            "expected loopback-refusal text; got: {err}",
        );
        assert!(err.contains("r27-S1"), "expected r27-S1 marker; got: {err}");
    }

    /// IPv4 in private RFC1918 range but NOT loopback. A common
    /// "I'll just point at the LAN Nomad" misconfig.
    #[test]
    fn nomad_addr_ipv4_non_loopback_rejected() {
        let mut cfg = base_nomad_cfg();
        cfg.nomad_addr = "http://10.0.0.5:4646".into();
        let err = cfg.validate().expect_err("10.0.0.5 must be rejected");
        assert!(
            err.contains("not loopback") || err.contains("not a loopback"),
            "expected loopback-refusal text; got: {err}",
        );
    }

    /// IPv6 non-loopback (a public address with the documentation
    /// prefix 2001:db8::/32). Bracket-stripping must extract the
    /// host correctly so the IP parse runs against `2001:db8::1`,
    /// not against `[2001:db8::1]:4646`.
    #[test]
    fn nomad_addr_ipv6_non_loopback_rejected() {
        let mut cfg = base_nomad_cfg();
        cfg.nomad_addr = "http://[2001:db8::1]:4646".into();
        let err = cfg.validate().expect_err("2001:db8::1 must be rejected");
        assert!(
            err.contains("not loopback") || err.contains("not a loopback"),
            "expected loopback-refusal text; got: {err}",
        );
    }

    /// Loopback host with NO port and NO path — minimal valid shape.
    /// Path-extraction must terminate the host at end-of-string.
    #[test]
    fn nomad_addr_loopback_no_port_accepted() {
        let mut cfg = base_nomad_cfg();
        cfg.nomad_addr = "http://127.0.0.1".into();
        cfg.validate().expect("127.0.0.1 (no port) must be accepted");
    }

    /// Loopback host with a trailing path — the host body ends at
    /// the first `/`, NOT at end-of-string.
    #[test]
    fn nomad_addr_loopback_with_path_accepted() {
        let mut cfg = base_nomad_cfg();
        cfg.nomad_addr = "http://127.0.0.1:4646/v1".into();
        cfg.validate().expect("loopback + path must be accepted");
    }

    #[test]
    fn validate_accepts_canonical_user_home_layout() {
        let mut cfg = base_nomad_cfg();
        // `/var/zeroship/ch` + `users` == `/var/zeroship/ch/users`.
        cfg.validate().expect("default layout must validate");
    }

    #[test]
    fn validate_trims_trailing_slash_on_nomad_addr() {
        // M2: a trailing slash on the env-supplied URL would produce
        // `format!("{nomad_addr}/v1/...")` → `http://...//v1/...`,
        // which Nomad's HTTP server returns 404 for. Trim a single
        // trailing slash in `validate()`.
        let mut cfg = base_nomad_cfg();
        cfg.nomad_addr = "http://127.0.0.1:4646/".to_string();
        cfg.validate().expect("trailing slash should be tolerated");
        assert_eq!(cfg.nomad_addr, "http://127.0.0.1:4646");
    }

    #[test]
    fn validate_accepts_disjoint_user_home_root() {
        let mut cfg = base_nomad_cfg();
        cfg.user_home_dir_root = PathBuf::from("/srv/zeroship-homes");
        cfg.validate()
            .expect("disjoint user_home_dir_root must validate");
    }

    #[test]
    fn validate_rejects_loopback_subnet_octet() {
        // M6: 127 lands on the loopback /8 — a typo'd second octet
        // would otherwise give every sandbox a guaranteed-
        // unreachable IP and the controller would only notice on
        // the first agent /livez timeout.
        let mut cfg = base_nomad_cfg();
        cfg.subnet_second_octet = 127;
        let err = cfg.validate().expect_err("must reject 127");
        assert!(err.contains("loopback"), "{err}");
    }

    #[test]
    fn validate_rejects_link_local_subnet_octet() {
        let mut cfg = base_nomad_cfg();
        cfg.subnet_second_octet = 169;
        let err = cfg.validate().expect_err("must reject 169");
        assert!(err.contains("link-local"), "{err}");
    }

    #[test]
    fn validate_rejects_multicast_subnet_octet() {
        let mut cfg = base_nomad_cfg();
        cfg.subnet_second_octet = 230;
        let err = cfg.validate().expect_err("must reject 230");
        assert!(err.contains("multicast"), "{err}");
    }

    #[test]
    fn validate_accepts_default_99_subnet_octet() {
        let mut cfg = base_nomad_cfg();
        cfg.subnet_second_octet = 99;
        cfg.validate().expect("default 99 must validate");
    }

    #[test]
    fn validate_accepts_alternate_private_subnet_octet() {
        // E.g. operator with corp 10.99/16 collision wants 10.50/16.
        let mut cfg = base_nomad_cfg();
        cfg.subnet_second_octet = 50;
        cfg.validate().expect("50 must validate");
    }

    #[test]
    fn validate_rejects_descendant_user_home_root() {
        let mut cfg = base_nomad_cfg();
        // Descendant of host_state_dir but not the canonical `users`.
        cfg.user_home_dir_root = PathBuf::from("/var/zeroship/ch/homes");
        let err = cfg.validate().expect_err("must reject");
        assert!(err.contains("USER_HOME_ROOT"), "{err}");
    }

    /// A7 (deferred): `with_token` is the only legal out-of-crate
    /// write path to `SandboxConfig.token`. Verify it replaces the
    /// prior value. Mirrors the field-setter test pattern from A6b's
    /// `with_config_replaces_existing` — build a fixture with one
    /// token, swap to a second, assert the second wins.
    ///
    /// `ApiToken` has no `PartialEq` (zeroize-wrapped string) so we
    /// compare via `as_bytes()`, which is the same surface
    /// `auth::check_token` uses on the hot path.
    #[test]
    fn with_token_replaces_existing() {
        let first = SandboxConfig::new_fixture()
            .with_token(ApiToken::new("first-token-aaaaaaaaaaaaaaaaaaaaaaaa"));
        assert_eq!(
            first.token.as_bytes(),
            b"first-token-aaaaaaaaaaaaaaaaaaaaaaaa",
            "fixture starts with the first token"
        );

        let second = first
            .with_token(ApiToken::new("second-token-bbbbbbbbbbbbbbbbbbbbbbbb"));
        assert_eq!(
            second.token.as_bytes(),
            b"second-token-bbbbbbbbbbbbbbbbbbbbbbbb",
            "with_token must replace the prior ApiToken"
        );
    }

    /// virtio-blk pivot (bug #11): SANDBOX_WORKSPACE_IMAGE_SIZE_GB=0
    /// is a foot-gun (mkfs.ext4 against a 0-byte sparse file aborts);
    /// from_env refuses to start in that state. Mutex over the global
    /// env table makes this test serializable with the other from_env
    /// tests; we set + unset the var locally.
    #[test]
    fn workspace_image_size_gb_zero_is_rejected_at_startup() {
        // Serialize against other env-mutating tests in this module by
        // taking the same mutex pattern used elsewhere if present;
        // since there's no shared mutex here we rely on the
        // SANDBOX_TOKEN+ALLOW_NO_AUTH cofiguration also being set so
        // the validator gets far enough to evaluate the image-size
        // field.
        std::env::set_var("SANDBOX_TOKEN", "x".repeat(32));
        std::env::set_var("SANDBOX_WORKSPACE_IMAGE_SIZE_GB", "0");
        let err = SandboxConfig::from_env().expect_err("zero must reject");
        assert!(
            err.contains("WORKSPACE_IMAGE_SIZE_GB"),
            "expected WORKSPACE_IMAGE_SIZE_GB in error; got: {err}"
        );
        std::env::remove_var("SANDBOX_WORKSPACE_IMAGE_SIZE_GB");
        std::env::remove_var("SANDBOX_TOKEN");
    }
}
