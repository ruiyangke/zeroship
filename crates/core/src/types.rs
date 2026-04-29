use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
}

// ---------------------------------------------------------------------------
// Routing manifest — the per-app dispatch table the gateway walks.
//
// Lives alongside the app's deploy in the control plane. Synced to the
// gateway with the rest of the route data. The build adapter
// (@zeroship/vite-plugin) emits this from the project's vite config
// + framework conventions.
// ---------------------------------------------------------------------------

/// One per app. Carries everything the gateway needs to route a request
/// without consulting the control plane on the hot path.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Manifest {
    /// Ordered routing rules. Walked first-match-wins on every
    /// request.
    #[serde(default)]
    pub rules: Vec<Rule>,

    /// Build-time static asset map. Populated on deploy; immutable
    /// until the next deploy. `path → asset entry` (path is the URL
    /// the asset is served at, e.g. `/index.html`).
    #[serde(default)]
    pub build_assets: HashMap<String, AssetEntry>,

    /// Runtime-emitted asset map. Populated by the user's server
    /// code via `zeroship.assets.put(...)`. Bumped via
    /// `asset_version` on each mutation; gateway re-syncs when the
    /// version changes.
    #[serde(default)]
    pub runtime_assets: HashMap<String, AssetEntry>,

    /// Hash of the server-only sub-bundle the worker should load.
    /// Distinct from `deploy_hash` (the whole-upload hash) so an
    /// asset-only deploy doesn't invalidate the worker's V8 isolate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_bundle_hash: Option<String>,

    /// Bumped on every `runtime_assets` mutation. Gateway compares
    /// local vs remote and refetches the runtime map only when this
    /// changes — keeps the hot path cheap.
    #[serde(default)]
    pub asset_version: i64,
}

/// A single routing rule. Match describes *when* it fires; action
/// describes *what* it does.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub r#match: Match,
    pub action: Action,
}

