//! Pre-compiled manifest dispatch.
//!
//! Source `Manifest` is unchanged on the wire. At route-update time the
//! gateway compiles each app's manifest into a hot-path-friendly form:
//! one `EffectivePolicy` per resource, an RPC wire-id index, and a
//! path-segment trie for URL resources. Per-request work drops to a
//! single `HashMap::get` on the most-specific resource key.

use std::collections::HashMap;

use zeroship_core::types::{
    AssetEntry, AuthLevel, CacheCtl, Cors, HttpMethod, Manifest, ProcedureKind, RateLimit,
    ResourceEntry, WorkerCode,
};

// ---------------------------------------------------------------------------
// Compiled manifest types
// ---------------------------------------------------------------------------

#[allow(missing_debug_implementations)]
pub struct CompiledManifest {
    assets: HashMap<String, AssetEntry>,
    runtime_assets: HashMap<String, AssetEntry>,
    #[allow(dead_code)]
    worker: Option<WorkerCode>,
    #[allow(dead_code)]
    asset_version: i64,
    /// Per-resource flattened policy, computed once at app load.
    effective_policies: HashMap<String, EffectivePolicy>,
    /// RPC wire-id (without the `rpc:` prefix) → key into
    /// `effective_policies`. The wire URL `/_zs/v1/<wireId>` strips the
    /// prefix; this table answers "is there an RPC resource for this id?"
    /// in O(1).
    rpc_index: HashMap<String, String>,
    /// URL-path lookup for resource-tree dispatch. Most-specific match
    /// wins (literal beats glob; longer literal beats shorter).
    url_index: PathMatcher,
}

#[derive(Debug, Clone)]
struct CompiledGlob {
    segments: Vec<GlobSegment>,
}

#[derive(Debug, Clone)]
enum GlobSegment {
    Literal(String),
    SingleCapture(String),
    CatchAll(String),
    Star,
}

// ---------------------------------------------------------------------------
// Resource-tree compile target
// ---------------------------------------------------------------------------

/// Flattened per-resource policy + dispatch action. Computed once at
/// app-load time by `CompiledManifest::compile`; the per-request lookup
/// is a single `HashMap::get` keyed by the resource id.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct EffectivePolicy {
    pub auth: AuthLevel,
    pub rate_limit: Option<RateLimit>,
    pub cors: Option<Cors>,
    pub cache: Option<CacheCtl>,
    pub csrf_origins: Option<Vec<String>>,
    pub idempotent: bool,
    /// Idempotency dedupe TTL in hours. Honored only when
    /// `idempotent: true`. `None` → gateway default of 24h. Bounded to
    /// `[1, 168]` at validate-time; the gateway clamps defensively.
    pub idempotency_ttl_hours: Option<u32>,
    pub max_input_bytes: Option<u32>,
    pub middleware: Vec<String>,
    pub publicly_accessible: bool,
    pub kind: Option<ProcedureKind>,
    pub action: ResolvedAction,
    pub input_schema: Option<String>,
    pub output_schema: Option<String>,
}

/// What dispatch should actually do once the policy is satisfied.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum ResolvedAction {
    /// Forward to the worker as an RPC call (key was `rpc:<id>`, no
    /// explicit redirect/rewrite/static).
    WorkerRpc,
    /// Forward to the worker as an SSR fetch (key was a URL path, no
    /// explicit redirect/rewrite/static).
    WorkerSsr,
    Redirect { to: String, status: u16 },
    Rewrite { to: String },
    Static { try_chain: Vec<String> },
}

/// URL-path lookup for resource-tree dispatch. Holds two layers:
///
/// * `literals` — exact path → resource id (O(1))
/// * `globs` — glob-shaped resource keys ordered by specificity
///   (most specific first; ties broken by lexicographic order so a
///   given match is deterministic)
#[derive(Debug, Default, Clone)]
struct PathMatcher {
    /// All literal-path resource keys (no glob characters), keyed by the
    /// path itself for O(1) exact match.
    literals: HashMap<String, String>,
    /// Compiled globs ordered most-specific first.
    globs: Vec<UrlGlob>,
}

