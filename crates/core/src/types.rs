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
}

// ---------------------------------------------------------------------------
// Routing manifest — the per-app dispatch table the gateway walks.
//
// Lives alongside the app's deploy in the control plane. Synced to the
// gateway with the rest of the route data. The build adapter
// (@zeroship/vite-plugin) emits this from the project's vite config
// + framework conventions.
// ---------------------------------------------------------------------------

/// Reference to the worker-side JS for a deploy. Uniform shape: an
/// `entry` specifier plus a `modules` map of specifier → blob hash.
/// Single-bundled servers have one entry in `modules`; code-split
/// servers have many. See `docs/reference/zsapp.md` Worker code section.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerCode {
    /// Specifier V8 evaluates first; must be a key in `modules`.
    pub entry: String,
    /// Specifier → blob hash.
    pub modules: HashMap<String, String>,
}

/// One per app. Carries everything the gateway needs to route a request
/// without consulting the control plane on the hot path. Wire format:
/// see `docs/reference/zsapp.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// Schema version. Reject unknown values.
    ///
    /// * `1` — initial published shape: `resources` map is the source
    ///   of truth. Future breaking changes bump to `2`.
    #[serde(default = "Manifest::default_version")]
    pub version: u16,

    /// Computed from the canonical (deploy_hash-omitted) manifest by the
    /// control plane on receipt. `None` at build time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deploy_hash: Option<String>,

    /// Worker-side JS for the deploy. `None` for SSG-only deploys
    /// (no worker rules). See [`WorkerCode`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker: Option<WorkerCode>,

    /// Unified resource map. Keys are `"rpc:<wireId>"` or `"/<path>"`
    /// or `"*"`.
    ///
    /// See `docs/proposals/rpc-v2.md` §7 for the full shape and merge
    /// semantics. The gateway compiles this into per-resource
    /// `EffectivePolicy` records at app-load time so per-request lookup
    /// is a single `HashMap::get`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub resources: HashMap<String, ResourceEntry>,

    /// JSONSchemas keyed by `"sha256:<hex>"`, referenced by
    /// `ResourceEntry.input_schema` / `output_schema`. Populated by
    /// the build's TS-types-to-JSONSchema pass.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub schemas: HashMap<String, serde_json::Value>,

    /// Wire-id stability map: `<filePath>::<exportName>` → `"rpc:<wireId>"`.
    /// Used by the build (carried forward across builds to keep the
    /// wire stable when files are renamed). Gateway parses + ignores.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub aliases: HashMap<String, String>,

    /// JSON transformer for the wire (e.g. `"superjson"`, `"json"`). v3+.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transformer: Option<String>,

    /// Build-time static asset map. Populated on deploy; immutable
    /// until the next deploy. `path → asset entry` (path is the URL
    /// the asset is served at, e.g. `/index.html`).
    #[serde(default)]
    pub assets: HashMap<String, AssetEntry>,

    /// Runtime-emitted asset map. Populated by the user's server
    /// code via `zeroship.assets.put(...)`. Bumped via
    /// `asset_version` on each mutation; gateway re-syncs when the
    /// version changes.
    #[serde(default)]
    pub runtime_assets: HashMap<String, AssetEntry>,

    /// Bumped on every `runtime_assets` mutation. Gateway compares
    /// local vs remote and refetches the runtime map only when this
    /// changes — keeps the hot path cheap.
    #[serde(default)]
    pub asset_version: i64,

    /// Asset hash → sourcemap blob hash. Optional, may be `{}`.
    #[serde(default)]
    pub sourcemaps: HashMap<String, String>,

    /// Informational; not load-bearing on the hot path.
    #[serde(default)]
    pub metadata: ManifestMetadata,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            version: 1,
            deploy_hash: None,
            worker: None,
            resources: HashMap::new(),
            schemas: HashMap::new(),
            aliases: HashMap::new(),
            transformer: None,
            assets: HashMap::new(),
            runtime_assets: HashMap::new(),
            asset_version: 0,
            sourcemaps: HashMap::new(),
            metadata: ManifestMetadata::default(),
        }
    }
}

/// Informational metadata about how the manifest was produced.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ManifestMetadata {
    /// Compiler identifier, e.g. `@zeroship/vite-plugin@0.x`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compiler: Option<String>,
    /// RFC 3339 build timestamp.
    #[serde(default)]
    pub built_at: String,
}

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

impl Manifest {
    /// Default schema version for `#[serde(default)]`.
    ///
    /// v1 is the initial published shape — see `docs/proposals/rpc-v2.md`
    /// §7. Future breaking changes bump to v2.
    fn default_version() -> u16 { 1 }