impl Manifest {
    /// Synthesize the default "everything goes to the worker" manifest.
    /// Preserves the kernel-cut behavior: POST /_rpc/* gets `WorkerMode::Rpc`
    /// (gateway gates the API key); everything else gets `WorkerMode::Ssr`.
    /// Used for apps that haven't yet shipped a manifest of their own.
    pub fn passthrough() -> Self {
        Self {
            rules: vec![
                Rule {
                    r#match: Match::Prefix {
                        method: Some(HttpMethod::Post),
                        path: "/_rpc/".into(),
                    },
                    action: Action::Worker {
                        mode: WorkerMode::Rpc,
                        cache: None,
                        rate_limit: None,
                    },
                },
                Rule {
                    r#match: Match::Any,
                    action: Action::Worker {
                        mode: WorkerMode::Ssr,
                        cache: None,
                        rate_limit: None,
                    },
                },
            ],
            build_assets: HashMap::new(),
            runtime_assets: HashMap::new(),
            server_bundle_hash: None,
            asset_version: 0,
        }
    }

    /// Validate manifest invariants. Returns the first violation as a
    /// human-readable message. Cheap — call on every load.
    ///
    /// Checks:
    /// * `Action::Static.status` ∈ [100, 599]
    /// * `Action::Redirect.status` ∈ [300, 399]
    /// * No rule is shadowed (made unreachable) by an earlier rule.
    pub fn validate(&self) -> Result<(), String> {
        for (i, rule) in self.rules.iter().enumerate() {
            match &rule.action {
                Action::Static { status: Some(s), .. } => {
                    if !(100..600).contains(s) {
                        return Err(format!(
                            "rule {i}: Static.status {s} out of range [100, 599]"
                        ));
                    }
                }
                Action::Redirect { status, .. } => {
                    if !(300..400).contains(status) {
                        return Err(format!(
                            "rule {i}: Redirect.status {status} must be 3xx"
                        ));
                    }
                }
                _ => {}
            }
        }
        for j in 1..self.rules.len() {
            for i in 0..j {
                if self.rules[i].shadows(&self.rules[j]) {
                    return Err(format!(
                        "rule {j} ({}) is unreachable behind rule {i} ({})",
                        rule_summary(&self.rules[j]),
                        rule_summary(&self.rules[i]),
                    ));
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Shadow detection helpers
// ---------------------------------------------------------------------------

const M_GET: u8 = 1 << 0;
const M_POST: u8 = 1 << 1;
const M_PUT: u8 = 1 << 2;
const M_PATCH: u8 = 1 << 3;
const M_DELETE: u8 = 1 << 4;
const M_HEAD: u8 = 1 << 5;
const M_OPTIONS: u8 = 1 << 6;
const M_ALL: u8 = M_GET | M_POST | M_PUT | M_PATCH | M_DELETE | M_HEAD | M_OPTIONS;
const M_GET_HEAD: u8 = M_GET | M_HEAD;

fn method_bit(m: HttpMethod) -> u8 {
    match m {
        HttpMethod::Get => M_GET,
        HttpMethod::Post => M_POST,
        HttpMethod::Put => M_PUT,
        HttpMethod::Patch => M_PATCH,
        HttpMethod::Delete => M_DELETE,
        HttpMethod::Head => M_HEAD,
        HttpMethod::Options => M_OPTIONS,
        HttpMethod::Any => M_ALL,
    }
}

/// Segment-aware prefix containment — must match `Match::Prefix` test semantics.
fn prefix_covers(prefix: &str, path: &str) -> bool {
    if let Some(bare) = prefix.strip_suffix('/') {
        path == bare || path.starts_with(prefix)
    } else {
        path == prefix || path.starts_with(&format!("{prefix}/"))
    }
}

fn rule_summary(rule: &Rule) -> String {
    let method = match rule.r#match {
        Match::Exact { method, .. }
        | Match::Prefix { method, .. }
        | Match::Glob { method, .. } => method,
        Match::Any => None,
    };
    let m_str = method.map(|m| m.as_str()).unwrap_or("*");
    match &rule.r#match {
        Match::Exact { path, .. } => format!("Exact {m_str} {path}"),
        Match::Prefix { path, .. } => format!("Prefix {m_str} {path}"),
        Match::Glob { path, .. } => format!("Glob {m_str} {path}"),
        Match::Any => "Any".to_string(),
    }
}

impl Rule {
    /// Bitset of methods this rule actually fires for.
    fn effective_methods(&self) -> u8 {
        let base = match self.r#match {
            Match::Exact { method, .. }
            | Match::Prefix { method, .. }
            | Match::Glob { method, .. } => match method {
                None | Some(HttpMethod::Any) => M_ALL,
                Some(m) => method_bit(m),
            },
            Match::Any => M_ALL,
        };
        // Tier 1 invariant: Static actions only fire for GET/HEAD (the
        // dispatcher skips them for other methods).
        if matches!(self.action, Action::Static { .. }) {
            base & M_GET_HEAD
        } else {
            base
        }
    }

    /// Does this rule's match cover every path the `other` matcher could match?
    // TODO: extend covers_path for Glob ↔ Glob and Glob ↔ Exact/Prefix
    fn covers_path(&self, other: &Match) -> bool {
        match (&self.r#match, other) {
            (Match::Any, _) => true,
            (Match::Exact { path: a, .. }, Match::Exact { path: b, .. }) => a == b,
            (Match::Prefix { path: p, .. }, Match::Exact { path: e, .. }) => prefix_covers(p, e),
            (Match::Prefix { path: p, .. }, Match::Prefix { path: q, .. }) => prefix_covers(p, q),
            _ => false,
        }
    }

    /// True if this rule makes `other` unreachable.
    fn shadows(&self, other: &Rule) -> bool {
        let s = self.effective_methods();
        let r = other.effective_methods();
        // Subset: every method bit set in r is also set in s.
        if (r & !s) != 0 {
            return false;
        }
        // If self has zero effective methods, it can't shadow anything.
        if s == 0 {
            return false;
        }
        self.covers_path(&other.r#match)
    }
}

/// How an incoming request is matched against a rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Match {
    /// Literal path equality. `O(1)` lookup.
    Exact {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        method: Option<HttpMethod>,
        path: String,
    },
    /// Literal prefix. e.g. `/_rpc/`, `/_assets/`.
    Prefix {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        method: Option<HttpMethod>,
        path: String,
    },
    /// Glob with named captures. e.g. `/blog/[slug]`, `/api/[...rest]`.
    Glob {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        method: Option<HttpMethod>,
        path: String,
    },
    /// Catch-all. Matches every request that reached this rule.
    Any,
}

/// What happens when a rule matches.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    /// Serve from `build_assets` / `runtime_assets`. The `try` chain
    /// is resolved left-to-right; first hit wins. `$path` substitutes
    /// the request path; `[name]` captures from the matching glob
    /// substitute as `[name]`.
    Static {
        r#try: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache: Option<CacheCtl>,
        /// Override response status. Common case: a final
        /// `static` rule with `try: ["/404.html"]` and `status: 404`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<u16>,
    },

    /// Forward the request to the worker. `mode` selects how the
    /// worker dispatches it.
    Worker {
        mode: WorkerMode,
        /// SSR-only: cache the worker's response. The worker can
        /// also self-cache via `zeroship.assets.put` + `ctx.waitUntil`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache: Option<CacheCtl>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rate_limit: Option<RateLimit>,
    },

    /// Outward HTTP redirect. `to` may reference glob captures
    /// (`[slug]`).
    Redirect {
        to: String,
        status: u16,
    },

    /// Internal URL rewrite. Mutates the request path and restarts
    /// rule walking from index 0 (with a hop limit to prevent loops).
    Rewrite { to: String },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkerMode {
    /// `POST /_rpc/<method>` → `worker.<method>(...args)`.
    Rpc,
    /// Any HTTP method/path → `worker.default.fetch(req, env, ctx)`.
    Ssr,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Head,
    Options,
    /// Any method. Equivalent to omitting the `method` field.
    #[serde(rename = "*")]
    Any,
}

/// Cache configuration applied to the response. Used by both
/// `static` and `worker` (SSR) actions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheCtl {
    /// `Cache-Control: public, max-age=<this>`.
    pub max_age: u64,
    /// stale-while-revalidate window in seconds. When set, gateway
    /// serves stale content while revalidating in the background.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swr_window: Option<u64>,
    /// Forever cache + `immutable` flag. Use for hashed asset URLs.
    #[serde(default)]
    pub immutable: bool,
    /// Background-refresh stale content via the worker rather than
    /// blocking the response. Defaults to true when `swr_window` is set.
    #[serde(default)]
    pub background_refresh: bool,
    /// Serve the cached version even if the upstream errors.
    #[serde(default)]
    pub stale_on_error: bool,
}