#[derive(Debug, Clone)]
struct UrlGlob {
    /// Original resource key, used as the lookup key into
    /// `effective_policies`.
    key: String,
    /// Compiled segment matcher.
    glob: CompiledGlob,
    /// Number of literal segments — used to break ties when ranking.
    /// More literal segments == more specific.
    literal_segs: usize,
}

impl PathMatcher {
    /// Look up the most-specific matching resource id for `path`.
    /// Returns `None` if nothing matched.
    ///
    /// Order:
    /// 1. Exact literal hit — O(1).
    /// 2. Glob match — first hit in the pre-sorted (most specific first)
    ///    list wins.
    fn find(&self, path: &str) -> Option<String> {
        if let Some(id) = self.literals.get(path) {
            return Some(id.clone());
        }
        for g in &self.globs {
            if match_glob_simple(&g.glob, path) {
                return Some(g.key.clone());
            }
        }
        None
    }
}

/// Test a CompiledGlob against a path without collecting captures.
fn match_glob_simple(glob: &CompiledGlob, path: &str) -> bool {
    let path_segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let mut pi = 0;
    let mut xi = 0;
    while pi < glob.segments.len() {
        let seg = &glob.segments[pi];
        if matches!(seg, GlobSegment::CatchAll(_)) {
            return true;
        }
        if xi >= path_segs.len() {
            return false;
        }
        let x = path_segs[xi];
        match seg {
            GlobSegment::Literal(lit) => {
                if lit != x {
                    return false;
                }
            }
            GlobSegment::SingleCapture(_) | GlobSegment::Star => {
                if x.is_empty() {
                    return false;
                }
            }
            GlobSegment::CatchAll(_) => unreachable!(),
        }
        pi += 1;
        xi += 1;
    }
    xi == path_segs.len()
}

/// `/api/admin/users` → ["", "api", "admin", "users"] then trimmed.
/// Returns the literal-segment count (used by `PathMatcher` for
/// most-specific tie-breaking when both globs match).
fn count_literal_segments(key: &str) -> usize {
    key.trim_start_matches('/')
        .split('/')
        .filter(|s| !s.is_empty() && !s.contains('[') && !s.contains('*'))
        .count()
}

fn is_glob_path(s: &str) -> bool {
    s.contains('[') || s.contains('*')
}

// ---------------------------------------------------------------------------
// Compilation
// ---------------------------------------------------------------------------

impl CompiledManifest {
    pub fn compile(m: &Manifest) -> Self {
        let (effective_policies, rpc_index, url_index) = compile_resource_tree(&m.resources);

        Self {
            assets: m.assets.clone(),
            runtime_assets: m.runtime_assets.clone(),
            worker: m.worker.clone(),
            asset_version: m.asset_version,
            effective_policies,
            rpc_index,
            url_index,
        }
    }

    /// Resolve a request to a resource id using the resource-tree.
    /// Returns `None` if no resource matched.
    ///
    /// * URL beginning with `/_zs/v1/` → strip prefix; lookup
    ///   `rpc:<remainder>` in `rpc_index`.
    /// * Otherwise → `url_index` lookup (most specific wins).
    pub fn lookup_resource(&self, path: &str) -> Option<&EffectivePolicy> {
        let key = self.lookup_resource_key(path)?;
        self.effective_policies.get(&key)
    }

    /// Same lookup as `lookup_resource`, but returns the resource key
    /// instead of the policy. Used by the per-resource rate-limiter to
    /// hash the matched key into a stable `rule_idx`.
    pub fn lookup_resource_key(&self, path: &str) -> Option<String> {
        if let Some(rest) = path.strip_prefix("/_zs/v1/") {
            return self.rpc_index.get(rest).cloned();
        }
        self.url_index.find(path)
    }

