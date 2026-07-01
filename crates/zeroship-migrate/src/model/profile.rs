//! Declarative migration policy profiles and sealed shared-infra apply profiles.
//!
//! Stage M2 keeps today's guard implementation intact. The profile config is
//! loaded strictly and resolved fail-closed, while [`SealedProfile`] is the new
//! structural carrier the future control-plane path will pass to the engine.

use std::time::Duration;

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::conn::ExecutorConfig;
use crate::guard::GuardConfig;
use crate::model::capability::{SealApplier, VendorCapabilities};

type HmacSha256 = Hmac<Sha256>;

/// Embedded least-privilege profile. This is also the fail-closed fallback.
pub const CONFINED_PROFILE_TOML: &str = r#"
[capabilities]
extension = false
schema = false
role = { allow = false, attrs = [] }
grant = false
rls = false
policy = false
function = false
raw_sql = false
raw_view_body = false
materialized_view = false
cross_schema = false
extensions = []
schemas = []

[operational]
index_creation = "allow_blocking"
lock_timeout_ms = 3000
statement_timeout_ms = 60000
table_rewrite = "allow"

[data_security]
require_rls = false
no_hard_delete = false
sensitive_columns = []
destructive_ops = "allow"
"#;

/// Embedded platform profile. Its capability booleans match
/// [`VendorCapabilities::operator`]; schema/extension allowlists remain explicit
/// inputs and therefore default empty here.
pub const PLATFORM_PROFILE_TOML: &str = r#"
extends = "confined"

[capabilities]
extension = true
schema = true
role = { allow = true, attrs = [] }
grant = true
rls = true
policy = true
function = true
raw_sql = true
raw_view_body = true
materialized_view = true
cross_schema = true
extensions = []
schemas = []

[operational]
index_creation = "allow_blocking"
lock_timeout_ms = 3000
statement_timeout_ms = 60000
table_rewrite = "allow"

[data_security]
require_rls = false
no_hard_delete = false
sensitive_columns = []
destructive_ops = "allow"
"#;

/// A strict declarative migration policy profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyProfile {
    /// Optional single-parent profile name. Stage 1 records the authoring shape;
    /// composition is a later server task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extends: Option<String>,
    /// Permission knobs for vendor/platform DDL.
    #[serde(default)]
    pub capabilities: PolicyCapabilities,
    /// Operational execution knobs.
    #[serde(default)]
    pub operational: OperationalConfig,
    /// DDL-observable and server-enforced obligations.
    #[serde(default)]
    pub data_security: DataSecurityConfig,
}

impl Default for PolicyProfile {
    fn default() -> Self {
        Self::confined()
    }
}

impl PolicyProfile {
    /// Parse a strict TOML profile. Unknown keys are hard errors.
    ///
    /// Callers that handle missing/malformed profile input should use
    /// [`from_toml_or_confined`](Self::from_toml_or_confined) to keep the
    /// fail-closed invariant at the call boundary.
    ///
    /// # Errors
    /// Returns a TOML parse/deserialization error, including
    /// `deny_unknown_fields` failures for typos.
    pub fn from_toml(input: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(input)
    }

    /// Parse a strict JSON profile. The config surface is format-symmetric; TOML
    /// is the primary checked-in authoring form.
    ///
    /// # Errors
    /// Returns a JSON parse/deserialization error, including
    /// `deny_unknown_fields` failures for typos.
    pub fn from_json(input: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(input)
    }

    /// Fail-closed TOML loader: missing or malformed input resolves to confined.
    #[must_use]
    pub fn from_toml_or_confined(input: Option<&str>) -> Self {
        input
            .and_then(|s| Self::from_toml(s).ok())
            .unwrap_or_else(Self::confined)
    }

    /// Embedded confined preset.
    #[must_use]
    pub fn confined() -> Self {
        Self::from_toml(CONFINED_PROFILE_TOML)
            .expect("embedded confined migration policy profile is valid")
    }

