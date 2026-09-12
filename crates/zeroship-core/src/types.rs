use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use zeroship_bundle::Manifest;

use crate::{AppId, UserId};

/// A registered application record.
///
/// It carries NO app-level API key, and there is no field withheld from its
/// serialized form - the record a caller receives is the whole record, which is
/// why it round-trips through `serde_json`. The reasoning for having no such key
/// at all is in `db/migrations-ts/20260905000200_drop_app_api_key.ts`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppRecord {
    pub id: AppId,
    pub name: String,
    pub plan_id: String,
    pub deploy_hash: Option<String>,
    pub archived_at: Option<String>,
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
/// Used by the control-plane catalog seed (`plan_catalog::builtin_plans`
/// free tier) and the registry's fallback for an app whose plan row is missing
/// or whose `runtime_limits_json` fails to parse. Keeping ONE const stops the
/// two copies from drifting (50ms CPU / 5s wall / 64MB heap). The worker
/// therefore never gets `(None, None, None)` (unbounded) for an unpriced app.
pub const FREE_TIER_RUNTIME_LIMITS: AppRuntimeLimits = AppRuntimeLimits {
    cpu_limit_ms: Some(50),
    wall_timeout_ms: Some(5_000),
    heap_limit_mb: Some(64),
};

/// Worker-facing raw TCP egress policy for creator isolates.
///
/// Empty `egress` means `Denied`. Non-empty means the worker may build a rule
/// set from the supplied entries and plan-level caps. `Trusted` is intentionally
/// not representable on this wire type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct AppNetPolicy {
    pub egress: Vec<NetEgressEntry>,
    pub max_sockets: u32,
    pub egress_ceiling_bytes: u64,
}

/// One egress rule as it crosses the wire.
///
/// `destination` is TEXT and the kind is INFERRED from its grammar, exactly as
/// the creator-facing API infers it: a value containing `/` must parse as a
/// CIDR, anything else must parse as an exact DNS name. Carrying the parsed
/// form here would let a hand-edited row skip validation on deserialization -
/// the worker re-runs `EgressRule::parse` on every entry it loads, which is the
/// property this shape exists to keep.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetEgressEntry {
    pub verdict: crate::net_policy::Verdict,
    pub destination: String,
    pub port: u16,
}

/// Plan-catalog tier limits for creator outbound raw TCP.
///
/// Hosts live in `app_net_grants`; plans only determine "how much". The
/// creator chooses WHICH hosts within these caps and can never raise them.
///
/// `max_grants` bounds the NUMBER of `app_net_grants` rows an app may hold. It
/// exists because the grant author is the creator: `max_sockets` and
/// `egress_ceiling_bytes` bound concurrency and volume, and neither bounds how
/// wide a destination set a creator can enumerate one exact host at a time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppNetPolicyLimits {
    pub max_sockets: u32,
    pub egress_ceiling_bytes: u64,
    /// Absent in a plan-catalog row written before this field existed; the
    /// free-tier value is the fail-closed default, never "unbounded".
    ///
    /// This serde default is the ONLY thing that supplies the key. The
    /// `zeroship.plans.net_policy_limits_json` column default
    /// (`20260702000400`) still carries just `max_sockets` +
    /// `egress_ceiling_bytes`, and that is deliberate: every reader of the
    /// column goes through this type (`plan_catalog.rs`, `registry.rs`,
    /// `net_grants.rs`), so a row missing the key and a row carrying
    /// `max_grants: 10` deserialize identically. Adding it to the column
    /// default is unobservable.
    ///
    /// A migration to add it was written and then DELETED rather than
    /// rewritten: the engine cannot lower a JSON-object `setColumnDefault`
    /// ("json value defaults need live column type"). Do not re-add one - there
    /// is nothing to buy.
    #[serde(default = "free_tier_max_grants")]
    pub max_grants: u32,
}

const fn free_tier_max_grants() -> u32 {
    FREE_TIER_NET_POLICY_LIMITS.max_grants
}

impl Default for AppNetPolicyLimits {
    fn default() -> Self {
        FREE_TIER_NET_POLICY_LIMITS
    }
}

pub const FREE_TIER_NET_POLICY_LIMITS: AppNetPolicyLimits = AppNetPolicyLimits {
    max_sockets: 4,
    egress_ceiling_bytes: 10 * 1024 * 1024,
    max_grants: 10,
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
    /// Creator outbound raw-TCP policy produced by the control plane.
    /// Defaults to `Denied` when absent. The worker can only translate this
    /// into `Denied` or a reviewed `Allowlist`; `Trusted` is reserved for
    /// operator-internal runtimes.
    #[serde(default)]
    pub net_policy: AppNetPolicy,
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

/// Per-organization payment/account-enforcement state (billing G2), derived by the
/// control plane from Stripe webhook truth and projected onto the gateway's
/// pulled [`RouteEntry`] (organization-keyed in the DB, surfaced per-app via the
/// owner join).
///
/// This is ORTHOGONAL to [`SpendState`]: spend caps USAGE within a paid
/// relationship; account state is the payment gate on the relationship itself.
/// They compose as an AND at the gateway — a request is served iff
/// `account_state ∈ {Active, PastDue}` AND `spend_state != Block`.
///
/// - `Active` — payment current; served (subject to spend).
/// - `PastDue` — ≥1 infra invoice failed and Stripe's retries are running; the
///   customer-favourable GRACE window — STILL SERVED (not blocked), just the
///   warning state.
/// - `Suspended` — the dunning window elapsed; the gateway rejects new requests
///   with 402 `ACCOUNT_SUSPENDED` before any worker proxy. Reversible: a later
///   recovered payment flips back to `Active`.
///
/// Serde is `snake_case` so the TEXT column / wire form is `"active"` …
/// `"suspended"`. `Default` is `Active` — a `RouteEntry` with no status row
/// (free/cardless apps, the common case) is unrestricted, and the
/// `#[serde(default)]` on the field keeps a wire payload predating the field
/// loadable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountState {
    #[default]
    Active,
    PastDue,
    Suspended,
}