    /// Look up an asset (build-time or runtime-emitted) by path.
    /// Returns `(entry, is_runtime)`. Used by the resource-tree static
    /// action to resolve a `try` chain entry to bytes.
    pub fn lookup_asset_for_static(&self, path: &str) -> Option<(&AssetEntry, bool)> {
        if let Some(e) = self.runtime_assets.get(path) {
            return Some((e, true));
        }
        if let Some(e) = self.assets.get(path) {
            return Some((e, false));
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Resource-tree compilation
// ---------------------------------------------------------------------------

/// Compile the `manifest.resources` map into the gateway's three
/// per-resource lookup tables: flattened per-resource policy, an
/// `rpc_index` keyed by wire id (prefix-stripped), and a `url_index`
/// (path-segment matcher).
fn compile_resource_tree(
    resources: &HashMap<String, ResourceEntry>,
) -> (
    HashMap<String, EffectivePolicy>,
    HashMap<String, String>,
    PathMatcher,
) {
    if resources.is_empty() {
        return (HashMap::new(), HashMap::new(), PathMatcher::default());
    }
    let mut policies: HashMap<String, EffectivePolicy> = HashMap::new();
    let mut rpc_index: HashMap<String, String> = HashMap::new();
    let mut literals: HashMap<String, String> = HashMap::new();
    let mut globs: Vec<UrlGlob> = Vec::new();

    for key in resources.keys() {
        let policy = resolve_effective_policy(key, resources);
        policies.insert(key.clone(), policy);

        if let Some(rpc_id) = key.strip_prefix("rpc:") {
            rpc_index.insert(rpc_id.to_string(), key.clone());
        } else if key.starts_with('/') {
            if is_glob_path(key) {
                let glob = compile_glob(key);
                let literal_segs = count_literal_segments(key);
                globs.push(UrlGlob {
                    key: key.clone(),
                    glob,
                    literal_segs,
                });
            } else {
                literals.insert(key.clone(), key.clone());
            }
        }
        // `*` is the root default — it never dispatches itself; it just
        // contributes to inheritance.
    }

    // Order globs most-specific first. More literal segments → more
    // specific. Tie-break by descending key length, then by lexical
    // order so the result is deterministic.
    globs.sort_by(|a, b| {
        b.literal_segs
            .cmp(&a.literal_segs)
            .then_with(|| b.key.len().cmp(&a.key.len()))
            .then_with(|| a.key.cmp(&b.key))
    });

    let url_index = PathMatcher { literals, globs };
    (policies, rpc_index, url_index)
}

/// Walk the inheritance chain (root `*` → ancestors → key) and apply
/// the per-field merge rules from the spec.
fn resolve_effective_policy(
    key: &str,
    resources: &HashMap<String, ResourceEntry>,
) -> EffectivePolicy {
    let chain = build_inheritance_chain(key, resources);

    let mut auth: AuthLevel = AuthLevel::Anon;
    let mut rate_limit: Option<RateLimit> = None;
    let mut cors: Option<Cors> = None;
    let mut cache: Option<CacheCtl> = None;
    let mut csrf_origins: Option<Vec<String>> = None;
    let mut idempotent: bool = false;
    let mut idempotency_ttl_hours: Option<u32> = None;
    let mut max_input_bytes: Option<u32> = None;
    let mut middleware: Vec<String> = Vec::new();
    let mut publicly_accessible: bool = false;
    let mut kind: Option<ProcedureKind> = None;
    let mut input_schema: Option<String> = None;
    let mut output_schema: Option<String> = None;

    for ancestor_key in &chain {
        let Some(node) = resources.get(ancestor_key) else { continue };
        let is_self = ancestor_key == key;

        if let Some(a) = node.auth {
            // stricter wins — child only weakens via override (validated).
            if a.rank() > auth.rank() {
                auth = a;
            } else if is_self && node.r#override.iter().any(|f| f == "auth") {
                auth = a;
            }
        }
        if let Some(rl) = &node.rate_limit {
            // min wins (stricter cap survives).
            rate_limit = Some(merge_rate_limit_min(rate_limit.as_ref(), rl));
        }
        if let Some(c) = &node.cors {
            // intersect: child can only narrow.
            cors = Some(merge_cors_intersect(cors.as_ref(), c));
        }
        if let Some(c) = &node.cache {
            // child overrides per-resource decision.
            cache = Some(c.clone());
        }
        if let Some(origins) = &node.csrf_origins {
            csrf_origins = Some(merge_string_intersect(csrf_origins.as_ref(), origins));
        }
        if let Some(b) = node.idempotent {
            idempotent = b;
        }
        if let Some(h) = node.idempotency_ttl_hours {
            // Child overrides — same merge rule as `idempotent`. The
            // override-marker validator catches accidental shadows.
            idempotency_ttl_hours = Some(h);
        }
        if let Some(b) = node.max_input_bytes {
            max_input_bytes = Some(match max_input_bytes {
                None => b,
                Some(prev) => prev.min(b),
            });
        }
        // middleware appends in chain order (root → child).
        for m in &node.middleware {
            middleware.push(m.clone());
        }
        if let Some(b) = node.publicly_accessible {
            publicly_accessible = b;
        }
        if is_self {
            kind = node.kind;
            input_schema.clone_from(&node.input_schema);
            output_schema.clone_from(&node.output_schema);
        }
    }

    let action = resolve_action(key, resources);

    EffectivePolicy {
        auth,
        rate_limit,
        cors,
        cache,
        csrf_origins,
        idempotent,
        idempotency_ttl_hours,
        max_input_bytes,
        middleware,
        publicly_accessible,
        kind,
        action,
        input_schema,
        output_schema,
    }
}

/// Build the parent chain root → … → key for a given resource.
///
/// Mirrors the validation-side helper but lives here so the gateway
/// doesn't take a dep on `validate`'s private function. For RPC keys
/// the ancestors come from dot-segments; for URL paths from path
/// segments; `*` is always at the head when declared.
fn build_inheritance_chain(
    key: &str,
    resources: &HashMap<String, ResourceEntry>,
) -> Vec<String> {
    if key == "*" {
        return vec!["*".to_string()];
    }
    let mut chain: Vec<String> = Vec::new();
    if resources.contains_key("*") {
        chain.push("*".to_string());
    }
    if let Some(rpc_id) = key.strip_prefix("rpc:") {
        let segs: Vec<&str> = rpc_id.split('.').collect();
        for end in 1..segs.len() {
            let ancestor = format!("rpc:{}", segs[..end].join("."));
            if resources.contains_key(&ancestor) {
                chain.push(ancestor);
            }
        }
    } else if key.starts_with('/') {
        let trimmed = key.trim_start_matches('/');
        let segs: Vec<&str> = trimmed.split('/').collect();
        for end in 1..segs.len() {
            let ancestor = format!("/{}", segs[..end].join("/"));
            if resources.contains_key(&ancestor) {
                chain.push(ancestor);
            }
        }
    }
    chain.push(key.to_string());
    chain
}

/// `min` merge for rate_limit. We collapse onto whichever side has the
/// smaller `rps`/`rpm` (treating absent → unbounded).
fn merge_rate_limit_min(prev: Option<&RateLimit>, next: &RateLimit) -> RateLimit {
    let Some(p) = prev else { return next.clone() };
    let pick_min = |a: Option<u32>, b: Option<u32>| match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (None, None) => None,
    };
    RateLimit {
        rpm: pick_min(p.rpm, next.rpm),
        rps: pick_min(p.rps, next.rps),
        // Use the child's `per` — it's a discriminator, not a cap.
        per: next.per,
    }
}

/// Intersection merge for `Cors`. Origins / methods / headers all
/// narrow as we walk root → child. `allow_credentials` is `&&` (any
/// ancestor can revoke credentials).
fn merge_cors_intersect(prev: Option<&Cors>, next: &Cors) -> Cors {
    let Some(p) = prev else { return next.clone() };
    let intersect_strings = |a: &[String], b: &[String]| -> Vec<String> {
        a.iter().filter(|x| b.iter().any(|y| x == &y)).cloned().collect()
    };
    let intersect_methods = |a: &[HttpMethod], b: &[HttpMethod]| -> Vec<HttpMethod> {
        a.iter().filter(|x| b.iter().any(|y| x == &y)).copied().collect()
    };
    Cors {
        allow_origins: intersect_strings(&p.allow_origins, &next.allow_origins),
        allow_methods: intersect_methods(&p.allow_methods, &next.allow_methods),
        allow_headers: intersect_strings(&p.allow_headers, &next.allow_headers),
        expose_headers: intersect_strings(&p.expose_headers, &next.expose_headers),
        allow_credentials: p.allow_credentials && next.allow_credentials,
        max_age_seconds: match (p.max_age_seconds, next.max_age_seconds) {
            (Some(a), Some(b)) => Some(a.min(b)),
            _ => p.max_age_seconds.or(next.max_age_seconds),
        },
    }
}

fn merge_string_intersect(prev: Option<&Vec<String>>, next: &[String]) -> Vec<String> {
    match prev {
        None => next.to_vec(),
        Some(p) => p
            .iter()
            .filter(|x| next.iter().any(|y| x == &y))
            .cloned()
            .collect(),
    }
}

fn resolve_action(key: &str, resources: &HashMap<String, ResourceEntry>) -> ResolvedAction {
    let Some(entry) = resources.get(key) else {
        // Root-only fallback (key isn't in the map). Should never happen
        // because every callsite walks `resources.keys()`.
        return ResolvedAction::WorkerSsr;
    };
    if let Some(r) = &entry.redirect {
        return ResolvedAction::Redirect {
            to: r.to.clone(),
            status: r.status,
        };
    }
    if let Some(t) = &entry.rewrite {
        return ResolvedAction::Rewrite { to: t.clone() };
    }
    if let Some(s) = &entry.r#static {
        return ResolvedAction::Static {
            try_chain: s.r#try.clone(),
        };
    }
    if key.starts_with("rpc:") {
        ResolvedAction::WorkerRpc
    } else {
        ResolvedAction::WorkerSsr
    }
}

