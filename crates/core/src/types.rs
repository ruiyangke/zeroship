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

/// Conservative free-tier runtime limits — the single shared source of truth.
///
/// Used by the control-plane catalog seed (`bootstrap_console::builtin_plans`
/// free tier) and the registry's fallback for an app whose plan row is missing
/// or whose `runtime_limits_json` fails to parse. Keeping ONE const stops the
/// two copies from drifting (50ms CPU / 5s wall / 64MB heap). The worker
/// therefore never gets `(None, None, None)` (unbounded) for an unpriced app.
pub const FREE_TIER_RUNTIME_LIMITS: AppRuntimeLimits = AppRuntimeLimits {
    cpu_limit_ms: Some(50),
    wall_timeout_ms: Some(5_000),
    heap_limit_mb: Some(64),
};

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

/// Per-app spend-enforcement state, derived by the control-plane spend engine
/// from period usage vs the app's effective spend limit and carried to the
/// gateway on the pulled [`RouteEntry`].
///
/// Ordered most- to least-permissive so the gateway gate is a simple match:
/// - `Allow` — under the warn threshold; serve normally.
/// - `Warn` — past ~80%; serve but stamp an `x-zs-spend-warn` header.
/// - `Degrade` — past the soft cap; serve but throttle the app's effective
///   concurrency + rate (gateway-side, no bucket rebuild).
/// - `Block` — at/over the hard cap; reject new requests with 402 before any
///   worker proxy.
///
/// Serde is `snake_case` so the TEXT column / wire form is `"allow"` …
/// `"block"`. `Default` is `Allow` — a `RouteEntry` with no spend row (the
/// common case) is unrestricted, and the `#[serde(default)]` on the field
/// keeps forward-loading a wire payload without the field tolerant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpendState {
    #[default]
    Allow,
    Warn,
    Degrade,
    Block,
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
    /// Current spend-enforcement state for the app, JOINed from
    /// `zeroship.app_spend_state` by the control-plane registry. The gateway
    /// gates on this BEFORE rate-limiting/dispatch (Block → 402, Degrade →
    /// throttle, Warn → header, Allow → pass). `#[serde(default)]` ⇒ `Allow`
    /// for an app with no spend row or a wire payload predating the field.
    #[serde(default)]
    pub spend_state: SpendState,
}

/// Map of app id → current deploy/config snapshot.
pub type VersionMap = HashMap<Uuid, AppVersionInfo>;

/// Map of app id → route entry for fast lookup.
pub type RouteMap = HashMap<Uuid, RouteEntry>;

/// Usage counters reported by a worker to the control plane.
///
/// The producer (worker meter flush task) emits these at-least-once: a POST
/// that times out is retried on the next flush tick, so the SAME report can
/// arrive at control twice. Idempotent ingest is keyed on
/// `(worker_id, sequence)`:
///
/// - `worker_id` identifies the emitting worker process.
/// - `sequence` is a monotonic per-`worker_id` counter, bumped once per
///   successfully-built report. Control dedups on `(worker_id, sequence)`
///   via `zeroship.usage_reports_seen`, so a duplicate is a no-op.
/// - `report_id` is a fresh uuidv7 per report — a human/trace-friendly id for
///   logs and a tie-breaker; the dedup key proper is `(worker_id, sequence)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageReport {
    pub worker_id: String,
    /// Fresh uuidv7 per report (trace id / tie-breaker).
    pub report_id: Uuid,
    /// Monotonic per-`worker_id` sequence; the dedup key with `worker_id`.
    pub sequence: u64,
    pub counters: HashMap<Uuid, AppUsage>,
}

/// Per-application usage counters for a billing interval.
///
/// The five fixed fields are the platform counters every app is billed on.
/// `custom` carries platform-emitted resource metrics from the trusted data
/// primitives (`db_reads`, `kv_writes`, `storage_ops`, …) — NOT SDK or app
/// self-reported counters (there is no `env.meter` API; the billing signal
/// is platform-measured). New metrics flow without a wire change per metric.
/// Empty `custom` is omitted from the JSON.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppUsage {
    pub requests: u64,
    pub cpu_us: u64,
    pub wall_us: u64,
    pub egress_bytes: u64,
    pub ingress_bytes: u64,
    /// Platform-emitted resource metrics from the trusted db/kv/storage
    /// primitives, keyed by metric name. Reserved names (the five fixed
    /// fields above) must not appear here; the producer keeps them separate.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub custom: HashMap<String, u64>,
}

/// Events emitted by the control plane to workers/gates.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ControlEvent {
    Deploy { app_id: Uuid, hash: String },
    Delete { app_id: Uuid },
    PlanChange { app_id: Uuid, plan_id: String },
    /// A spend-state transition for an app, emitted by the spend-reconcile
    /// cron on each tick that changes an app's [`SpendState`]. Per decision
    /// D1 this is for the audit log / future SSE fan-out ONLY — there is no
    /// live `ControlEvent` delivery path today; enforcement rides the pulled
    /// [`RouteEntry::spend_state`], not this event.
    SpendState { app_id: Uuid, state: SpendState },
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