    /// Embedded platform preset.
    #[must_use]
    pub fn platform() -> Self {
        Self::from_toml(PLATFORM_PROFILE_TOML)
            .expect("embedded platform migration policy profile is valid")
    }

    /// Resolve a named embedded preset. There is deliberately no `permissive`
    /// preset.
    #[must_use]
    pub fn preset(name: &str) -> Option<Self> {
        match name {
            "confined" => Some(Self::confined()),
            "platform" => Some(Self::platform()),
            _ => None,
        }
    }

    /// The profile's capability booleans lowered to the existing capability set.
    #[must_use]
    pub fn vendor_capabilities(&self) -> VendorCapabilities {
        self.capabilities.to_vendor_capabilities()
    }

    /// The fixed polarity metadata for the config surface.
    #[must_use]
    pub const fn polarity_table() -> &'static [PolicyKnobSemantics] {
        POLICY_KNOB_SEMANTICS
    }
}

/// Permission knobs in the policy profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyCapabilities {
    /// `CREATE/DROP EXTENSION`.
    #[serde(default)]
    pub extension: bool,
    /// `CREATE/DROP SCHEMA`.
    #[serde(default)]
    pub schema: bool,
    /// Role operations plus the stage-1 role-attribute allowlist placeholder.
    #[serde(default)]
    pub role: RoleCapabilityConfig,
    /// `GRANT`/`REVOKE`.
    #[serde(default)]
    pub grant: bool,
    /// RLS enable/force/disable/no-force.
    #[serde(default)]
    pub rls: bool,
    /// `CREATE/DROP POLICY`.
    #[serde(default)]
    pub policy: bool,
    /// `CREATE/DROP FUNCTION`.
    #[serde(default)]
    pub function: bool,
    /// Gated raw SQL escape.
    #[serde(default)]
    pub raw_sql: bool,
    /// Gated raw view-body escape.
    #[serde(default)]
    pub raw_view_body: bool,
    /// PostgreSQL materialized views.
    #[serde(default)]
    pub materialized_view: bool,
    /// Cross-schema references.
    #[serde(default)]
    pub cross_schema: bool,
    /// Named extension allowlist.
    #[serde(default)]
    pub extensions: Vec<String>,
    /// Schema allowlist.
    #[serde(default)]
    pub schemas: Vec<String>,
}

impl Default for PolicyCapabilities {
    fn default() -> Self {
        Self::from_vendor_capabilities(&VendorCapabilities::confined())
    }
}

impl PolicyCapabilities {
    /// Convert existing vendor capability presets into declarative config.
    #[must_use]
    pub fn from_vendor_capabilities(caps: &VendorCapabilities) -> Self {
        Self {
            extension: caps.allow_extension,
            schema: caps.allow_schema,
            role: RoleCapabilityConfig {
                allow: caps.allow_role,
                attrs: Vec::new(),
            },
            grant: caps.allow_grant,
            rls: caps.allow_rls,
            policy: caps.allow_policy,
            function: caps.allow_function,
            raw_sql: caps.allow_raw_sql,
            raw_view_body: caps.allow_raw_view_body,
            materialized_view: caps.allow_materialized_view,
            cross_schema: caps.allow_cross_schema,
            extensions: Vec::new(),
            schemas: caps.schemas.clone(),
        }
    }

    /// Lower declarative booleans to today's vendor capability set.
    #[must_use]
    pub fn to_vendor_capabilities(&self) -> VendorCapabilities {
        VendorCapabilities {
            allow_extension: self.extension,
            allow_schema: self.schema,
            allow_role: self.role.allow,
            allow_grant: self.grant,
            allow_rls: self.rls,
            allow_policy: self.policy,
            allow_function: self.function,
            allow_raw_sql: self.raw_sql,
            allow_raw_view_body: self.raw_view_body,
            allow_materialized_view: self.materialized_view,
            allow_cross_schema: self.cross_schema,
            schemas: self.schemas.clone(),
        }
    }
}