fn compile_glob(pattern: &str) -> CompiledGlob {
    let raw_segs: Vec<&str> = pattern.trim_start_matches('/').split('/').collect();
    let total = raw_segs.len();
    let mut segments = Vec::with_capacity(total);
    for (i, seg) in raw_segs.iter().enumerate() {
        if seg.starts_with("[...") && seg.ends_with(']') {
            assert!(
                i + 1 == total,
                "glob catch-all [...name] must be the last segment in `{pattern}`",
            );
            let name = &seg[4..seg.len() - 1];
            segments.push(GlobSegment::CatchAll(name.to_string()));
        } else if seg.starts_with('[') && seg.ends_with(']') {
            let name = &seg[1..seg.len() - 1];
            segments.push(GlobSegment::SingleCapture(name.to_string()));
        } else if *seg == "*" {
            segments.push(GlobSegment::Star);
        } else {
            segments.push(GlobSegment::Literal((*seg).to_string()));
        }
    }
    CompiledGlob { segments }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use zeroship_core::types::{
        AuthLevel, Manifest, ProcedureKind, RateLimit, RateLimitPer, RedirectAction,
        ResourceEntry, StaticAction,
    };

    fn rpc_entry(kind: ProcedureKind) -> ResourceEntry {
        ResourceEntry {
            kind: Some(kind),
            ..Default::default()
        }
    }

    #[test]
    fn empty_resources_yields_no_lookup_hits() {
        let m = Manifest::default();
        let c = CompiledManifest::compile(&m);
        assert!(c.lookup_resource("/foo").is_none());
        assert!(c.lookup_resource("/_zs/v1/x").is_none());
    }

    #[test]
    fn resource_tree_rpc_lookup_strips_wire_prefix() {
        let mut resources = HashMap::new();
        resources.insert(
            "rpc:todos.list".to_string(),
            rpc_entry(ProcedureKind::Query),
        );
        let m = Manifest {
            version: 1,
            resources,
            ..Manifest::default()
        };
        let c = CompiledManifest::compile(&m);
        let p = c.lookup_resource("/_zs/v1/todos.list").expect("matches");
        assert_eq!(p.kind, Some(ProcedureKind::Query));
        assert!(matches!(p.action, ResolvedAction::WorkerRpc));
        // Unknown wire id → no match.
        assert!(c.lookup_resource("/_zs/v1/unknown.method").is_none());
        // Same id without the wire prefix is NOT looked up via rpc_index.
        assert!(c.lookup_resource("/todos.list").is_none());
    }

    #[test]
    fn resource_tree_url_lookup_picks_most_specific() {
        let mut resources = HashMap::new();
        resources.insert(
            "/api".into(),
            ResourceEntry {
                auth: Some(AuthLevel::User),
                ..Default::default()
            },
        );
        resources.insert(
            "/api/admin".into(),
            ResourceEntry {
                auth: Some(AuthLevel::Admin),
                r#override: vec!["auth".into()],
                ..Default::default()
            },
        );
        let m = Manifest {
            version: 1,
            resources,
            ..Manifest::default()
        };
        let c = CompiledManifest::compile(&m);
        // Exact-literal match takes priority.
        let key = c.lookup_resource_key("/api/admin").expect("matches");
        assert_eq!(key, "/api/admin");
    }

    #[test]
    fn resource_tree_glob_url_lookup() {
        let mut resources = HashMap::new();
        resources.insert("/blog/[slug]".into(), ResourceEntry::default());
        let m = Manifest {
            version: 1,
            resources,
            ..Manifest::default()
        };
        let c = CompiledManifest::compile(&m);
        let key = c.lookup_resource_key("/blog/hello").expect("glob matches");
        assert_eq!(key, "/blog/[slug]");
        // Empty segment after the prefix doesn't match a single-segment capture.
        assert!(c.lookup_resource_key("/blog/").is_none());
    }

    #[test]
    fn effective_policy_inherits_auth_from_root() {
        let mut resources = HashMap::new();
        resources.insert(
            "*".into(),
            ResourceEntry {
                auth: Some(AuthLevel::Admin),
                ..Default::default()
            },
        );
        resources.insert(
            "rpc:todos.list".into(),
            rpc_entry(ProcedureKind::Query),
        );
        let m = Manifest {
            version: 1,
            resources,
            ..Manifest::default()
        };
        let c = CompiledManifest::compile(&m);
        let p = c.lookup_resource("/_zs/v1/todos.list").expect("matches");
        assert_eq!(p.auth, AuthLevel::Admin, "inherits root admin");
        assert_eq!(p.kind, Some(ProcedureKind::Query));
    }

    #[test]
    fn effective_policy_stricter_auth_wins_along_chain() {
        let mut resources = HashMap::new();
        // Root: anon, parent: user, child: doesn't override → user wins.
        resources.insert(
            "*".into(),
            ResourceEntry {
                auth: Some(AuthLevel::Anon),
                publicly_accessible: Some(true),
                ..Default::default()
            },
        );
        resources.insert(
            "rpc:todos".into(),
            ResourceEntry {
                auth: Some(AuthLevel::User),
                r#override: vec!["auth".into(), "publicly_accessible".into()],
                publicly_accessible: Some(false),
                ..Default::default()
            },
        );
        resources.insert(
            "rpc:todos.list".into(),
            rpc_entry(ProcedureKind::Query),
        );
        let m = Manifest {
            version: 1,
            resources,
            ..Manifest::default()
        };
        let c = CompiledManifest::compile(&m);
        let p = c.lookup_resource("/_zs/v1/todos.list").expect("matches");
        assert_eq!(p.auth, AuthLevel::User, "stricter user beats root anon");
    }

    #[test]
    fn effective_policy_rate_limit_min_wins() {
        let mut resources = HashMap::new();
        resources.insert(
            "*".into(),
            ResourceEntry {
                auth: Some(AuthLevel::Admin),
                rate_limit: Some(RateLimit {
                    rpm: Some(600),
                    rps: None,
                    per: RateLimitPer::Ip,
                }),
                ..Default::default()
            },
        );
        resources.insert(
            "rpc:todos.add".into(),
            ResourceEntry {
                kind: Some(ProcedureKind::Mutation),
                rate_limit: Some(RateLimit {
                    rpm: Some(60),
                    rps: None,
                    per: RateLimitPer::Ip,
                }),
                r#override: vec!["rate_limit".into()],
                ..Default::default()
            },
        );
        let m = Manifest {
            version: 1,
            resources,
            ..Manifest::default()
        };
        let c = CompiledManifest::compile(&m);
        let p = c.lookup_resource("/_zs/v1/todos.add").expect("matches");
        assert_eq!(p.rate_limit.as_ref().unwrap().rpm, Some(60), "stricter cap wins");
    }

    #[test]
    fn resolved_action_picks_explicit_over_default() {
        let mut resources = HashMap::new();
        resources.insert(
            "/old".into(),
            ResourceEntry {
                redirect: Some(RedirectAction { to: "/new".into(), status: 301 }),
                ..Default::default()
            },
        );
        resources.insert(
            "/_assets/main.js".into(),
            ResourceEntry {
                r#static: Some(StaticAction { r#try: vec!["$path".into()] }),
                ..Default::default()
            },
        );
        let m = Manifest {
            version: 1,
            resources,
            ..Manifest::default()
        };
        let c = CompiledManifest::compile(&m);
        let p1 = c.lookup_resource("/old").expect("matches");
        match &p1.action {
            ResolvedAction::Redirect { to, status } => {
                assert_eq!(to, "/new");
                assert_eq!(*status, 301);
            }
            other => panic!("expected Redirect, got {other:?}"),
        }
        let p2 = c.lookup_resource("/_assets/main.js").expect("matches");
        assert!(matches!(p2.action, ResolvedAction::Static { .. }));
    }

    #[test]
    fn middleware_appends_root_to_child() {
        let mut resources = HashMap::new();
        resources.insert(
            "*".into(),
            ResourceEntry {
                auth: Some(AuthLevel::Admin),
                middleware: vec!["audit".into()],
                ..Default::default()
            },
        );
        resources.insert(
            "rpc:billing.charge".into(),
            ResourceEntry {
                kind: Some(ProcedureKind::Mutation),
                middleware: vec!["transaction".into()],
                ..Default::default()
            },
        );
        let m = Manifest {
            version: 1,
            resources,
            ..Manifest::default()
        };
        let c = CompiledManifest::compile(&m);
        let p = c.lookup_resource("/_zs/v1/billing.charge").expect("matches");
        assert_eq!(
            p.middleware,
            vec!["audit".to_string(), "transaction".to_string()]
        );
    }

    #[test]
    fn url_index_orders_globs_by_specificity() {
        // Glob `/api/v1/*` should win over a less specific glob; literals
        // beat globs when both could match.
        let mut resources = HashMap::new();
        resources.insert("/api/v1/users".into(), ResourceEntry::default());
        resources.insert("/api/v1/*".into(), ResourceEntry::default());
        let m = Manifest {
            version: 1,
            resources,
            ..Manifest::default()
        };
        let c = CompiledManifest::compile(&m);
        // Literal path takes priority.
        assert_eq!(
            c.lookup_resource_key("/api/v1/users").as_deref(),
            Some("/api/v1/users")
        );
        // Glob picks up unmatched paths.
        assert_eq!(
            c.lookup_resource_key("/api/v1/teams").as_deref(),
            Some("/api/v1/*")
        );
    }

    #[test]
    fn passthrough_resolves_to_worker_ssr_via_root_star() {
        let m = Manifest::passthrough();
        let c = CompiledManifest::compile(&m);
        // `*` doesn't dispatch; nothing matches by URL.
        assert!(c.lookup_resource("/anything").is_none());
        assert!(c.lookup_resource("/_zs/v1/anything").is_none());
    }
}