    /// Synthesize the default "everything goes to the worker" manifest.
    /// Used for apps that haven't yet shipped a manifest of their own.
    /// One catch-all `*` resource entry keeps every URL path landing on
    /// the worker as SSR; an `anon` policy with `publicly_accessible: true`
    /// satisfies the secure-by-default check. Synthesized rather than
    /// built — `metadata.built_at` is the fixed epoch sentinel so it's
    /// recognizable.
    pub fn passthrough() -> Self {
        let mut resources: HashMap<String, ResourceEntry> = HashMap::new();
        resources.insert(
            "*".to_string(),
            ResourceEntry {
                auth: Some(AuthLevel::Anon),
                publicly_accessible: Some(true),
                ..Default::default()
            },
        );
        Self {
            version: 1,
            deploy_hash: None,
            worker: None,
            resources,
            schemas: HashMap::new(),
            aliases: HashMap::new(),
            transformer: Some("superjson".into()),
            assets: HashMap::new(),
            runtime_assets: HashMap::new(),
            asset_version: 0,
            sourcemaps: HashMap::new(),
            metadata: ManifestMetadata {
                compiler: Some(format!(
                    "zeroship-passthrough@{}",
                    env!("CARGO_PKG_VERSION")
                )),
                built_at: "1970-01-01T00:00:00Z".to_string(),
            },
        }
    }