/// Role capability plus attribute allowlist placeholder.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleCapabilityConfig {
    /// Whether role DDL is allowed.
    #[serde(default)]
    pub allow: bool,
    /// Allowed role attributes. `SUPERUSER` is intentionally absent from the
    /// enum and therefore rejected by serde.
    #[serde(default)]
    pub attrs: Vec<RoleAttribute>,
}

/// Role attributes the future SB-2 guard may admit. `SUPERUSER` remains
/// unrepresentable here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoleAttribute {
    /// PostgreSQL `BYPASSRLS`.
    #[serde(rename = "BYPASSRLS")]
    BypassRls,
    /// PostgreSQL `CREATEROLE`.
    #[serde(rename = "CREATEROLE")]
    CreateRole,
}

/// Operational policy knobs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationalConfig {
    /// Index creation posture. Stage 1 records it; the non-transactional
    /// concurrent executor is a later task.
    #[serde(default)]
    pub index_creation: IndexCreation,
    /// Maximum PG lock-acquisition timeout in milliseconds.
    #[serde(default = "default_lock_timeout_ms")]
    pub lock_timeout_ms: u64,
    /// Maximum PG statement timeout in milliseconds.
    #[serde(default = "default_statement_timeout_ms")]
    pub statement_timeout_ms: u64,
    /// Table rewrite posture. Stage 1 records it; guard enforcement is later.
    #[serde(default)]
    pub table_rewrite: TableRewrite,
}

impl Default for OperationalConfig {
    fn default() -> Self {
        Self {
            index_creation: IndexCreation::AllowBlocking,
            lock_timeout_ms: default_lock_timeout_ms(),
            statement_timeout_ms: default_statement_timeout_ms(),
            table_rewrite: TableRewrite::Allow,
        }
    }
}

impl OperationalConfig {
    /// Apply the currently-backed operational knobs to an executor config clone.
    pub(crate) fn apply_to_executor_config(&self, exec_cfg: &mut ExecutorConfig) {
        exec_cfg.pg.lock_timeout = Duration::from_millis(self.lock_timeout_ms);
        exec_cfg.pg.statement_timeout = Duration::from_millis(self.statement_timeout_ms);
    }
}

const fn default_lock_timeout_ms() -> u64 {
    3_000
}

const fn default_statement_timeout_ms() -> u64 {
    60_000
}

/// Index creation policy. Ordered from more restrictive to less restrictive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexCreation {
    /// Require `CREATE INDEX CONCURRENTLY` once the non-transactional path lands.
    RequireConcurrent,
    /// Allow today's blocking transactional index creation.
    #[default]
    AllowBlocking,
}

/// Table rewrite policy. Ordered from more restrictive to less restrictive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TableRewrite {
    /// Forbid table rewrites.
    Forbid,
    /// Warn on table rewrites.
    Warn,
    /// Allow today's behavior.
    #[default]
    Allow,
}

/// Data-security obligations. Stage 1 records these; enforcement is later or
/// server-side where the engine cannot structurally prove the claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DataSecurityConfig {
    /// Require RLS. DDL-observable, engine-enforceable in a later guard pass.
    #[serde(default)]
    pub require_rls: bool,
    /// Forbid hard deletes. DDL-observable enough for migration-time policy.
    #[serde(default)]
    pub no_hard_delete: bool,
    /// Server-enforced sensitive-column set.
    #[serde(default)]
    pub sensitive_columns: Vec<String>,
    /// Server-composed destructive-op posture after approval projection.
    #[serde(default)]
    pub destructive_ops: DestructiveOps,
}

impl Default for DataSecurityConfig {
    fn default() -> Self {
        Self {
            require_rls: false,
            no_hard_delete: false,
            sensitive_columns: Vec::new(),
            destructive_ops: DestructiveOps::Allow,
        }
    }
}

