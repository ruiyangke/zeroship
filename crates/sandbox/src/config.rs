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
    pub token: ApiToken,

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
/// pre-provisioned out-of-band — see `docs/sandbox/nomad-ch.md` for
/// the host setup runbook.
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
    /// after `POST /v1/jobs`. Past this we give up and the
    /// `CreateGuard` tears the job down. `SANDBOX_NOMAD_CH_READY_TIMEOUT_SECS`
    /// (default 60). Includes both Nomad scheduling latency and the
    /// wrapper's CH boot time; should be a few × the typical CH boot
    /// (~3-4 s on a healthy host).
    pub ready_timeout_secs: u64,

    /// On controller startup, list every Nomad job whose `ID` starts
    /// with the `zsbx-` prefix and stop+purge it. Same trade-off as
    /// the K8s flag: useful for single-replica deployments to
    /// recover after a crash; **dangerous in HA** because the first
    /// replica nukes every other replica's active sandboxes on
    /// rolling restart. Default off; opt-in for single-node operators.
    /// `SANDBOX_NOMAD_CH_STARTUP_ORPHAN_CLEANUP` (default `false`).
    pub startup_orphan_cleanup: bool,
}

impl NomadCHConfig {
    /// Validate the parsed config. Called from
    /// [`SandboxConfig::from_env`] before the backend is instantiated
    /// so misconfig surfaces at startup, not on the first sandbox.
    pub fn validate(&self) -> Result<(), String> {
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
        if (100u32 + self.vm_index_ceil as u32) > 255 {
            return Err(format!(
                "SANDBOX_NOMAD_CH_VM_INDEX_CEIL ({}) would overflow IP \
                 third octet (10.99.{}.2). Max is 155.",
                self.vm_index_ceil,
                100 + self.vm_index_ceil as u32
            ));
        }
        Ok(())
    }
}

impl SandboxConfig {
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
            ready_timeout_secs: parse_env("SANDBOX_NOMAD_CH_READY_TIMEOUT_SECS", 60u64)?,
            startup_orphan_cleanup: parse_env(
                "SANDBOX_NOMAD_CH_STARTUP_ORPHAN_CLEANUP",
                false,
            )?,
        };

        nomad_ch.validate()?;

        Ok(Self {
            port, token, backend, image, workspace_root, network,
            memory_mb, cpus, idle_timeout_secs, max_lifetime_secs, auto_pull,
            k8s, nomad_ch,
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
            ready_timeout_secs: 60,
            startup_orphan_cleanup: false,
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
}
