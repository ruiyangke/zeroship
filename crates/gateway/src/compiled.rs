//! Pre-compiled manifest dispatch.
//!
//! Source `Manifest` is unchanged on the wire. At route-update time the
//! gateway compiles each app's manifest into a hot-path-friendly form:
//! one `EffectivePolicy` per resource, an RPC wire-id index, and a
//! path-segment trie for URL resources. Per-request work drops to a
//! single `HashMap::get` on the most-specific resource key.

use std::collections::HashMap;

use zeroship_bundle::{
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
    /// `effective_policies`. The wire URL `/__zeroship/v1/<wireId>` strips the
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
    /// Per-procedure handler timeout in milliseconds. `None` inherits
    /// the app-level `wall_timeout_ms`. Idempotency in-flight wait caps
    /// to this value when set.
    pub timeout_ms: Option<u64>,
    pub max_input_bytes: Option<u32>,
    pub middleware: Vec<String>,
    pub publicly_accessible: bool,
    /// OAuth scopes a request MUST carry to reach this resource (auth-sdk
    /// Slice 3c, §5.3). Accumulated by union along the inheritance chain
    /// (root → child): a scoped parent's requirement is inherited and a
    /// child can only ADD, never weaken it. Empty ⇒ no scope gate. The
    /// auth gate (`resolve_auth`) checks the authenticated principal's
    /// granted `scopes` against this set — ONLY on `User`/`Admin` routes
    /// (an `Anon` public route never scope-gates an authenticated visitor)
    /// — and answers a `403` (`scope_required` JSON body,
    /// `insufficient_scope` `WWW-Authenticate` token) on a miss.
    pub required_scopes: Vec<String>,
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

/// A path segment that means "this directory" (WHATWG URL "single-dot path
/// segment"): literal `.` or its percent-encoded form `%2e` (case-insensitive).
fn is_single_dot_segment(seg: &str) -> bool {
    seg == "." || seg.eq_ignore_ascii_case("%2e")
}

/// A path segment that means "parent directory" (WHATWG URL "double-dot path
/// segment"): `..`, `.%2e`, `%2e.`, or `%2e%2e` (case-insensitive). These are
/// the exact forms a browser's `new URL(...)` collapses, so the gateway must
/// resolve them identically to keep its auth match in lock-step with the
/// worker's re-parse.
fn is_double_dot_segment(seg: &str) -> bool {
    seg.eq_ignore_ascii_case("..")
        || seg.eq_ignore_ascii_case(".%2e")
        || seg.eq_ignore_ascii_case("%2e.")
        || seg.eq_ignore_ascii_case("%2e%2e")
}

/// True if `seg` is any dot-segment (single or double). A request path that
/// carries one is non-canonical and — because WHATWG `new URL` collapses it —
/// is the gateway↔worker path-disagreement vector SEC-2 closes. Callers that
/// want fail-closed rejection (the dispatch layer) use this to 400 such paths;
/// the matcher uses [`canonicalize_path`] to resolve them defensively.
pub(crate) fn is_dot_segment(seg: &str) -> bool {
    is_single_dot_segment(seg) || is_double_dot_segment(seg)
}

/// True if the raw request path is NOT already in canonical form because it
/// carries a dot-segment (`.`/`..`, literal or `%2e`-encoded) or an empty
/// interior segment (`//`). These are exactly the forms a browser's WHATWG
/// `new URL` rewrites, so forwarding the raw path while matching auth on a
/// different normalization is the SEC-2 bypass. The dispatch layer rejects
/// such requests (400) rather than guess which normalization the worker will
/// pick.
pub(crate) fn path_has_traversal_or_empty_segment(path: &str) -> bool {
    let trimmed = path.strip_prefix('/').unwrap_or(path);
    // A lone trailing slash (`/api/admin/`) is a benign single-slash-policy
    // case, normalized — not rejected. Only an EMPTY INTERIOR segment (`//`)
    // or any dot-segment is a disagreement vector.
    let segs: Vec<&str> = trimmed.split('/').collect();
    for (i, seg) in segs.iter().enumerate() {
        if is_dot_segment(seg) {
            return true;
        }
        // Interior empty segment (`a//b`) — exclude the final element so a
        // single trailing slash is not treated as traversal.
        if seg.is_empty() && i + 1 < segs.len() {
            return true;
        }
    }
    false
}

/// Canonicalize a request path before resource matching (SEC-2).
///
/// Resolves the path to the single normal form the gateway both *matches auth
/// against* and *forwards to the worker*, so the gateway's resource match can
/// never disagree with the worker's WHATWG `new URL(req.url).pathname`:
///
/// * percent-encoded dot segments (`%2e`/`%2E`) decode to `.`,
/// * single-dot segments (`.`) and empty segments (`//`) are dropped,
/// * double-dot segments (`..`) pop the previous segment, clamped at root
///   (never escaping above `/`),
/// * a single trailing slash is stripped (`/a/b/` → `/a/b`); root stays `/`.
///
/// Only the dot-segment-relevant `%2e` is decoded — every other percent-escape
/// is preserved byte-for-byte so the canonical form still round-trips through
/// the worker's URL parser unchanged.
pub(crate) fn canonicalize_path(path: &str) -> String {
    let trimmed = path.strip_prefix('/').unwrap_or(path);
    let mut out: Vec<&str> = Vec::new();
    for seg in trimmed.split('/') {
        if seg.is_empty() || is_single_dot_segment(seg) {
            // Drop empty (`//`) and single-dot (`.`) segments.
            continue;
        }
        if is_double_dot_segment(seg) {
            // Pop the parent; clamp at root (a `..` past root is ignored).
            out.pop();
            continue;
        }
        out.push(seg);
    }
    if out.is_empty() {
        return "/".to_string();
    }
    let mut canonical = String::with_capacity(path.len());
    for seg in out {
        canonical.push('/');
        canonical.push_str(seg);
    }
    canonical
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
    /// * URL beginning with `/__zeroship/v1/` → strip prefix; lookup
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
        // SEC-2: canonicalize BEFORE matching so dot-segment / percent-encoded
        // -dot / empty-segment / trailing-slash evasions of a protected
        // resource resolve to that resource (and its stricter auth) instead of
        // falling through to a permissive catch-all. The dispatch layer also
        // rejects (400) traversal forms; canonicalizing here keeps every
        // caller (CORS preflight, rate-limit keying, this lookup) safe even if
        // a future callsite forgets the 400 guard.
        let canonical = canonicalize_path(path);
        if let Some(rest) = canonical.strip_prefix("/__zeroship/v1/") {
            return self.rpc_index.get(rest).cloned();
        }
        self.url_index.find(&canonical)
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
    // SEC-5: track whether ANY resource in the chain declared `auth`, so a
    // procedure that falls through to the default is distinguishable from one
    // deliberately set to `anon` — see the fail-closed default after the loop.
    let mut auth_declared = false;
    let mut rate_limit: Option<RateLimit> = None;
    let mut cors: Option<Cors> = None;
    let mut cache: Option<CacheCtl> = None;
    let mut csrf_origins: Option<Vec<String>> = None;
    let mut idempotent: bool = false;
    let mut idempotency_ttl_hours: Option<u32> = None;
    let mut max_input_bytes: Option<u32> = None;
    let mut middleware: Vec<String> = Vec::new();
    let mut publicly_accessible: bool = false;
    let mut required_scopes: Vec<String> = Vec::new();
    let mut kind: Option<ProcedureKind> = None;
    let mut input_schema: Option<String> = None;
    let mut output_schema: Option<String> = None;

    for ancestor_key in &chain {
        let Some(node) = resources.get(ancestor_key) else { continue };
        let is_self = ancestor_key == key;

        if let Some(a) = node.auth {
            auth_declared = true;
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
        // required_scopes UNIONS along the chain (root → child): a scoped
        // parent's requirement is inherited and a child only ADDS. No
        // `override` weakens it — a stricter scope gate must never be
        // silently dropped by a more-specific resource. Dedupe to keep the
        // 403 `required` body and the superset check clean.
        for s in &node.required_scopes {
            if !required_scopes.iter().any(|existing| existing == s) {
                required_scopes.push(s.clone());
            }
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

    // SEC-5 fail-closed default for the RPC surface. A procedure whose entire
    // inheritance chain declares no `auth` defaults to `User`, never the silent
    // `Anon` that turned a forgotten or mistyped policy into an unauthenticated
    // exposure (the SEC-5 class — a drifted family key left `projects.*` open).
    // The web surface (URL / SSR / static) keeps the public-by-default norm;
    // only `rpc:` procedures flip. A deliberately public procedure opts in with
    // `auth: anon` + `publicly_accessible: true` somewhere in its chain, which
    // sets `auth_declared` and so is left untouched here.
    if !auth_declared && key.starts_with("rpc:") {
        auth = AuthLevel::User;
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
        timeout_ms: None, // wired through ResourceEntry once the field lands
        max_input_bytes,
        middleware,
        publicly_accessible,
        required_scopes,
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
    use zeroship_bundle::{
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
        assert!(c.lookup_resource("/__zeroship/v1/x").is_none());
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
        let p = c.lookup_resource("/__zeroship/v1/todos.list").expect("matches");
        assert_eq!(p.kind, Some(ProcedureKind::Query));
        assert!(matches!(p.action, ResolvedAction::WorkerRpc));
        // Unknown wire id → no match.
        assert!(c.lookup_resource("/__zeroship/v1/unknown.method").is_none());
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
    fn path_normalization_bypass_resolves_to_protected_resource() {
        // SEC-2: the gateway must canonicalize the request path BEFORE
        // resource matching, so dot-segment / percent-encoded-dot /
        // trailing-slash evasions of a protected literal can never fall
        // through to a permissive catch-all. Manifest: `/api/admin` is
        // `user`-gated; a root catch-all glob is `anon`. Pre-fix each
        // evasion matches the anon catch-all (the literal compares the RAW
        // string and misses); post-fix the canonical form re-matches the
        // `/api/admin` user literal. The worker's WHATWG `new URL` collapses
        // these exact forms to `/api/admin`, so the gateway match and the
        // worker view must agree on the protected resource.
        let mut resources = HashMap::new();
        resources.insert(
            "/api/admin".into(),
            ResourceEntry {
                auth: Some(AuthLevel::User),
                ..Default::default()
            },
        );
        resources.insert(
            "/[...rest]".into(),
            ResourceEntry {
                auth: Some(AuthLevel::Anon),
                publicly_accessible: Some(true),
                ..Default::default()
            },
        );
        let m = Manifest {
            version: 1,
            resources,
            ..Manifest::default()
        };
        let c = CompiledManifest::compile(&m);

        // Baseline: the catch-all is reachable for an unrelated path.
        assert_eq!(
            c.lookup_resource_key("/public/page").as_deref(),
            Some("/[...rest]"),
            "unrelated path falls through to the anon catch-all"
        );

        for evasion in [
            "/api/foo/../admin",
            "/api/%2e/admin",
            "/api/%2E/admin",
            "/api/./admin",
            "/api/admin/",
            "/api//admin",
            "/api/foo/%2e%2e/admin",
        ] {
            let key = c.lookup_resource_key(evasion);
            assert_eq!(
                key.as_deref(),
                Some("/api/admin"),
                "evasion {evasion:?} must canonicalize to the user-gated /api/admin, \
                 not fall through to the anon catch-all (got {key:?})"
            );
            let policy = c.lookup_resource(evasion).expect("policy resolves");
            assert_eq!(
                policy.auth,
                AuthLevel::User,
                "evasion {evasion:?} must resolve the User auth gate, not Anon"
            );
        }
    }

    #[test]
    fn canonicalize_path_resolves_dot_segments_and_trailing_slash() {
        // SEC-2 unit coverage of the canonicalizer that backs the matcher:
        // percent-encoded dot segments decode, `.`/`..` resolve (clamped at
        // root), empty segments collapse, and a single trailing slash is
        // stripped (root stays `/`).
        assert_eq!(canonicalize_path("/api/admin"), "/api/admin");
        assert_eq!(canonicalize_path("/api/admin/"), "/api/admin");
        assert_eq!(canonicalize_path("/api/./admin"), "/api/admin");
        assert_eq!(canonicalize_path("/api/foo/../admin"), "/api/admin");
        assert_eq!(canonicalize_path("/api/%2e/admin"), "/api/admin");
        assert_eq!(canonicalize_path("/api/%2E/admin"), "/api/admin");
        assert_eq!(canonicalize_path("/api/foo/%2e%2e/admin"), "/api/admin");
        assert_eq!(canonicalize_path("/api//admin"), "/api/admin");
        // `..` past root is clamped, never escapes above `/`.
        assert_eq!(canonicalize_path("/../../etc/passwd"), "/etc/passwd");
        // Root and empty normalize to `/`.
        assert_eq!(canonicalize_path("/"), "/");
        assert_eq!(canonicalize_path(""), "/");
        // A dot INSIDE a segment is not a dot-segment.
        assert_eq!(canonicalize_path("/docs/a.b.c"), "/docs/a.b.c");
        // RPC wire paths are untouched (no dot segments).
        assert_eq!(
            canonicalize_path("/__zeroship/v1/todos.list"),
            "/__zeroship/v1/todos.list"
        );
    }

    /// ISS-60 (SSG trailing-slash): a prerendered docs site declares a static
    /// `/about` resource and a catch-all `/[...rest]` SPA/index fallback.
    /// `GET /about/` (trailing slash) must resolve to the `/about` static
    /// resource — NOT fall through to the catch-all and render the index page.
    /// The fix rides on the SEC-2 `canonicalize_path` trailing-slash strip:
    /// `/about/` → `/about` BEFORE matching, so the literal hits and the same
    /// canonical path is forwarded to the worker (no auth/forward desync).
    #[test]
    fn ssg_trailing_slash_resolves_to_static_resource() {
        let mut resources = HashMap::new();
        // The static page (what `vite build` emits for an SSG route).
        resources.insert(
            "/about".into(),
            ResourceEntry {
                r#static: Some(StaticAction {
                    r#try: vec!["/about.html".into()],
                }),
                ..Default::default()
            },
        );
        // The SPA/index catch-all fallback.
        resources.insert(
            "/[...rest]".into(),
            ResourceEntry {
                r#static: Some(StaticAction {
                    r#try: vec!["$path".into(), "/index.html".into()],
                }),
                ..Default::default()
            },
        );
        let m = Manifest {
            version: 1,
            resources,
            ..Manifest::default()
        };
        let c = CompiledManifest::compile(&m);

        // Without the slash, the literal resolves (baseline).
        assert_eq!(
            c.lookup_resource_key("/about").as_deref(),
            Some("/about"),
            "the literal /about resource resolves directly"
        );
        // WITH the trailing slash, it must STILL resolve to /about — not the
        // catch-all. This is the ISS-60 bug: pre-canonicalization `/about/`
        // missed the literal and fell through to `/[...rest]` (the index page).
        assert_eq!(
            c.lookup_resource_key("/about/").as_deref(),
            Some("/about"),
            "/about/ (trailing slash) must resolve to the /about static resource, \
             not the SPA/index catch-all"
        );
        // The matched policy carries the static action for the page, not the
        // catch-all's `$path`/index try-chain.
        let policy = c.lookup_resource("/about/").expect("policy resolves");
        match &policy.action {
            ResolvedAction::Static { try_chain } => assert_eq!(
                try_chain,
                &vec!["/about.html".to_string()],
                "/about/ serves the about page's try-chain, not the index fallback"
            ),
            other => panic!("expected the /about static action, got {other:?}"),
        }
        // An unrelated path still falls through to the catch-all (no regression).
        assert_eq!(
            c.lookup_resource_key("/nonexistent/page").as_deref(),
            Some("/[...rest]"),
            "unrelated paths still reach the SPA/index catch-all"
        );
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
        let p = c.lookup_resource("/__zeroship/v1/todos.list").expect("matches");
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
        let p = c.lookup_resource("/__zeroship/v1/todos.list").expect("matches");
        assert_eq!(p.auth, AuthLevel::User, "stricter user beats root anon");
    }

    /// SEC-5 fail-closed default: an RPC procedure whose entire inheritance
    /// chain declares NO `auth` must resolve to `User`, never the silent `Anon`
    /// that turned a forgotten/typo'd policy into an unauthenticated exposure.
    /// Pre-flip this resolved to `Anon` → RED.
    #[test]
    fn rpc_procedure_defaults_to_user_when_no_auth_declared() {
        let mut resources = HashMap::new();
        resources.insert("rpc:todos.list".into(), rpc_entry(ProcedureKind::Query));
        let m = Manifest {
            version: 1,
            resources,
            ..Manifest::default()
        };
        let c = CompiledManifest::compile(&m);
        let p = c
            .lookup_resource("/__zeroship/v1/todos.list")
            .expect("matches");
        assert_eq!(
            p.auth,
            AuthLevel::User,
            "an undeclared rpc procedure must fail closed to user, not anon"
        );
        assert!(
            !p.publicly_accessible,
            "the fail-closed default is not publicly accessible"
        );
    }

    /// The web surface keeps the public-by-default norm — only the `rpc:` API
    /// flips. A URL/SSR resource with no declared auth stays `Anon` so a
    /// creator's blog/landing/static assets remain readable without login.
    #[test]
    fn url_resource_keeps_public_default_when_no_auth_declared() {
        let mut resources = HashMap::new();
        resources.insert("/blog".into(), ResourceEntry::default());
        let m = Manifest {
            version: 1,
            resources,
            ..Manifest::default()
        };
        let c = CompiledManifest::compile(&m);
        let p = c.lookup_resource("/blog").expect("matches");
        assert_eq!(
            p.auth,
            AuthLevel::Anon,
            "url/web resources keep public-by-default"
        );
    }

    /// A deliberately public procedure (its family declares `auth: anon` +
    /// `publicly_accessible: true`) stays `Anon`. The fail-closed default only
    /// fires when NOTHING in the chain declares auth, so an explicit public
    /// opt-in is preserved.
    #[test]
    fn rpc_procedure_explicit_public_stays_anon() {
        let mut resources = HashMap::new();
        resources.insert(
            "rpc:wizard".into(),
            ResourceEntry {
                auth: Some(AuthLevel::Anon),
                publicly_accessible: Some(true),
                ..Default::default()
            },
        );
        resources.insert("rpc:wizard.suggest".into(), rpc_entry(ProcedureKind::Action));
        let m = Manifest {
            version: 1,
            resources,
            ..Manifest::default()
        };
        let c = CompiledManifest::compile(&m);
        let p = c
            .lookup_resource("/__zeroship/v1/wizard.suggest")
            .expect("matches");
        assert_eq!(
            p.auth,
            AuthLevel::Anon,
            "explicit anon+publicly_accessible family keeps the procedure public"
        );
        assert!(p.publicly_accessible);
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
        let p = c.lookup_resource("/__zeroship/v1/todos.add").expect("matches");
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
        let p = c.lookup_resource("/__zeroship/v1/billing.charge").expect("matches");
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
    fn effective_policy_required_scopes_compiles_from_manifest() {
        // A route declaring `required_scopes` compiles them onto the
        // EffectivePolicy verbatim (auth-sdk Slice 3c, §5.3). This is the
        // value the gateway auth gate checks the principal's scopes against.
        let mut resources = HashMap::new();
        resources.insert(
            "rpc:billing.read".into(),
            ResourceEntry {
                kind: Some(ProcedureKind::Query),
                auth: Some(AuthLevel::User),
                required_scopes: vec!["read:billing".into()],
                ..Default::default()
            },
        );
        let m = Manifest {
            version: 1,
            resources,
            ..Manifest::default()
        };
        let c = CompiledManifest::compile(&m);
        let p = c.lookup_resource("/__zeroship/v1/billing.read").expect("matches");
        assert_eq!(p.required_scopes, vec!["read:billing".to_string()]);
        assert_eq!(p.auth, AuthLevel::User);
    }

    #[test]
    fn effective_policy_required_scopes_union_along_chain() {
        // required_scopes accumulate by UNION root → child: a scoped parent's
        // requirement is inherited and the child only ADDS. A child can never
        // weaken the gate (no `override`), and duplicates are deduped.
        let mut resources = HashMap::new();
        resources.insert(
            "*".into(),
            ResourceEntry {
                auth: Some(AuthLevel::User),
                required_scopes: vec!["openid".into()],
                ..Default::default()
            },
        );
        resources.insert(
            "rpc:billing".into(),
            ResourceEntry {
                required_scopes: vec!["read:billing".into(), "openid".into()],
                ..Default::default()
            },
        );
        resources.insert(
            "rpc:billing.charge".into(),
            ResourceEntry {
                kind: Some(ProcedureKind::Mutation),
                required_scopes: vec!["write:billing".into()],
                ..Default::default()
            },
        );
        let m = Manifest {
            version: 1,
            resources,
            ..Manifest::default()
        };
        let c = CompiledManifest::compile(&m);
        let p = c
            .lookup_resource("/__zeroship/v1/billing.charge")
            .expect("matches");
        // Root openid + parent read:billing (+ openid deduped) + child write:billing.
        assert_eq!(
            p.required_scopes,
            vec![
                "openid".to_string(),
                "read:billing".to_string(),
                "write:billing".to_string()
            ],
            "scopes union along chain, deduped, in root→child order"
        );
    }

    #[test]
    fn effective_policy_required_scopes_default_empty() {
        // A route with no `required_scopes` compiles to an empty vec — the
        // gateway treats that as "no scope gate" (unchanged behavior).
        let mut resources = HashMap::new();
        resources.insert("rpc:todos.list".into(), rpc_entry(ProcedureKind::Query));
        let m = Manifest {
            version: 1,
            resources,
            ..Manifest::default()
        };
        let c = CompiledManifest::compile(&m);
        let p = c.lookup_resource("/__zeroship/v1/todos.list").expect("matches");
        assert!(p.required_scopes.is_empty());
    }

    #[test]
    fn passthrough_resolves_to_worker_ssr_via_root_star() {
        let m = Manifest::passthrough();
        let c = CompiledManifest::compile(&m);
        // `*` doesn't dispatch; nothing matches by URL.
        assert!(c.lookup_resource("/anything").is_none());
        assert!(c.lookup_resource("/__zeroship/v1/anything").is_none());
    }
}