/// Destructive operation posture. Ordered from more restrictive to less
/// restrictive. `RequireApproval` is recorded for server composition; the engine
/// receives only forbid/allow after the server projects it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DestructiveOps {
    /// Forbid destructive operations.
    Forbid,
    /// Require server approval before projecting to forbid/allow.
    RequireApproval,
    /// Allow today's approval-gated behavior.
    #[default]
    Allow,
}

/// Polarity category for profile knobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyPolarity {
    /// Permission knobs tighten by allowing less.
    Permission,
    /// Obligation knobs tighten by requiring more.
    Obligation,
}

/// Meet operation for a config knob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyMeet {
    /// Boolean AND.
    And,
    /// Set intersection.
    Intersection,
    /// Ordered or numeric minimum.
    Min,
    /// Boolean OR.
    Or,
    /// Set union.
    Union,
}

/// Static semantics for a profile knob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyKnobSemantics {
    /// Dot-path key.
    pub key: &'static str,
    /// Permission or obligation.
    pub polarity: PolicyPolarity,
    /// Composition meet operation.
    pub meet: PolicyMeet,
}

const POLICY_KNOB_SEMANTICS: &[PolicyKnobSemantics] = &[
    PolicyKnobSemantics { key: "capabilities.extension", polarity: PolicyPolarity::Permission, meet: PolicyMeet::And },
    PolicyKnobSemantics { key: "capabilities.schema", polarity: PolicyPolarity::Permission, meet: PolicyMeet::And },
    PolicyKnobSemantics { key: "capabilities.role.allow", polarity: PolicyPolarity::Permission, meet: PolicyMeet::And },
    PolicyKnobSemantics { key: "capabilities.role.attrs", polarity: PolicyPolarity::Permission, meet: PolicyMeet::Intersection },
    PolicyKnobSemantics { key: "capabilities.grant", polarity: PolicyPolarity::Permission, meet: PolicyMeet::And },
    PolicyKnobSemantics { key: "capabilities.rls", polarity: PolicyPolarity::Permission, meet: PolicyMeet::And },
    PolicyKnobSemantics { key: "capabilities.policy", polarity: PolicyPolarity::Permission, meet: PolicyMeet::And },
    PolicyKnobSemantics { key: "capabilities.function", polarity: PolicyPolarity::Permission, meet: PolicyMeet::And },
    PolicyKnobSemantics { key: "capabilities.raw_sql", polarity: PolicyPolarity::Permission, meet: PolicyMeet::And },
    PolicyKnobSemantics { key: "capabilities.raw_view_body", polarity: PolicyPolarity::Permission, meet: PolicyMeet::And },
    PolicyKnobSemantics { key: "capabilities.materialized_view", polarity: PolicyPolarity::Permission, meet: PolicyMeet::And },
    PolicyKnobSemantics { key: "capabilities.cross_schema", polarity: PolicyPolarity::Permission, meet: PolicyMeet::And },
    PolicyKnobSemantics { key: "capabilities.extensions", polarity: PolicyPolarity::Permission, meet: PolicyMeet::Intersection },
    PolicyKnobSemantics { key: "capabilities.schemas", polarity: PolicyPolarity::Permission, meet: PolicyMeet::Intersection },
    PolicyKnobSemantics { key: "operational.table_rewrite", polarity: PolicyPolarity::Permission, meet: PolicyMeet::Min },
    PolicyKnobSemantics { key: "operational.index_creation", polarity: PolicyPolarity::Permission, meet: PolicyMeet::Min },
    PolicyKnobSemantics { key: "operational.lock_timeout_ms", polarity: PolicyPolarity::Permission, meet: PolicyMeet::Min },
    PolicyKnobSemantics { key: "operational.statement_timeout_ms", polarity: PolicyPolarity::Permission, meet: PolicyMeet::Min },
    PolicyKnobSemantics { key: "data_security.require_rls", polarity: PolicyPolarity::Obligation, meet: PolicyMeet::Or },
    PolicyKnobSemantics { key: "data_security.no_hard_delete", polarity: PolicyPolarity::Obligation, meet: PolicyMeet::Or },
    PolicyKnobSemantics { key: "data_security.sensitive_columns", polarity: PolicyPolarity::Obligation, meet: PolicyMeet::Union },
    PolicyKnobSemantics { key: "data_security.destructive_ops", polarity: PolicyPolarity::Permission, meet: PolicyMeet::Min },
];

