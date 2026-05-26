use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::asset::AssetEntry;
use crate::rule::{AuthLevel, ResourceEntry};

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
/// servers have many. See `docs/reference/zship.md` Worker code section.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerCode {
    /// Specifier V8 evaluates first; must be a key in `modules`.
    pub entry: String,
    /// Specifier → blob hash.
    pub modules: HashMap<String, String>,
}

/// One per app. Carries everything the gateway needs to route a request
/// without consulting the control plane on the hot path. Wire format:
/// see `docs/reference/zship.md`.
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
    /// See `docs/proposals/rpc.md` §7 for the full shape and merge
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

    /// Build-time export discovery.
    ///
    /// **Deprecated as of Stage 5c (ZS-standard refactor).** The runtime
    /// no longer reads any field here — schema discovery now reads
    /// `default.schema` directly off the loaded entry module
    /// (`crates/runtime/src/bootstrap/db_init.js`). The Vite plugin no
    /// longer writes the field. Kept on the wire so older archives
    /// (with `exports.schema` set) still deserialize cleanly during
    /// upgrade; a future stage removes the field entirely.
    ///
    /// Additive on the wire: an old manifest without `exports`
    /// deserializes unchanged, and a fresh build omits the field
    /// entirely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exports: Option<ManifestExports>,
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
            exports: None,
        }
    }
}

/// Build-time export discovery — **deprecated as of Stage 5c.**
///
/// The runtime no longer reads any field here. Schema discovery now
/// reads `default.schema` directly off the loaded entry module
/// (`crates/runtime/src/bootstrap/db_init.js`). The Vite plugin no
/// longer writes this struct. The type is retained on the wire so
/// older `.zship` archives that included it still deserialize
/// cleanly during graceful upgrade; a future stage removes it.
///
/// Wire shape stays permissive — both fields are optional and
/// serialize to nothing when empty.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ManifestExports {
    /// **Deprecated as of Stage 5c — runtime reads `default.schema`
    /// off the entry.** Kept for graceful upgrade of older archives;
    /// future stage removes. Historically: bundle-relative POSIX
    /// path of the module whose default export held the DB schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,

    /// Reserved for future file-based handler discovery. Optional;
    /// empty Vec serializes via `skip_serializing_if` so old manifests
    /// still round-trip identically.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub handlers: Vec<HandlerEntry>,
}

/// One discovered handler module — Stage 2+ file-based discovery.
/// Stage 1 only ships the type so manifests that opt in early (e.g.
/// tests) can be parsed by older readers without an unknown-field
/// error.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HandlerEntry {
    /// Path relative to the bundle root, e.g. `"src/api/query/listTodos.ts"`.
    pub path: String,
    /// Capability bucket: `"query"` | `"mutation"` | `"action"`.
    pub capability: String,
    /// Exported handler name, e.g. `"listTodos"`.
    pub name: String,
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

impl Manifest {
    /// Default schema version for `#[serde(default)]`.
    ///
    /// v1 is the initial published shape — see `docs/proposals/rpc.md`
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
            transformer: Some("json".into()),
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
            exports: None,
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
            // Idempotency TTL band: [1, 168] hours.
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

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
}