    /// Validate manifest invariants. Returns the first violation as a
    /// human-readable message. Cheap — call on every load.
    ///
    /// Checks:
    /// * `version == 1` (unknown versions rejected with a clear error).
    /// * `worker.entry` is a key in `worker.modules`; every value in
    ///   `worker.modules` is 64-char lowercase sha256 hex.
    /// * Every `sourcemaps` key/value is sha256-hex (lowercase, 64 chars).
    /// * `resources` keys conform to the `rpc:` / URL / `*` shape.
    /// * Each resource has at most one routing action.
    /// * Override marker required when shadowing an inherited field.
    /// * `auth: anon` requires `publicly_accessible: true`.
    /// * Schema-hash references match `sha256:[0-9a-f]{64}` and
    ///   exist in `manifest.schemas`.
    ///
    /// Note: `runtime_assets == {}` and `asset_version == 0` are
    /// fresh-deploy invariants, NOT type-level ones. `validate()` is
    /// also called on already-deployed manifests with mutated runtime
    /// state, so we don't reject those here.
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err(format!(
                "unsupported manifest version {}: only version 1 is accepted",
                self.version
            ));
        }
        if let Some(WorkerCode { entry, modules }) = &self.worker {
            if !modules.contains_key(entry) {
                return Err(format!(
                    "worker.entry {entry:?} is not a key in worker.modules"
                ));
            }
            for (spec, hash) in modules {
                if !crate::blob::validate_hash_format(hash) {
                    return Err(format!(
                        "worker.modules[{spec}] {hash:?} is not a 64-char lowercase sha256 hex"
                    ));
                }
            }
        }
        for (k, v) in &self.sourcemaps {
            if !is_sha256_hex(k) {
                return Err(format!(
                    "sourcemaps key {k:?} is not a lowercase 64-char sha256 hex"
                ));
            }
            if !is_sha256_hex(v) {
                return Err(format!(
                    "sourcemaps[{k}] value {v:?} is not a lowercase 64-char sha256 hex"
                ));
            }
        }
        // Validate per-asset compression variants. v1 supports `br`
        // and `gzip`; unknown encoding tokens are rejected so a typo
        // can't silently disable a variant on the wire.
        for (path, entry) in &self.assets {
            for (enc, variant) in &entry.variants {
                if !is_supported_variant_encoding(enc) {
                    return Err(format!(
                        "assets[{path}].variants[{enc:?}]: unknown encoding (allowed: br, gzip)"
                    ));
                }
                if !is_sha256_hex(&variant.hash) {
                    return Err(format!(
                        "assets[{path}].variants[{enc}].hash {hash:?} is not a lowercase 64-char sha256 hex",
                        hash = variant.hash
                    ));
                }
            }
        }
        for (path, entry) in &self.runtime_assets {
            for (enc, variant) in &entry.variants {
                if !is_supported_variant_encoding(enc) {
                    return Err(format!(
                        "runtime_assets[{path}].variants[{enc:?}]: unknown encoding (allowed: br, gzip)"
                    ));
                }
                if !is_sha256_hex(&variant.hash) {
                    return Err(format!(
                        "runtime_assets[{path}].variants[{enc}].hash {hash:?} is not a lowercase 64-char sha256 hex",
                        hash = variant.hash
                    ));
                }
            }
        }
        if !self.resources.is_empty() {
            self.validate_resources()?;
        }
        Ok(())
    }

    /// Resource-tree checks. Run only when the resource map is non-empty.
    fn validate_resources(&self) -> Result<(), String> {
        // 1. Per-entry well-formedness.
        for (key, entry) in &self.resources {
            if !is_valid_resource_key(key) {
                return Err(format!(
                    "resource key {key:?} is malformed: must be \"*\", \"/<path>\", or \"rpc:<id>\""
                ));
            }
            // Routing-action exclusivity: at most one of redirect/rewrite/static.
            let action_count = (entry.redirect.is_some() as u8)
                + (entry.rewrite.is_some() as u8)
                + (entry.r#static.is_some() as u8);
            if action_count > 1 {
                return Err(format!(
                    "resource {key:?}: at most one of `redirect`, `rewrite`, `static` may be set"
                ));
            }
            // Secure-by-default.
            if entry.auth == Some(AuthLevel::Anon) && entry.publicly_accessible != Some(true) {
                return Err(format!(
                    "resource {key:?}: `auth: anon` requires `publicly_accessible: true`"
                ));
            }
            // Redirect status must be 3xx.
            if let Some(r) = &entry.redirect {
                if !(300..400).contains(&r.status) {
                    return Err(format!(
                        "resource {key:?}: redirect.status {} must be 3xx",
                        r.status
                    ));
                }
            }
            // Idempotency TTL band: spec §8 — [1, 168] hours.
            if let Some(h) = entry.idempotency_ttl_hours {
                if !(1..=168).contains(&h) {
                    return Err(format!(
                        "resource {key:?}: idempotency_ttl_hours {h} out of band [1, 168]"
                    ));
                }
            }
            // Schema hash format + existence.
            if let Some(s) = &entry.input_schema {
                if !is_schema_ref(s) {
                    return Err(format!(
                        "resource {key:?}: input_schema {s:?} must match `sha256:[0-9a-f]{{64}}`"
                    ));
                }
                if !self.schemas.contains_key(s) {
                    return Err(format!(
                        "resource {key:?}: input_schema {s:?} not present in manifest.schemas"
                    ));
                }
            }
            if let Some(s) = &entry.output_schema {
                if !is_schema_ref(s) {
                    return Err(format!(
                        "resource {key:?}: output_schema {s:?} must match `sha256:[0-9a-f]{{64}}`"
                    ));
                }
                if !self.schemas.contains_key(s) {
                    return Err(format!(
                        "resource {key:?}: output_schema {s:?} not present in manifest.schemas"
                    ));
                }
            }
        }
        // 2. Override-marker presence — every shadowed field needs an
        //    explicit `override: [field]` on the child. Walk the
        //    inheritance chain (root `*` → ancestors → self) for each
        //    resource and check that any field declared on the child
        //    that is also declared on an ancestor is listed in the
        //    child's override.
        for (key, entry) in &self.resources {
            let chain = inheritance_chain(key, &self.resources);
            // Bail if the chain didn't terminate at the expected depth
            // (defense in depth — keys are structural so cycles are
            // impossible, but check anyway).
            if chain.len() > 16 {
                return Err(format!(
                    "resource {key:?}: inheritance chain exceeded depth 16"
                ));
            }
            // The chain is root → … → self. Iterate ancestors only.
            for ancestor_key in chain.iter().rev().skip(1) {
                let Some(ancestor) = self.resources.get(ancestor_key) else { continue };
                for field in shadowed_fields(entry, ancestor) {
                    if !entry.r#override.iter().any(|f| f == field) {
                        return Err(format!(
                            "resource {key:?} declares `{field}` already declared by ancestor \
                             {ancestor_key:?}; add `override: [\"{field}\"]` to confirm"
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

/// Resource-key syntax check. Three legal shapes:
/// * `*` — root default policy.
/// * `/<path>` — URL namespace (path-segment globs allowed).
/// * `rpc:<id>` — RPC namespace; `id` is `[a-zA-Z0-9._*-]+`.
fn is_valid_resource_key(s: &str) -> bool {
    if s == "*" {
        return true;
    }
    if let Some(rpc_id) = s.strip_prefix("rpc:") {
        if rpc_id.is_empty() {
            return false;
        }
        return rpc_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '*' | '-'));
    }
    s.starts_with('/')
}

fn is_schema_ref(s: &str) -> bool {
    if let Some(hex) = s.strip_prefix("sha256:") {
        is_sha256_hex(hex)
    } else {
        false
    }
}

/// Build the inheritance chain root → … → key. Empty when `key` is
/// `*` (root has no ancestors). Walks dot segments for `rpc:`, path
/// segments for URL, `*` for both. Always ends at `*` if it's
/// declared, then bottoms out at `key` itself.
fn inheritance_chain(key: &str, resources: &HashMap<String, ResourceEntry>) -> Vec<String> {
    let mut chain: Vec<String> = Vec::new();
    if key == "*" {
        return vec!["*".to_string()];
    }
    if resources.contains_key("*") {
        chain.push("*".to_string());
    }
    if let Some(rpc_id) = key.strip_prefix("rpc:") {
        // dot-segment ancestors: "todos.add" → "todos"
        let segs: Vec<&str> = rpc_id.split('.').collect();
        for end in 1..segs.len() {
            let ancestor = format!("rpc:{}", segs[..end].join("."));
            if resources.contains_key(&ancestor) {
                chain.push(ancestor);
            }
        }
    } else if key.starts_with('/') {
        // Path-segment ancestors: "/api/admin/users" → "/api/admin", "/api"
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

/// Names of fields the child declares that an ancestor also declares.
/// Used by the override-marker check to enforce explicit shadowing.
fn shadowed_fields<'a>(child: &'a ResourceEntry, ancestor: &'a ResourceEntry) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    if child.auth.is_some() && ancestor.auth.is_some() {
        out.push("auth");
    }
    if child.rate_limit.is_some() && ancestor.rate_limit.is_some() {
        out.push("rate_limit");
    }
    if child.cors.is_some() && ancestor.cors.is_some() {
        out.push("cors");
    }
    if child.cache.is_some() && ancestor.cache.is_some() {
        out.push("cache");
    }
    if child.csrf_origins.is_some() && ancestor.csrf_origins.is_some() {
        out.push("csrf_origins");
    }
    if child.idempotent.is_some() && ancestor.idempotent.is_some() {
        out.push("idempotent");
    }
    if child.idempotency_ttl_hours.is_some() && ancestor.idempotency_ttl_hours.is_some() {
        out.push("idempotency_ttl_hours");
    }
    if child.max_input_bytes.is_some() && ancestor.max_input_bytes.is_some() {
        out.push("max_input_bytes");
    }
    if child.publicly_accessible.is_some() && ancestor.publicly_accessible.is_some() {
        out.push("publicly_accessible");
    }
    out
}

/// Permitted `Content-Encoding` tokens for `AssetEntry::variants`.
/// `identity` is intentionally NOT a variant — it's the default when
/// no `Accept-Encoding` match is found.
fn is_supported_variant_encoding(s: &str) -> bool {
    matches!(s, "br" | "gzip")
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

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
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

/// One asset blob, content-addressed. Stored in the object store
/// keyed by `hash`; surfaced to the gateway via the manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetEntry {
    /// SHA-256 (hex) of the raw (identity) bytes. Used as the
    /// object-store key, the canonical ETag, and the default body when
    /// no `Accept-Encoding` variant is selected.
    pub hash: String,
    pub content_type: String,
    /// Identity (uncompressed) byte count. Used for `Content-Length`
    /// and quotas. Compressed variants have their own `size` in
    /// `variants[<encoding>].size`.
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
    /// Pre-compressed encoding variants. Keys are HTTP
    /// `Content-Encoding` token names (`"br"`, `"gzip"`); values are
    /// the COMPRESSED hash + compressed size. Identity is always
    /// available via the parent `hash`/`size`; entries here are the
    /// alternative encodings the build pipeline emitted.
    ///
    /// The gateway negotiates via `Accept-Encoding` and serves the
    /// chosen variant's blob (with `Content-Encoding: <key>` and
    /// `Vary: Accept-Encoding` set on the response). The ETag and
    /// Content-Length reflect the variant, not the identity.
    ///
    /// Validation: variant keys MUST be one of the allowed encodings
    /// (currently `"br"` and `"gzip"`); variant hashes MUST be
    /// 64-char lowercase hex.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub variants: HashMap<String, AssetVariant>,
}

/// A pre-compressed encoding variant of an [`AssetEntry`].
///
/// Stored in `AssetEntry::variants` keyed by `Content-Encoding` token.
/// Each variant is its own content-addressed blob — the build
/// pipeline compresses the asset, hashes the compressed bytes, and
/// uploads the variant blob alongside the identity blob.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AssetVariant {
    /// SHA-256 (hex) of the compressed bytes.
    pub hash: String,
    /// Size of the compressed bytes (NOT the original).
    pub size: u64,
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
