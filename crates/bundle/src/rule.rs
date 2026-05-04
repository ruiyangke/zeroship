use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// A single routing rule. Match describes *when* it fires; action
/// describes *what* it does.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub r#match: Match,
    pub action: Action,
    /// Optional CORS policy applied to this rule. When set, the gateway
    /// short-circuits CORS preflight (`OPTIONS` with `Origin`) before
    /// dispatch and injects the response headers after dispatch.
    /// Defaults to `None`, keeping legacy manifests deserializable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<Cors>,
}

/// CORS policy attached to a [`Rule`]. Per-rule (not global, not
/// per-action) so apps can opt in selectively.
///
/// v1 simplifications: origins are exact strings; the literal `"*"`
/// matches any origin (subject to credentials rules); no glob, no
/// automatic header reflection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Cors {
    /// Origins to allow. Each entry is an exact string match. The
    /// literal "*" matches any origin (subject to credentials rules).
    pub allow_origins: Vec<String>,
    /// HTTP methods to allow on cross-origin requests.
    #[serde(default)]
    pub allow_methods: Vec<HttpMethod>,
    /// Request headers the browser may send.
    #[serde(default)]
    pub allow_headers: Vec<String>,
    /// Response headers the browser may expose to the calling page.
    #[serde(default)]
    pub expose_headers: Vec<String>,
    #[serde(default)]
    pub allow_credentials: bool,
    /// Cache duration for the preflight response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age_seconds: Option<u64>,
}

// ---------------------------------------------------------------------------
// v3 resource tree (rpc-v2 §7).
//
// One entry per resource keyed off `Manifest.resources`. The shape is the
// public wire — clients (the build, the gateway, future codegen) must agree
// on field names, so every optional field skips serialization when absent.
// The gateway compiles this map into per-resource `EffectivePolicy` records
// at app-load time; the wire layout itself isn't walked on the hot path.
// ---------------------------------------------------------------------------

/// One resource entry in `Manifest.resources`. Mixes routing-action
/// hints (redirect / rewrite / static — at most one), policy fields
/// (auth, cors, …) and procedure metadata (kind, schemas).
///
/// All fields default and all optional fields skip serialization when
/// absent — keeps v3 manifests small and lets future fields land without
/// breaking on-the-wire compat.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ResourceEntry {
    // ── Routing actions (at most one of redirect / rewrite / static) ─────
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect: Option<RedirectAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rewrite: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#static: Option<StaticAction>,

    // ── Policy fields (any combination) ──────────────────────────────────
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<Cors>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheCtl>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub csrf_origins: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotent: Option<bool>,
    /// Per-procedure idempotency dedupe TTL in hours. Honored by the
    /// gateway only when `idempotent: true`. Min 1, max 168 (7 days).
    /// Absent → gateway default of 24h. Authored as
    /// `fn.config.idempotencyTtl: { hours: 168 }`; the vite-plugin
    /// extracts the `hours` field and writes it here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_ttl_hours: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_bytes: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub middleware: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publicly_accessible: Option<bool>,

    // ── Override marker — required when shadowing inherited fields ───────
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub r#override: Vec<String>,

    // ── Procedure metadata (RPC only) ────────────────────────────────────
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ProcedureKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<String>,
}

/// Authentication level applied to a resource. Strictness order is
/// `admin > user > anon`; the inheritance walk uses this to decide who
/// wins (stricter wins; weakening requires `override`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthLevel {
    /// No authentication required. Unsafe by default — must pair with
    /// `publicly_accessible: true` to confirm intent.
    Anon,
    /// Authenticated end-user.
    User,
    /// Platform admin. Highest level.
    Admin,
}

impl AuthLevel {
    /// Strictness rank — higher number = stricter. Used by the merge
    /// rule "stricter wins".
    pub fn rank(self) -> u8 {
        match self {
            Self::Anon => 0,
            Self::User => 1,
            Self::Admin => 2,
        }
    }
}

/// Procedure kind, declared on `rpc:` resource entries. Drives method
/// gating (mutations refuse `GET`) and the wire shape (streams use SSE,
/// subscriptions use WebSocket).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProcedureKind {
    Query,
    Mutation,
    Stream,
    Subscription,
}

/// HTTP redirect declaration on a resource entry. Status defaults to
/// 302 (a soft redirect) so apps that just want to forward a path
/// don't have to think about cacheability.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RedirectAction {
    pub to: String,
    #[serde(default = "redirect_default_status")]
    pub status: u16,
}

fn redirect_default_status() -> u16 { 302 }

/// Static-asset declaration on a resource entry. Mirrors the
/// `Action::Static.try` chain — first asset that resolves wins.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct StaticAction {
    pub r#try: Vec<String>,
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
    /// Serve from `assets` / `runtime_assets`. The `try` chain
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