/// The only guard-compatible postures a sealed shared-infra profile can lower to.
///
/// There is deliberately no `Trusted` variant. The belt-skip posture is therefore
/// unrepresentable in a [`SealedProfile`], independent of runtime checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SealedPosture {
    /// Lower to today's confined guard.
    Confined,
    /// Lower to today's platform guard.
    Platform,
}

/// The resolved capability/operational state carried inside a sealed profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SealedEffectiveProfile {
    posture: SealedPosture,
    project_schema: String,
    capabilities: PolicyCapabilities,
    operational: OperationalConfig,
    data_security: DataSecurityConfig,
}

impl SealedEffectiveProfile {
    #[allow(dead_code)]
    fn from_profile(project_schema: String, profile: PolicyProfile) -> Self {
        let mut caps = profile.vendor_capabilities();
        caps.schemas.clear();
        let posture = if caps == VendorCapabilities::operator() {
            SealedPosture::Platform
        } else {
            SealedPosture::Confined
        };
        Self {
            posture,
            project_schema,
            capabilities: profile.capabilities,
            operational: profile.operational,
            data_security: profile.data_security,
        }
    }

    fn to_guard_config(&self) -> GuardConfig {
        match self.posture {
            SealedPosture::Confined => {
                GuardConfig::confined(self.project_schema.clone())
                    .with_extension_allowlist(self.capabilities.extensions.clone())
            }
            SealedPosture::Platform => {
                let cap = SealApplier::new();
                GuardConfig::platform(
                    &cap,
                    self.capabilities.schemas.clone(),
                    self.capabilities.extensions.clone(),
                )
            }
        }
    }
}

#[derive(Serialize)]
struct SealPayload<'a> {
    effective: &'a SealedEffectiveProfile,
    nonce: &'a [u8],
    issued_at: u64,
    ceiling_version: u64,
}

/// Authenticated, resolved profile for zeroship-operated infrastructure.
///
/// The constructor is crate-private and requires [`SealApplier`]. External
/// crates can hold and pass this opaque value, but cannot fabricate one. The
/// symmetric MAC is an in-process contract for stage 1; if the sealer and runner
/// split out-of-process, this should become an asymmetric signature with key
/// rotation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SealedProfile {
    effective: SealedEffectiveProfile,
    nonce: Vec<u8>,
    issued_at: u64,
    ceiling_version: u64,
    seal: Vec<u8>,
}

impl SealedProfile {
    /// Crate-private mint seam. The `seal` covers every other field via
    /// deterministic JSON serialization and is never optional.
    #[allow(dead_code)]
    pub(crate) fn mint(
        _cap: &SealApplier,
        profile: PolicyProfile,
        project_schema: impl Into<String>,
        key: &[u8],
        nonce: Vec<u8>,
        issued_at: u64,
        ceiling_version: u64,
    ) -> Result<Self, SealError> {
        let mut sealed = Self {
            effective: SealedEffectiveProfile::from_profile(project_schema.into(), profile),
            nonce,
            issued_at,
            ceiling_version,
            seal: Vec::new(),
        };
        sealed.seal = sealed.compute_mac(key)?;
        Ok(sealed)
    }

    /// Verify the MAC and ceiling freshness.
    ///
    /// # Errors
    /// Returns [`SealError::SupersededCeiling`] for stale ceiling versions and
    /// [`SealError::InvalidSeal`] for tampering or the wrong key.
    pub fn verify(&self, verifier: &SealVerifier) -> Result<(), SealError> {
        if self.ceiling_version != verifier.current_ceiling_version {
            return Err(SealError::SupersededCeiling {
                sealed: self.ceiling_version,
                current: verifier.current_ceiling_version,
            });
        }
        let mac = self.build_mac(&verifier.key)?;
        mac.verify_slice(&self.seal)
            .map_err(|_| SealError::InvalidSeal)?;
        Ok(())
    }