/// A routing entry resolved from an incoming request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteEntry {
    pub name: String,
    pub plan_id: String,
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
    /// Per-app apex origin used by auth access-token issuance and gateway
    /// browser-session minting for pairwise subject scoping. `Option`: `None`
    /// until the OAuth client is fully provisioned; minting then fails closed.
    #[serde(default)]
    pub sector_identifier: Option<String>,
    /// Current spend-enforcement state for the app, JOINed from
    /// `zeroship.app_spend_state` by the control-plane registry. The gateway
    /// gates on this BEFORE rate-limiting/dispatch (Block → 402, Degrade →
    /// throttle, Warn → header, Allow → pass). `#[serde(default)]` ⇒ `Allow`
    /// for an app with no spend row or a wire payload predating the field.
    #[serde(default)]
    pub spend_state: SpendState,
    /// Current payment/account-enforcement state for the app's ORGANIZATION
    /// (billing G2), JOINed from `zeroship.organization_billing_status` by the
    /// control-plane registry on `apps.organization_id`.
    ///
    /// It used to be reached through the app's project, that project's
    /// organization, and that organization's OWNERS - which fanned out, so the
    /// registry had to collapse the result most-restrictive-first or seating an
    /// owner with a good card could un-suspend an app. An organization has one
    /// status row, so there is no fan-out and no collapse. The
    /// gateway gates on this BEFORE spend (an outer AND): `Suspended` → 402
    /// `ACCOUNT_SUSPENDED`; `PastDue`/`Active` pass (PastDue is the grace
    /// window). `#[serde(default)]` ⇒ `Active` for an app whose organization has no
    /// status row (free/cardless, the common case) or a wire payload predating
    /// the field.
    #[serde(default)]
    pub account_state: AccountState,
}

/// Map of app id → current deploy/config snapshot.
pub type VersionMap = HashMap<AppId, AppVersionInfo>;

/// Map of app id → route entry for fast lookup.
pub type RouteMap = HashMap<AppId, RouteEntry>;

/// Authentication lifecycle state pushed to gateways with the route table.
///
/// Gateways validate app credentials offline, so this is the control-plane
/// answer to account erasure and administrative disablement without adding a
/// database lookup to every request. `locked_until` is deliberately absent:
/// anonymous failed-login attempts can set it, so using it to invalidate live
/// credentials would let an attacker force-log-out another user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayPrincipalLifecycle {
    pub user_id: UserId,
    pub disabled: bool,
    pub anonymized: bool,
    pub deletion_requested: bool,
    pub deletion_scheduled: bool,
    /// Pairwise subjects already issued for this principal. Persisted mappings
    /// keep denials valid after a route is removed and avoid a users-by-routes
    /// expansion at every gateway pull.
    pub pairwise_subjects: Vec<String>,
}

impl GatewayPrincipalLifecycle {
    #[must_use]
    pub fn blocks_authentication(&self) -> bool {
        self.disabled || self.anonymized || self.deletion_requested || self.deletion_scheduled
    }

    #[must_use]
    pub fn disabled(user_id: UserId, pairwise_subjects: Vec<String>) -> Self {
        Self {
            user_id,
            disabled: true,
            anonymized: false,
            deletion_requested: false,
            deletion_scheduled: false,
            pairwise_subjects,
        }
    }

    #[must_use]
    pub fn anonymized(user_id: UserId, pairwise_subjects: Vec<String>) -> Self {
        Self {
            user_id,
            disabled: false,
            anonymized: true,
            deletion_requested: false,
            deletion_scheduled: false,
            pairwise_subjects,
        }
    }

    #[must_use]
    pub fn deletion_requested(user_id: UserId, pairwise_subjects: Vec<String>) -> Self {
        Self {
            user_id,
            disabled: false,
            anonymized: false,
            deletion_requested: true,
            deletion_scheduled: false,
            pairwise_subjects,
        }
    }

    #[must_use]
    pub fn deletion_scheduled(user_id: UserId, pairwise_subjects: Vec<String>) -> Self {
        Self {
            user_id,
            disabled: false,
            anonymized: false,
            deletion_requested: false,
            deletion_scheduled: true,
            pairwise_subjects,
        }
    }
}

/// Durable token-family cutoff pushed to gateways with the route table.
/// A credential is rejected when its whole-second `iat` predates this value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayFamilyRevocation {
    pub client_id: String,
    pub subject: String,
    pub revoked_after: i64,
}

/// Complete gateway pull payload. The lifecycle field is required on the wire:
/// accepting a route-only response would silently turn account invalidation
/// off while still advancing route-sync freshness.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewaySnapshot {
    pub routes: RouteMap,
    pub principal_lifecycle: Vec<GatewayPrincipalLifecycle>,
    pub family_revocations: Vec<GatewayFamilyRevocation>,
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
    Deploy {
        app_id: AppId,
        hash: String,
    },
    Delete {
        app_id: AppId,
    },
    PlanChange {
        app_id: AppId,
        plan_id: String,
    },
    /// A spend-state transition for an app, emitted by the spend-reconcile
    /// cron on each tick that changes an app's [`SpendState`]. Per decision
    /// D1 this is for the audit log / future SSE fan-out ONLY — there is no
    /// live `ControlEvent` delivery path today; enforcement rides the pulled
    /// [`RouteEntry::spend_state`], not this event.
    SpendState {
        app_id: AppId,
        state: SpendState,
    },
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