/// Per-rule rate limiting. Enforced at the gateway, before the worker
/// is ever invoked.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpm: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rps: Option<u32>,
    /// What identity to bucket requests by.
    #[serde(default = "RateLimit::default_per")]
    pub per: RateLimitPer,
}

impl RateLimit {
    fn default_per() -> RateLimitPer { RateLimitPer::Ip }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitPer {
    Ip,
    Session,
    App,
}

/// One asset blob, content-addressed. Stored in the object store
/// keyed by `hash`; surfaced to the gateway via the manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetEntry {
    /// SHA-256 (hex) of the raw bytes. Used as the object-store key
    /// AND as the HTTP `ETag`.
    pub hash: String,
    pub content_type: String,
    /// Total bytes (decoded). Used for `Content-Length` and quotas.
    pub size: u64,
    /// Per-asset cache override. If absent, the rule's `cache` field
    /// applies; if that's also absent, gateway picks a sensible default
    /// (1y immutable for `/_assets/<hash>` paths, short for everything
    /// else).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheCtl>,
    /// Unix-seconds. Set on `assets.put`. The gateway computes
    /// staleness from `now - updated_at` against the `cache` window.
    #[serde(default)]
    pub updated_at: i64,
}

// ---------------------------------------------------------------------------
// Pattern matching helpers
// ---------------------------------------------------------------------------