    /// Ceiling version embedded in the sealed profile.
    #[must_use]
    pub const fn ceiling_version(&self) -> u64 {
        self.ceiling_version
    }

    /// Issued-at timestamp supplied by the sealer.
    #[must_use]
    pub const fn issued_at(&self) -> u64 {
        self.issued_at
    }

    /// Nonce supplied by the sealer.
    #[must_use]
    pub fn nonce(&self) -> &[u8] {
        &self.nonce
    }

    /// The guard-compatible posture. This cannot be `Trusted`.
    #[must_use]
    pub const fn posture(&self) -> SealedPosture {
        self.effective.posture
    }

    pub(crate) fn project_schema(&self) -> &str {
        &self.effective.project_schema
    }

    pub(crate) fn to_guard_config(&self) -> GuardConfig {
        self.effective.to_guard_config()
    }

    pub(crate) fn apply_operational_to_executor_config(&self, exec_cfg: &mut ExecutorConfig) {
        self.effective.operational.apply_to_executor_config(exec_cfg);
    }

    #[cfg(test)]
    fn tamper_first_nonce_byte(&mut self) {
        if let Some(first) = self.nonce.first_mut() {
            *first ^= 0xff;
        }
    }

    #[allow(dead_code)]
    fn compute_mac(&self, key: &[u8]) -> Result<Vec<u8>, SealError> {
        let mac = self.build_mac(key)?;
        Ok(mac.finalize().into_bytes().to_vec())
    }

    fn build_mac(&self, key: &[u8]) -> Result<HmacSha256, SealError> {
        let mut mac = HmacSha256::new_from_slice(key).map_err(|_| SealError::InvalidKey)?;
        let payload = SealPayload {
            effective: &self.effective,
            nonce: &self.nonce,
            issued_at: self.issued_at,
            ceiling_version: self.ceiling_version,
        };
        let bytes = serde_json::to_vec(&payload).map_err(SealError::Serialize)?;
        mac.update(&bytes);
        Ok(mac)
    }
}

/// Verification context for a sealed profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealVerifier {
    key: Vec<u8>,
    current_ceiling_version: u64,
}

impl SealVerifier {
    /// Build a verifier from the active in-process MAC key and ceiling version.
    #[must_use]
    pub fn new(key: impl AsRef<[u8]>, current_ceiling_version: u64) -> Self {
        Self {
            key: key.as_ref().to_vec(),
            current_ceiling_version,
        }
    }
}

