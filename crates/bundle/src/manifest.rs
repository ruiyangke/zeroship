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

    /// Per-app auth declaration. Carries the app's CUSTOM OAuth scopes
    /// (`auth.scopes`) the consent screen offers end users. Omitted on the
    /// wire when empty (`AuthConfig::is_empty`), so a manifest that declares
    /// no scopes serializes without an `auth` key and round-trips unchanged.
    ///
    /// Declared scopes are end-user (namespace-(b)) scopes — self-grantable,
    /// never platform-delegated. On deploy the control plane validates them
    /// (format + platform-vocab collision), persists them to
    /// `control.app_scope_defs`, and mirrors the allowlist into the per-app
    /// OAuth client `scope` — all atomically (auth-sdk Slice 3, spec §5.1).
    #[serde(default, skip_serializing_if = "AuthConfig::is_empty")]
    pub auth: AuthConfig,

    /// Inert creator-authored outbound TCP request hints. These are NOT grants
    /// and never affect worker enforcement directly; control diffs them against
    /// the operator-owned grant table to surface pending review.
    #[serde(default, skip_serializing_if = "NetConfig::is_empty")]
    pub net: NetConfig,

    /// Informational; not load-bearing on the hot path.
    #[serde(default)]
    pub metadata: ManifestMetadata,

    /// Build-time export discovery.
    ///
    /// **Deprecated as of Stage 5c (ZS-standard refactor).** The runtime
    /// no longer reads any field here; schema install now uses the generated
    /// runtime descriptor carried by `runtime_descriptor`. The Vite plugin no
    /// longer writes this field. Kept on the wire so older archives that carried
    /// `exports` still deserialize cleanly during local upgrade; a future stage
    /// removes the field entirely.
    ///
    /// Additive on the wire: an old manifest without `exports`
    /// deserializes unchanged, and a fresh build omits the field
    /// entirely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exports: Option<ManifestExports>,

    /// The generated **runtime schema descriptor** carried by the `.zship`,
    /// content-addressed like worker modules and assets (`{hash}`).
    ///
    /// This is the `schema.runtime.json` artifact `gen-types` emits by folding
    /// the migration set — a `Record<collection, Record<column, FieldDef>>` that
    /// formalises what the runtime's `normalizeSchema` produces. In the
    /// migration-service cutover, the runtime reads this descriptor instead of
    /// any schema declared on the user module. Migration documents are applied
    /// through the migration service and are not carried by the `.zship`.
    ///
    /// `None` (the default; `skip_serializing_if`) is valid for schema-less apps.
    ///
    /// `validate()` enforces the blob-hash format only; the descriptor's JSON
    /// shape is the producer's (`gen-types`) and consumer's (P4b runtime) contract,
    /// not this struct's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_descriptor: Option<RuntimeDescriptorEntry>,
}

/// The generated runtime schema descriptor carried by a `.zship`
/// (`manifest.runtime_descriptor`).
///
/// `hash` is the sha256 of the `schema.runtime.json` blob body. The descriptor is
/// a single anonymous artifact reconstructed from its blob alone; migration
/// documents are applied through the migration service and are not represented
/// in the manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeDescriptorEntry {
    /// sha256 hash (lowercase, 64 hex chars) of the `schema.runtime.json` blob.
    pub hash: String,
}

/// One logical migration file entry produced by the migration build frontend.
///
/// Migration documents are applied through the standalone migration service and
/// are not carried in [`Manifest`]. The type remains in this crate because the
/// frozen migration engine uses it as a shared build-artifact record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MigrationFileEntry {
    /// Logical migration filename, e.g. `20240617123000_create_users.ir.json`.
    pub name: String,
    /// sha256 hash (lowercase, 64 hex chars) of the migration file body.
    pub hash: String,
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
            auth: AuthConfig::default(),
            net: NetConfig::default(),
            metadata: ManifestMetadata::default(),
            exports: None,
            runtime_descriptor: None,
        }
    }
}

/// Per-app auth declaration carried on the manifest. Today it holds only the
/// app's declared OAuth scopes (`scopes`); future auth-related declarations
/// (e.g. relay-email policy) extend this struct.
///
/// Empty by default; serializes to nothing when empty (`is_empty`) so a
/// manifest that declares no scopes round-trips identically to one with no
/// `auth` key at all.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct AuthConfig {
    /// App-declared custom end-user scopes (namespace-(b), spec §5.1). Each
    /// is `{ id, label, description }`. The consent screen renders these with
    /// their `label`/`description`; they are self-grantable by the end user.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<ScopeDef>,
}