impl Match {
    /// Test a request against this matcher. Returns Some(captures) if
    /// it matches, None if it doesn't. Captures are bound names from
    /// glob params (`[slug]`, `[...rest]`).
    pub fn test(&self, method: &str, path: &str) -> Option<HashMap<String, String>> {
        let method_ok = match self.method() {
            None | Some(HttpMethod::Any) => true,
            Some(m) => method.eq_ignore_ascii_case(m.as_str()),
        };
        if !method_ok {
            return None;
        }
        match self {
            Match::Exact { path: p, .. } => {
                if path == p { Some(HashMap::new()) } else { None }
            }
            Match::Prefix { path: p, .. } => {
                // Segment-aware: prefix must match either the whole path,
                // or end at a path-segment boundary (`/`). Avoids the classic
                // footgun where `/admin` matches `/administrator`.
                let hits = if let Some(bare) = p.strip_suffix('/') {
                    // Author wrote a trailing slash — treat the slash as part
                    // of the boundary so `/_rpc/` matches `/_rpc/foo` (and
                    // `/_rpc` itself, by extension).
                    path == bare || path.starts_with(p)
                } else {
                    path == p || path.starts_with(&format!("{p}/"))
                };
                if hits { Some(HashMap::new()) } else { None }
            }
            Match::Glob { path: pat, .. } => glob_match(pat, path),
            Match::Any => Some(HashMap::new()),
        }
    }

    fn method(&self) -> Option<HttpMethod> {
        match self {
            Match::Exact { method, .. }
            | Match::Prefix { method, .. }
            | Match::Glob { method, .. } => *method,
            Match::Any => None,
        }
    }
}

impl HttpMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
            HttpMethod::Put => "PUT",
            HttpMethod::Patch => "PATCH",
            HttpMethod::Delete => "DELETE",
            HttpMethod::Head => "HEAD",
            HttpMethod::Options => "OPTIONS",
            HttpMethod::Any => "*",
        }
    }
}

/// Match a Next.js-style glob pattern.
///
/// Supported forms:
///   `/foo`              — literal segment
///   `/[name]`           — single-segment named capture
///   `/[...rest]`        — multi-segment named capture (must be last)
///   `*`                 — single-segment wildcard (anonymous)
///
/// Returns Some(captures) when the path matches, None otherwise.
fn glob_match(pattern: &str, path: &str) -> Option<HashMap<String, String>> {
    let pat_segs: Vec<&str> = pattern.trim_start_matches('/').split('/').collect();
    let path_segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();

    let mut captures: HashMap<String, String> = HashMap::new();
    let mut pi = 0;
    let mut xi = 0;
    while pi < pat_segs.len() {
        let p = pat_segs[pi];

        // Catchall `[...name]` consumes the rest of the path.
        if p.starts_with("[...") && p.ends_with(']') {
            let name = &p[4..p.len() - 1];
            if pi + 1 != pat_segs.len() {
                return None; // catchall must be last
            }
            let rest = path_segs[xi..].join("/");
            captures.insert(name.to_string(), rest);
            return Some(captures);
        }

        if xi >= path_segs.len() {
            return None;
        }
        let x = path_segs[xi];

        if p.starts_with('[') && p.ends_with(']') {
            // Single-segment named capture. Empty segments don't
            // count — `/blog/[slug]` should NOT match `/blog/`.
            if x.is_empty() {
                return None;
            }
            let name = &p[1..p.len() - 1];
            captures.insert(name.to_string(), x.to_string());
        } else if p == "*" {
            if x.is_empty() {
                return None;
            }
        } else if p != x {
            return None;
        }

        pi += 1;
        xi += 1;
    }

    if xi != path_segs.len() {
        return None;
    }
    Some(captures)
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