/// Sealed-profile verification failure.
#[derive(Debug, thiserror::Error)]
pub enum SealError {
    /// The configured MAC key was invalid.
    #[error("invalid sealed-profile MAC key")]
    InvalidKey,
    /// The profile's MAC did not verify.
    #[error("sealed migration policy profile did not verify")]
    InvalidSeal,
    /// The profile was minted under an older ceiling generation.
    #[error("sealed profile references superseded ceiling v{sealed} (current v{current}): re-submit required")]
    SupersededCeiling {
        /// Ceiling version embedded in the seal.
        sealed: u64,
        /// Current active ceiling version.
        current: u64,
    },
    /// Canonical seal payload serialization failed.
    #[error("serialize sealed migration policy payload: {0}")]
    Serialize(#[source] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::capability::VendorCapabilities;

    const KEY: &[u8] = b"stage-1 test key for sealed migrate policy";

    #[test]
    fn policy_profile_toml_round_trips_and_matches_platform_caps() {
        let parsed = PolicyProfile::from_toml(PLATFORM_PROFILE_TOML).unwrap();
        assert_eq!(parsed.vendor_capabilities(), VendorCapabilities::operator());
        let encoded = toml::to_string(&parsed).unwrap();
        let reparsed = PolicyProfile::from_toml(&encoded).unwrap();
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn policy_profile_denies_unknown_fields() {
        let err = PolicyProfile::from_toml(
            r#"
            [capabilities]
            raw_sq = true
            "#,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("unknown field"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn policy_profile_fail_closed_loader_uses_confined_for_empty_or_typo() {
        assert_eq!(
            PolicyProfile::from_toml_or_confined(None).vendor_capabilities(),
            VendorCapabilities::confined()
        );
        assert_eq!(
            PolicyProfile::from_toml_or_confined(Some("")).vendor_capabilities(),
            VendorCapabilities::confined()
        );
        assert_eq!(
            PolicyProfile::from_toml_or_confined(Some("[capabilities]\nraw_sq = true"))
                .vendor_capabilities(),
            VendorCapabilities::confined()
        );
    }

    #[test]
    fn confined_and_platform_presets_match_existing_vendor_capability_presets() {
        assert_eq!(
            PolicyProfile::confined().vendor_capabilities(),
            VendorCapabilities::confined()
        );
        assert_eq!(
            PolicyProfile::platform().vendor_capabilities(),
            VendorCapabilities::operator()
        );
        assert!(PolicyProfile::preset("permissive").is_none());
    }

    #[test]
    fn role_attrs_reject_superuser_by_type() {
        let err = PolicyProfile::from_toml(
            r#"
            [capabilities]
            role = { allow = true, attrs = ["SUPERUSER"] }
            "#,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("unknown variant"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn polarity_table_tags_permissions_and_obligations() {
        assert!(PolicyProfile::polarity_table().iter().any(|k| {
            k.key == "capabilities.role.attrs"
                && k.polarity == PolicyPolarity::Permission
                && k.meet == PolicyMeet::Intersection
        }));
        assert!(PolicyProfile::polarity_table().iter().any(|k| {
            k.key == "data_security.require_rls"
                && k.polarity == PolicyPolarity::Obligation
                && k.meet == PolicyMeet::Or
        }));
    }

    #[test]
    fn sealed_profile_mint_verify_round_trip() {
        let cap = SealApplier::new();
        let sealed = SealedProfile::mint(
            &cap,
            PolicyProfile::platform(),
            "app",
            KEY,
            b"nonce-1".to_vec(),
            1_719_792_000,
            7,
        )
        .unwrap();
        sealed.verify(&SealVerifier::new(KEY, 7)).unwrap();
        assert_eq!(sealed.posture(), SealedPosture::Platform);
    }

    #[test]
    fn sealed_profile_rejects_tampering() {
        let cap = SealApplier::new();
        let mut sealed = SealedProfile::mint(
            &cap,
            PolicyProfile::platform(),
            "app",
            KEY,
            b"nonce-1".to_vec(),
            1_719_792_000,
            7,
        )
        .unwrap();
        sealed.tamper_first_nonce_byte();
        assert!(matches!(
            sealed.verify(&SealVerifier::new(KEY, 7)),
            Err(SealError::InvalidSeal)
        ));
    }

    #[test]
    fn sealed_profile_rejects_superseded_ceiling() {
        let cap = SealApplier::new();
        let sealed = SealedProfile::mint(
            &cap,
            PolicyProfile::platform(),
            "app",
            KEY,
            b"nonce-1".to_vec(),
            1_719_792_000,
            7,
        )
        .unwrap();
        assert!(matches!(
            sealed.verify(&SealVerifier::new(KEY, 8)),
            Err(SealError::SupersededCeiling {
                sealed: 7,
                current: 8
            })
        ));
    }

    #[test]
    fn sealed_profile_type_has_no_trusted_posture() {
        let cap = SealApplier::new();
        let sealed = SealedProfile::mint(
            &cap,
            PolicyProfile::platform(),
            "app",
            KEY,
            b"nonce-1".to_vec(),
            1_719_792_000,
            7,
        )
        .unwrap();
        match sealed.posture() {
            SealedPosture::Confined | SealedPosture::Platform => {}
        }
    }
}