impl AuthConfig {
    /// True when nothing is declared — drives `skip_serializing_if` so an
    /// app with no auth declaration omits the `auth` key entirely.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.scopes.is_empty()
    }
}

/// Inert outbound raw-TCP request hints carried by the manifest.
///
/// These entries are creator-authored and therefore never become enforcement
/// policy by themselves. Control surfaces them as pending review until an
/// operator writes the corresponding `app_net_grants` table row.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct NetConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requests: Vec<NetRequest>,
}

impl NetConfig {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.requests.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetRequest {
    pub host: String,
    pub port: u16,
    pub reason: String,
}

impl NetRequest {
    fn validate(&self) -> Result<(), String> {
        let host = self.host.trim();
        if host.is_empty() {
            return Err("net.requests host must not be empty".to_string());
        }
        if self.port == 0 {
            return Err("net.requests port must be between 1 and 65535".to_string());
        }
        let star_count = host.bytes().filter(|b| *b == b'*').count();
        if host == "*" {
            return Err("net.requests host cannot be bare '*'".to_string());
        }
        if star_count > 0 && !host.starts_with("*.") {
            return Err(format!(
                "net.requests host {host:?} must use the '*.example.com' wildcard form"
            ));
        }
        if self.reason.trim().is_empty() {
            return Err("net.requests reason must not be empty".to_string());
        }
        Ok(())
    }
}

/// One app-declared custom scope (`manifest.auth.scopes[i]`).
///
/// `id` follows the `verb:resource` convention (lowercase, `:`-segmented),
/// e.g. `read:billing`. `label` + `description` are what the consent screen
/// renders. Format is validated by [`ScopeDef::validate_id_format`]; the
/// control plane additionally rejects ids that collide with the closed
/// platform-scope vocabulary (spec §5.1) — that collision check lives in the
/// control crate because only it depends on `zeroship_authz`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScopeDef {
    /// Scope id, e.g. `read:billing`. `verb:resource`, lowercase.
    pub id: String,
    /// Short human label rendered on the consent screen.
    pub label: String,
    /// Longer description of what granting the scope allows. Optional;
    /// omitted on the wire when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl ScopeDef {
    /// Validate a scope id's FORMAT (not platform-vocab collision — that is
    /// the control plane's job, spec §5.1). The id must match
    /// `^[a-z][a-z0-9_]*(:[a-z][a-z0-9_]*)*$`: one or more `:`-separated
    /// segments, each starting with a lowercase letter then lowercase
    /// alphanumerics / underscores.
    ///
    /// Returns the offending reason on failure so the deploy error is
    /// actionable.
    ///
    /// # Errors
    /// A human-readable message when `id` is malformed.
    pub fn validate_id_format(id: &str) -> Result<(), String> {
        if id.is_empty() {
            return Err("scope id is empty".to_string());
        }
        for segment in id.split(':') {
            let mut chars = segment.chars();
            match chars.next() {
                Some(c) if c.is_ascii_lowercase() => {}
                _ => {
                    return Err(format!(
                        "scope id {id:?} segment {segment:?} must start with a lowercase letter"
                    ));
                }
            }
            for c in chars {
                if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
                    return Err(format!(
                        "scope id {id:?} segment {segment:?} may contain only [a-z0-9_]"
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Build-time export discovery — **deprecated as of Stage 5c.**
///
/// The runtime no longer reads any field here. Schema install now uses
/// `Manifest::runtime_descriptor`, produced from the migration fold. The Vite
/// plugin no longer writes this struct. The type is retained on the wire so
/// older `.zship` archives that included it still deserialize cleanly during
/// local upgrade; a future stage removes it.
///
/// Wire shape stays permissive — both fields are optional and
/// serialize to nothing when empty.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ManifestExports {
    /// **Deprecated as of Stage 5c.** Kept for older archives; future stage
    /// removes. Historically: bundle-relative POSIX path of the module whose
    /// default export held the DB schema.
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
            auth: AuthConfig::default(),
            net: NetConfig::default(),
            metadata: ManifestMetadata {
                compiler: Some(format!(
                    "zeroship-passthrough@{}",
                    env!("CARGO_PKG_VERSION")
                )),
                built_at: "1970-01-01T00:00:00Z".to_string(),
            },
            exports: None,
            runtime_descriptor: None,
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
        // Declared scope ids must be well-formed (`verb:resource`, lowercase).
        // Platform-vocabulary collision is rejected separately by the control
        // plane on deploy (spec §5.1) — it has the `zeroship_authz::Scope`
        // dependency this crate deliberately does not.
        for scope in &self.auth.scopes {
            ScopeDef::validate_id_format(&scope.id)?;
        }
        for request in &self.net.requests {
            request.validate()?;
        }
        // The `schema.runtime.json` blob is content-addressed like every other
        // bundle blob. The descriptor's JSON shape is checked by the runtime
        // when it injects `__zsRuntimeDescriptor`.
        if let Some(desc) = &self.runtime_descriptor {
            if !is_sha256_hex(&desc.hash) {
                return Err(format!(
                    "runtime_descriptor.hash {hash:?} is not a lowercase 64-char sha256 hex",
                    hash = desc.hash
                ));
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
            // `required_scopes` must be well-formed AND declared. An id that
            // is malformed, or references neither a `manifest.auth.scopes`
            // ScopeDef nor a reserved identity scope, can never appear in any
            // principal's grant — so the route would hard-403 every request
            // forever and still pass validate(). Catch it at deploy time.
            // (Declared scopes are format-checked separately above; here we
            // re-check format so a typo in `required_scopes` is rejected with
            // a route-scoped message rather than slipping through.)
            for scope in &entry.required_scopes {
                ScopeDef::validate_id_format(scope).map_err(|e| {
                    format!("resource {key:?}: required_scopes id {scope:?} is malformed: {e}")
                })?;
                let declared = self.auth.scopes.iter().any(|s| &s.id == scope);
                if !declared && !is_reserved_identity_scope(scope) {
                    return Err(format!(
                        "resource {key:?}: required_scopes id {scope:?} is not declared in \
                         manifest.auth.scopes and is not a reserved identity scope \
                         (openid, profile, email, offline_access)"
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

/// The reserved OIDC identity scopes every issuer grants implicitly. A
/// route may demand these in `required_scopes` without declaring them in
/// `manifest.auth.scopes` (they are platform-issued, not app-declared).
/// Mirrors the OIDC core scope set plus `offline_access` (refresh tokens).
fn is_reserved_identity_scope(s: &str) -> bool {
    matches!(s, "openid" | "profile" | "email" | "offline_access")
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

#[cfg(test)]
mod runtime_descriptor_validation_tests {
    use super::*;

    fn base() -> Manifest {
        Manifest {
            metadata: ManifestMetadata {
                compiler: Some("test".into()),
                built_at: "2026-04-29T00:00:00Z".into(),
            },
            ..Manifest::default()
        }
    }

    #[test]
    fn runtime_descriptor_defaults_none_and_omitted_on_wire() {
        let m = base();
        assert!(m.runtime_descriptor.is_none());
        let json = serde_json::to_string(&m).unwrap();
        assert!(
            !json.contains("runtime_descriptor"),
            "absent descriptor must be omitted on the wire: {json}"
        );
        // A manifest without `runtime_descriptor` deserializes to None.
        let back: Manifest = serde_json::from_str(&json).unwrap();
        assert!(back.runtime_descriptor.is_none());
    }

    #[test]
    fn runtime_descriptor_round_trips_byte_identical() {
        let mut m = base();
        let hash = "a".repeat(64);
        m.runtime_descriptor = Some(RuntimeDescriptorEntry { hash: hash.clone() });
        m.validate().expect("valid descriptor accepted");
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("runtime_descriptor"), "descriptor must serialize: {json}");
        let back: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.runtime_descriptor, m.runtime_descriptor,
            "runtime_descriptor must round-trip byte-identical"
        );
        assert_eq!(back.runtime_descriptor.unwrap().hash, hash);
    }

    #[test]
    fn rejects_bad_runtime_descriptor_hash() {
        let mut m = base();
        m.runtime_descriptor = Some(RuntimeDescriptorEntry {
            hash: "NOTAHASH".into(),
        });
        let err = m.validate().unwrap_err();
        assert!(
            err.contains("runtime_descriptor.hash") && err.contains("sha256"),
            "malformed descriptor hash must be rejected, got {err}"
        );
    }
}
