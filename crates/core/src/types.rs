use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zeroship_bundle::Manifest;

/// A registered application record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppRecord {
    pub id: Uuid,
    pub name: String,
    pub plan_id: String,
    pub deploy_hash: Option<String>,
    #[serde(skip_serializing)]
    pub api_key: String,
    pub created_at: String,
    pub updated_at: String,
}

/// Worker-facing runtime limits for a specific app.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct AppRuntimeLimits {
    pub cpu_limit_ms: Option<u64>,
    pub wall_timeout_ms: Option<u64>,
    /// Maximum V8 heap in megabytes. `None` → 128 MB default in the worker.
    /// Free-tier apps should be capped low (64 MB); paid tiers can go higher.
    pub heap_limit_mb: Option<u32>,
}

/// Worker-facing metadata for an app version/config snapshot.
///
/// `PartialEq`/`Eq` are intentionally NOT derived: `manifest`'s recursive
/// types (`CacheCtl`, `RateLimit`, `AssetEntry`, …) don't carry them, and
/// the worker's hot path only reads individual fields (no whole-struct
/// comparison). Add them only when a caller actually needs `==`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppVersionInfo {
    pub deploy_hash: Option<String>,
    pub plan_id: String,
    pub runtime: AppRuntimeLimits,
    /// Monotonic counter bumped on every var/secret mutation. Workers
    /// compare local vs remote and refetch env when they differ —
    /// closes the "rotated secret stays stale until next deploy" gap.
    /// `0` for a freshly-created app with no env mutations.
    #[serde(default)]
    pub env_version: i64,
    /// Per-app routing manifest. Carried inline on every `/internal/versions`
    /// poll so workers can resolve the worker-bundle blob hash
    /// (`manifest.worker.modules[manifest.worker.entry]`) without an extra
    /// round trip. `None` for apps that have not deployed yet (NULL
    /// `manifest_json` row); SSG-only deploys still carry a manifest with
    /// `worker = None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<Manifest>,
}

/// A routing entry resolved from an incoming request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteEntry {
    pub name: String,
    pub plan_id: String,
    pub api_key_hash: String,
    pub deploy_hash: Option<String>,
    /// Per-app routing manifest. Always present — apps that haven't
    /// shipped a manifest get [`Manifest::passthrough`] synthesized at
    /// load time, which preserves "POST /_rpc/* → RPC, everything else →
    /// SSR" semantics.
    #[serde(default = "Manifest::passthrough")]
    pub manifest: Manifest,
    /// Per-app OAuth `client_id`, resolved from the request `Host` via
    /// the route cache. The gateway's browser-auth endpoints and the
    /// Bearer arm bind on this (`claims.client_id == oauth_client_id`).
    ///
    /// `Option`, never a defaulted empty `String`: an empty-string
    /// `client_id` is a footgun (a malformed token with an empty
    /// `client_id` claim could match `""`). It is `None` until the
    /// control plane provisions the app's OAuth client; consumers
    /// hard-fail (`503`/`401`) rather than bind to a falsy value.
    /// `#[serde(default)]` ⇒ `None` so an un-provisioned `RouteEntry`
    /// stays loadable.
    #[serde(default)]
    pub oauth_client_id: Option<String>,
    /// Per-app apex origin used for pairwise/relay subject scoping
    /// (`derive_pairwise(sub, sector_identifier)`). `Option`: `None`
    /// until provisioned, in which case the pairwise projection
    /// hard-fails closed (no `pws_` derivation, no header emitted).
    #[serde(default)]
    pub sector_identifier: Option<String>,
}

/// Map of app id → current deploy/config snapshot.
pub type VersionMap = HashMap<Uuid, AppVersionInfo>;

/// Map of app id → route entry for fast lookup.
pub type RouteMap = HashMap<Uuid, RouteEntry>;

/// Usage counters reported by a worker to the control plane.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageReport {
    pub worker_id: String,
    pub counters: HashMap<Uuid, AppUsage>,
}

/// Per-application usage counters for a billing interval.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppUsage {
    pub requests: u64,
    pub cpu_us: u64,
    pub wall_us: u64,
    pub egress_bytes: u64,
    pub ingress_bytes: u64,
}

/// Events emitted by the control plane to workers/gates.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ControlEvent {
    Deploy { app_id: Uuid, hash: String },
    Delete { app_id: Uuid },
    PlanChange { app_id: Uuid, plan_id: String },
}

/// Canonical errors for zeroship-common operations.
#[derive(Debug, thiserror::Error)]
pub enum CommonError {
    #[error("not found")]
    NotFound,
    #[error("already exists")]
    AlreadyExists,
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("unauthorized")]
    Unauthorized,
    #[error("internal error: {0}")]
    Internal(String),
}
