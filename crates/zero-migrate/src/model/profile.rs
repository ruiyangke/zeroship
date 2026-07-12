//! Declarative migration policy profiles and sealed shared-infra apply profiles.
//!
//! Stage M2 keeps today's guard implementation intact. The profile config is
//! loaded strictly and resolved fail-closed, while [`SealedProfile`] is the new
//! structural carrier the future control-plane path will pass to the engine.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::model::capability::{SealApplier, VendorCapabilities};
// The sealed-profile → executor/guard plumbing is currently exercised only by the
// in-crate test suite (its production caller was the removed native-pg apply
// path), so these imports ride behind `cfg(test)`.
#[cfg(test)]
use std::time::Duration;
#[cfg(test)]
use crate::conn::ExecutorConfig;
#[cfg(test)]
use crate::guard::GuardConfig;

type HmacSha256 = Hmac<Sha256>;

const MIN_SEAL_MAC_KEY_BYTES: usize = 32;
const SEAL_MAX_AGE_SECS: u64 = 15 * 60;
const PLATFORM_LOCK_TIMEOUT_CEILING_MS: u64 = 3_000;
const PLATFORM_STATEMENT_TIMEOUT_CEILING_MS: u64 = 60_000;

/// Embedded least-privilege profile. This is also the fail-closed fallback.
pub const CONFINED_PROFILE_TOML: &str = include_str!("../../policy-profiles/confined.toml");

/// Embedded platform profile. Its capability booleans match
/// [`VendorCapabilities::operator`]; schema/extension allowlists remain explicit
/// inputs and therefore default empty here.
pub const PLATFORM_PROFILE_TOML: &str = include_str!("../../policy-profiles/platform.toml");

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
    /// Platform-managed table shape injected around author-declared columns.
    #[serde(default)]
    pub system_shape: TableSystemShapePolicy,
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

    /// Compose the operator ceiling with the submitted draft.
    ///
    /// Permission knobs meet by tightening toward the smaller surface
    /// (boolean-AND, set intersection, ordered/numeric minimum) and reject a
    /// draft that tries to exceed the ceiling. Obligation knobs meet by unioning
    /// upward so ceiling requirements cannot be removed by the draft.
    pub fn meet_ceiling_draft(ceiling: &Self, draft: &Self) -> Result<Self, SealError> {
        Ok(Self {
            extends: None,
            capabilities: PolicyCapabilities::meet_ceiling_draft(
                &ceiling.capabilities,
                &draft.capabilities,
            )?,
            operational: OperationalConfig::meet_ceiling_draft(
                &ceiling.operational,
                &draft.operational,
            )?,
            data_security: DataSecurityConfig::meet_ceiling_draft(
                &ceiling.data_security,
                &draft.data_security,
            )?,
            system_shape: TableSystemShapePolicy::meet_ceiling_draft(
                &ceiling.system_shape,
                &draft.system_shape,
            )?,
        })
    }
}

/// Policy for platform-managed table shape injected around author-declared columns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableSystemShapePolicy {
    /// System columns prepended to every table.
    #[serde(default)]
    pub columns: Vec<InjectedSystemColumnPolicy>,
    /// System indexes created for every table, excluding the implicit primary-key
    /// index represented by [`primary_key`](Self::primary_key).
    #[serde(default)]
    pub indexes: Vec<InjectedSystemIndexPolicy>,
    /// Primary-key policy for the table.
    #[serde(default)]
    pub primary_key: TablePrimaryKeyPolicy,
    /// Whether author-supplied primary keys are accepted.
    #[serde(default)]
    pub author_primary_key: AuthorPrimaryKeyPolicy,
}

impl Default for TableSystemShapePolicy {
    fn default() -> Self {
        Self::confined()
    }
}

impl TableSystemShapePolicy {
    /// The current confined table shape injected by `build_table_snapshot`.
    #[must_use]
    pub fn confined() -> Self {
        Self {
            columns: vec![
                InjectedSystemColumnPolicy {
                    name: "id".into(),
                    data_type: "text".into(),
                    nullable: false,
                },
                InjectedSystemColumnPolicy {
                    name: "created_at".into(),
                    data_type: "timestamp with time zone".into(),
                    nullable: false,
                },
                InjectedSystemColumnPolicy {
                    name: "updated_at".into(),
                    data_type: "timestamp with time zone".into(),
                    nullable: false,
                },
                InjectedSystemColumnPolicy {
                    name: "created_by".into(),
                    data_type: "text".into(),
                    nullable: true,
                },
                InjectedSystemColumnPolicy {
                    name: "updated_by".into(),
                    data_type: "text".into(),
                    nullable: true,
                },
                InjectedSystemColumnPolicy {
                    name: "version".into(),
                    data_type: "integer".into(),
                    nullable: false,
                },
                InjectedSystemColumnPolicy {
                    name: "deleted_at".into(),
                    data_type: "timestamp with time zone".into(),
                    nullable: true,
                },
            ],
            indexes: vec![
                InjectedSystemIndexPolicy {
                    columns: vec!["deleted_at".into()],
                },
                InjectedSystemIndexPolicy {
                    columns: vec!["updated_at".into()],
                },
                InjectedSystemIndexPolicy {
                    columns: vec!["created_by".into()],
                },
            ],
            primary_key: TablePrimaryKeyPolicy::ExplicitColumns(vec!["id".into()]),
            author_primary_key: AuthorPrimaryKeyPolicy::Forbid,
        }
    }

    /// The platform/operator table-shape policy: no automatic injection.
    #[must_use]
    pub fn platform() -> Self {
        Self {
            columns: Vec::new(),
            indexes: Vec::new(),
            primary_key: TablePrimaryKeyPolicy::Author(PrimaryKeyAuthorPolicy::Author),
            author_primary_key: AuthorPrimaryKeyPolicy::Allow,
        }
    }

    fn meet_ceiling_draft(ceiling: &Self, draft: &Self) -> Result<Self, SealError> {
        if ceiling == &Self::platform() {
            return Ok(draft.clone());
        }
        if draft == ceiling {
            return Ok(draft.clone());
        }
        Err(SealError::PolicyExceedsCeiling {
            knob: "system_shape",
        })
    }

    fn validate_for_seal(&self) -> Result<(), SealError> {
        if self == &Self::confined() || self == &Self::platform() {
            return Ok(());
        }
        Err(SealError::UnsupportedProfileKnob {
            knob: "system_shape",
        })
    }
}

/// One injected system column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InjectedSystemColumnPolicy {
    /// Column name.
    pub name: String,
    /// `information_schema` data-type spelling.
    #[serde(rename = "type")]
    pub data_type: String,
    /// Whether the injected column is nullable.
    #[serde(default)]
    pub nullable: bool,
}

/// One injected system index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InjectedSystemIndexPolicy {
    /// Indexed columns, in order.
    pub columns: Vec<String>,
}

/// Primary-key source for a table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TablePrimaryKeyPolicy {
    /// The platform injects an explicit primary key over these columns.
    ExplicitColumns(Vec<String>),
    /// The author owns primary-key declaration.
    Author(PrimaryKeyAuthorPolicy),
}

impl Default for TablePrimaryKeyPolicy {
    fn default() -> Self {
        Self::ExplicitColumns(vec!["id".into()])
    }
}

/// TOML/JSON sentinel used by [`TablePrimaryKeyPolicy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrimaryKeyAuthorPolicy {
    /// Author-owned primary-key policy.
    Author,
}

/// Whether author-supplied primary keys are accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorPrimaryKeyPolicy {
    /// Reject author-supplied primary keys.
    #[default]
    Forbid,
    /// Accept author-supplied primary keys.
    Allow,
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
    /// PostgreSQL partition attach.
    #[serde(default)]
    pub partition: bool,
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
            partition: caps.allow_partition,
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
            allow_partition: self.partition,
            allow_policy: self.policy,
            allow_function: self.function,
            allow_raw_sql: self.raw_sql,
            allow_raw_view_body: self.raw_view_body,
            allow_materialized_view: self.materialized_view,
            allow_cross_schema: self.cross_schema,
            schemas: self.schemas.clone(),
        }
    }

    fn meet_ceiling_draft(ceiling: &Self, draft: &Self) -> Result<Self, SealError> {
        Ok(Self {
            extension: meet_bool_permission(
                "capabilities.extension",
                ceiling.extension,
                draft.extension,
            )?,
            schema: meet_bool_permission("capabilities.schema", ceiling.schema, draft.schema)?,
            role: RoleCapabilityConfig::meet_ceiling_draft(&ceiling.role, &draft.role)?,
            grant: meet_bool_permission("capabilities.grant", ceiling.grant, draft.grant)?,
            rls: meet_bool_permission("capabilities.rls", ceiling.rls, draft.rls)?,
            partition: meet_bool_permission(
                "capabilities.partition",
                ceiling.partition,
                draft.partition,
            )?,
            policy: meet_bool_permission("capabilities.policy", ceiling.policy, draft.policy)?,
            function: meet_bool_permission(
                "capabilities.function",
                ceiling.function,
                draft.function,
            )?,
            raw_sql: meet_bool_permission(
                "capabilities.raw_sql",
                ceiling.raw_sql,
                draft.raw_sql,
            )?,
            raw_view_body: meet_bool_permission(
                "capabilities.raw_view_body",
                ceiling.raw_view_body,
                draft.raw_view_body,
            )?,
            materialized_view: meet_bool_permission(
                "capabilities.materialized_view",
                ceiling.materialized_view,
                draft.materialized_view,
            )?,
            cross_schema: meet_bool_permission(
                "capabilities.cross_schema",
                ceiling.cross_schema,
                draft.cross_schema,
            )?,
            extensions: meet_string_permission_set(
                "capabilities.extensions",
                &ceiling.extensions,
                &draft.extensions,
            )?,
            schemas: meet_string_permission_set(
                "capabilities.schemas",
                &ceiling.schemas,
                &draft.schemas,
            )?,
        })
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

impl RoleCapabilityConfig {
    fn meet_ceiling_draft(ceiling: &Self, draft: &Self) -> Result<Self, SealError> {
        Ok(Self {
            allow: meet_bool_permission(
                "capabilities.role.allow",
                ceiling.allow,
                draft.allow,
            )?,
            attrs: meet_role_attribute_permission_set(
                "capabilities.role.attrs",
                &ceiling.attrs,
                &draft.attrs,
            )?,
        })
    }
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
    #[serde(
        default = "default_lock_timeout_ms",
        deserialize_with = "deserialize_lock_timeout_ms"
    )]
    pub lock_timeout_ms: u64,
    /// Maximum PG statement timeout in milliseconds.
    #[serde(
        default = "default_statement_timeout_ms",
        deserialize_with = "deserialize_statement_timeout_ms"
    )]
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
    fn meet_ceiling_draft(ceiling: &Self, draft: &Self) -> Result<Self, SealError> {
        if draft.index_creation.is_looser_than(ceiling.index_creation) {
            return Err(SealError::PolicyExceedsCeiling {
                knob: "operational.index_creation",
            });
        }
        if draft.table_rewrite.is_looser_than(ceiling.table_rewrite) {
            return Err(SealError::PolicyExceedsCeiling {
                knob: "operational.table_rewrite",
            });
        }

        Ok(Self {
            index_creation: ceiling.index_creation.tightest(draft.index_creation),
            lock_timeout_ms: meet_timeout_ms(
                ceiling.lock_timeout_ms,
                draft.lock_timeout_ms,
                "operational.lock_timeout_ms",
                PLATFORM_LOCK_TIMEOUT_CEILING_MS,
            )?,
            statement_timeout_ms: meet_timeout_ms(
                ceiling.statement_timeout_ms,
                draft.statement_timeout_ms,
                "operational.statement_timeout_ms",
                PLATFORM_STATEMENT_TIMEOUT_CEILING_MS,
            )?,
            table_rewrite: ceiling.table_rewrite.tightest(draft.table_rewrite),
        })
    }

    /// Apply the currently-backed operational knobs to an executor config clone.
    #[cfg(test)]
    pub(crate) fn apply_to_executor_config(&self, exec_cfg: &mut ExecutorConfig) {
        let lock_ceiling = duration_millis_u64(exec_cfg.pg.lock_timeout);
        let statement_ceiling = duration_millis_u64(exec_cfg.pg.statement_timeout);
        let lock_timeout_ms = min_non_zero_timeout_ms(self.lock_timeout_ms, lock_ceiling)
            .unwrap_or(PLATFORM_LOCK_TIMEOUT_CEILING_MS);
        let statement_timeout_ms =
            min_non_zero_timeout_ms(self.statement_timeout_ms, statement_ceiling)
                .unwrap_or(PLATFORM_STATEMENT_TIMEOUT_CEILING_MS);
        exec_cfg.pg.lock_timeout = Duration::from_millis(lock_timeout_ms);
        exec_cfg.pg.statement_timeout = Duration::from_millis(statement_timeout_ms);
    }

    fn validate_for_seal(&self) -> Result<(), SealError> {
        validate_timeout_ms(
            self.lock_timeout_ms,
            "operational.lock_timeout_ms",
            PLATFORM_LOCK_TIMEOUT_CEILING_MS,
        )?;
        validate_timeout_ms(
            self.statement_timeout_ms,
            "operational.statement_timeout_ms",
            PLATFORM_STATEMENT_TIMEOUT_CEILING_MS,
        )?;
        if self.index_creation != IndexCreation::AllowBlocking {
            return Err(SealError::UnsupportedProfileKnob {
                knob: "operational.index_creation",
            });
        }
        if self.table_rewrite != TableRewrite::Allow {
            return Err(SealError::UnsupportedProfileKnob {
                knob: "operational.table_rewrite",
            });
        }
        Ok(())
    }
}

const fn default_lock_timeout_ms() -> u64 {
    PLATFORM_LOCK_TIMEOUT_CEILING_MS
}

const fn default_statement_timeout_ms() -> u64 {
    PLATFORM_STATEMENT_TIMEOUT_CEILING_MS
}

fn deserialize_lock_timeout_ms<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = u64::deserialize(deserializer)?;
    validate_timeout_ms(
        value,
        "operational.lock_timeout_ms",
        PLATFORM_LOCK_TIMEOUT_CEILING_MS,
    )
    .map_err(<D::Error as serde::de::Error>::custom)
}

fn deserialize_statement_timeout_ms<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = u64::deserialize(deserializer)?;
    validate_timeout_ms(
        value,
        "operational.statement_timeout_ms",
        PLATFORM_STATEMENT_TIMEOUT_CEILING_MS,
    )
    .map_err(<D::Error as serde::de::Error>::custom)
}

fn validate_timeout_ms(
    value: u64,
    knob: &'static str,
    ceiling_ms: u64,
) -> Result<u64, SealError> {
    if value == 0 {
        return Err(SealError::InvalidTimeout {
            knob,
            value,
            ceiling_ms,
        });
    }
    if value > ceiling_ms {
        return Err(SealError::InvalidTimeout {
            knob,
            value,
            ceiling_ms,
        });
    }
    Ok(value)
}

#[cfg(test)]
const fn min_non_zero_timeout_ms(left: u64, right: u64) -> Option<u64> {
    match (left, right) {
        (0, 0) => None,
        (0, value) | (value, 0) => Some(value),
        (left, right) if left < right => Some(left),
        (_, right) => Some(right),
    }
}

#[cfg(test)]
fn duration_millis_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn meet_bool_permission(
    knob: &'static str,
    ceiling: bool,
    draft: bool,
) -> Result<bool, SealError> {
    if draft && !ceiling {
        return Err(SealError::PolicyExceedsCeiling { knob });
    }
    Ok(ceiling && draft)
}

fn meet_string_permission_set(
    knob: &'static str,
    ceiling: &[String],
    draft: &[String],
) -> Result<Vec<String>, SealError> {
    let mut effective = Vec::new();
    for item in draft {
        if !ceiling.contains(item) {
            return Err(SealError::PolicyExceedsCeiling { knob });
        }
        if !effective.contains(item) {
            effective.push(item.clone());
        }
    }
    Ok(effective)
}

fn meet_role_attribute_permission_set(
    knob: &'static str,
    ceiling: &[RoleAttribute],
    draft: &[RoleAttribute],
) -> Result<Vec<RoleAttribute>, SealError> {
    let mut effective = Vec::new();
    for attr in draft {
        if !ceiling.contains(attr) {
            return Err(SealError::PolicyExceedsCeiling { knob });
        }
        if !effective.contains(attr) {
            effective.push(*attr);
        }
    }
    Ok(effective)
}

fn meet_timeout_ms(
    ceiling: u64,
    draft: u64,
    knob: &'static str,
    platform_ceiling_ms: u64,
) -> Result<u64, SealError> {
    validate_timeout_ms(ceiling, knob, platform_ceiling_ms)?;
    validate_timeout_ms(draft, knob, platform_ceiling_ms)?;
    if draft > ceiling {
        return Err(SealError::PolicyExceedsCeiling { knob });
    }
    Ok(draft)
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

impl IndexCreation {
    const fn rank(self) -> u8 {
        match self {
            Self::RequireConcurrent => 0,
            Self::AllowBlocking => 1,
        }
    }

    const fn tightest(self, other: Self) -> Self {
        if self.rank() <= other.rank() { self } else { other }
    }

    const fn is_looser_than(self, ceiling: Self) -> bool {
        self.rank() > ceiling.rank()
    }
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

impl TableRewrite {
    const fn rank(self) -> u8 {
        match self {
            Self::Forbid => 0,
            Self::Warn => 1,
            Self::Allow => 2,
        }
    }

    const fn tightest(self, other: Self) -> Self {
        if self.rank() <= other.rank() { self } else { other }
    }

    const fn is_looser_than(self, ceiling: Self) -> bool {
        self.rank() > ceiling.rank()
    }
}

/// Data-security obligations. `require_rls` and `destructive_ops` are enforced
/// by the guard; the remaining knobs stay server-side until the engine has a
/// structural enforcement point for them.
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

impl DataSecurityConfig {
    /// Polarity-correct ceiling⊓draft meet for data-security knobs.
    ///
    /// Obligation knobs OR/union upward. Permission knobs tighten toward the
    /// stricter enum value; a draft that tries to loosen the ceiling is rejected
    /// instead of silently clamped.
    pub fn meet_ceiling_draft(ceiling: &Self, draft: &Self) -> Result<Self, SealError> {
        if ceiling.destructive_ops != DestructiveOps::RequireApproval
            && draft.destructive_ops.is_looser_than(ceiling.destructive_ops)
        {
            return Err(SealError::PolicyExceedsCeiling {
                knob: "data_security.destructive_ops",
            });
        }

        let mut sensitive_columns = ceiling.sensitive_columns.clone();
        for column in &draft.sensitive_columns {
            if !sensitive_columns.contains(column) {
                sensitive_columns.push(column.clone());
            }
        }

        Ok(Self {
            require_rls: ceiling.require_rls || draft.require_rls,
            no_hard_delete: ceiling.no_hard_delete || draft.no_hard_delete,
            sensitive_columns,
            destructive_ops: ceiling.destructive_ops.tightest(draft.destructive_ops),
        })
    }
}

/// Destructive operation posture. Ordered from more restrictive to less
/// restrictive. `RequireApproval` is retained as a server composition value, but
/// sealed engine configs accept only the enforceable `forbid`/`warn`/`allow`
/// states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DestructiveOps {
    /// Forbid destructive operations.
    Forbid,
    /// Allow destructive operations and surface a structured warning.
    Warn,
    /// Require server approval before projecting to forbid/allow.
    RequireApproval,
    /// Allow today's approval-gated behavior.
    #[default]
    Allow,
}

impl DestructiveOps {
    const fn rank(self) -> u8 {
        match self {
            Self::RequireApproval => 0,
            Self::Forbid => 1,
            Self::Warn => 2,
            Self::Allow => 3,
        }
    }

    const fn tightest(self, other: Self) -> Self {
        if self.rank() <= other.rank() { self } else { other }
    }

    const fn is_looser_than(self, ceiling: Self) -> bool {
        self.rank() > ceiling.rank()
    }
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
    /// Numeric minimum where zero is treated as absent, not as the tightest value.
    MinNonZero,
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
    PolicyKnobSemantics { key: "capabilities.partition", polarity: PolicyPolarity::Permission, meet: PolicyMeet::And },
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
    PolicyKnobSemantics { key: "operational.lock_timeout_ms", polarity: PolicyPolarity::Permission, meet: PolicyMeet::MinNonZero },
    PolicyKnobSemantics { key: "operational.statement_timeout_ms", polarity: PolicyPolarity::Permission, meet: PolicyMeet::MinNonZero },
    PolicyKnobSemantics { key: "data_security.require_rls", polarity: PolicyPolarity::Obligation, meet: PolicyMeet::Or },
    PolicyKnobSemantics { key: "data_security.no_hard_delete", polarity: PolicyPolarity::Obligation, meet: PolicyMeet::Or },
    PolicyKnobSemantics { key: "data_security.sensitive_columns", polarity: PolicyPolarity::Obligation, meet: PolicyMeet::Union },
    PolicyKnobSemantics { key: "data_security.destructive_ops", polarity: PolicyPolarity::Permission, meet: PolicyMeet::Min },
];

/// The only guard-compatible postures a sealed shared-infra profile can lower to.
///
/// There is deliberately no `Trusted` variant. The belt-skip posture is therefore
/// unrepresentable in a [`SealedProfile`], independent of runtime checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealedPosture {
    /// Lower to today's confined guard.
    Confined,
    /// Lower to today's platform guard.
    Platform,
}

/// The resolved capability/operational state carried inside a sealed profile.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SealedEffectiveProfile {
    posture: SealedPosture,
    project_schema: String,
    capabilities: PolicyCapabilities,
    operational: OperationalConfig,
    data_security: DataSecurityConfig,
    system_shape: TableSystemShapePolicy,
}

impl SealedEffectiveProfile {
    #[allow(dead_code)]
    fn from_profile(project_schema: String, profile: PolicyProfile) -> Result<Self, SealError> {
        profile.validate_for_seal()?;
        let mut caps = profile.vendor_capabilities();
        caps.schemas.clear();
        let posture = if caps == VendorCapabilities::operator() {
            SealedPosture::Platform
        } else {
            SealedPosture::Confined
        };
        Ok(Self {
            posture,
            project_schema,
            capabilities: profile.capabilities,
            operational: profile.operational,
            data_security: profile.data_security,
            system_shape: profile.system_shape,
        })
    }

    #[cfg(test)]
    fn to_guard_config(&self) -> GuardConfig {
        match self.posture {
            SealedPosture::Confined => {
                GuardConfig::confined(self.project_schema.clone())
                    .with_extension_allowlist(self.capabilities.extensions.clone())
                    .with_data_security(
                        self.data_security.require_rls,
                        self.data_security.destructive_ops,
                    )
            }
            SealedPosture::Platform => {
                let cap = SealApplier::new();
                GuardConfig::platform(
                    &cap,
                    self.capabilities.schemas.clone(),
                    self.capabilities.extensions.clone(),
                )
                .with_data_security(
                    self.data_security.require_rls,
                    self.data_security.destructive_ops,
                )
            }
        }
    }
}

impl PolicyProfile {
    fn validate_for_seal(&self) -> Result<(), SealError> {
        self.capabilities.validate_for_seal()?;
        self.operational.validate_for_seal()?;
        self.data_security.validate_for_seal()?;
        self.system_shape.validate_for_seal()?;
        Ok(())
    }
}

impl PolicyCapabilities {
    fn validate_for_seal(&self) -> Result<(), SealError> {
        if !self.role.attrs.is_empty() {
            return Err(SealError::UnsupportedProfileKnob {
                knob: "capabilities.role.attrs",
            });
        }

        let mut caps = self.to_vendor_capabilities();
        caps.schemas.clear();
        if caps == VendorCapabilities::operator() {
            return Ok(());
        }
        if caps == VendorCapabilities::confined() && self.schemas.is_empty() {
            return Ok(());
        }

        Err(SealError::UnsupportedProfileKnob {
            knob: "capabilities.granular",
        })
    }
}

impl DataSecurityConfig {
    fn validate_for_seal(&self) -> Result<(), SealError> {
        if self.no_hard_delete {
            return Err(SealError::UnsupportedProfileKnob {
                knob: "data_security.no_hard_delete",
            });
        }
        if !self.sensitive_columns.is_empty() {
            return Err(SealError::UnsupportedProfileKnob {
                knob: "data_security.sensitive_columns",
            });
        }
        if self.destructive_ops == DestructiveOps::RequireApproval {
            return Err(SealError::UnsupportedProfileKnob {
                knob: "data_security.destructive_ops",
            });
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct SealPayload<'a> {
    effective: SealPayloadEffective<'a>,
    nonce: &'a [u8],
    issued_at: u64,
    ceiling_version: u64,
}

#[derive(Serialize)]
struct SealPayloadEffective<'a> {
    posture: SealPayloadPosture,
    project_schema: &'a str,
    capabilities: &'a PolicyCapabilities,
    operational: &'a OperationalConfig,
    data_security: &'a DataSecurityConfig,
    system_shape: &'a TableSystemShapePolicy,
}

#[derive(Clone, Copy, Serialize)]
enum SealPayloadPosture {
    Confined,
    Platform,
}

impl From<SealedPosture> for SealPayloadPosture {
    fn from(posture: SealedPosture) -> Self {
        match posture {
            SealedPosture::Confined => Self::Confined,
            SealedPosture::Platform => Self::Platform,
        }
    }
}

/// Authenticated, resolved profile for platform-operated infrastructure.
///
/// The constructor is crate-private and requires [`SealApplier`]. External
/// crates can hold and pass this opaque value, but cannot fabricate one,
/// including through serde deserialization.
///
/// ```compile_fail
/// let _: zero_migrate::SealedProfile = serde_json::from_str("{}").unwrap();
/// ```
///
/// The symmetric MAC is an in-process contract for stage 1; if the sealer and
/// runner split out-of-process, this should become an asymmetric signature with
/// key rotation.
#[derive(Debug, Clone, PartialEq, Eq)]
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
        Self::mint_composed(
            _cap,
            profile.clone(),
            profile,
            project_schema,
            key,
            nonce,
            issued_at,
            ceiling_version,
        )
    }

    /// Crate-private mint seam for the server path that has a distinct operator
    /// ceiling and submitted draft. The composed effective profile is sealed and
    /// later lowered to the guard used by [`crate::command::ir_apply::apply_sealed`].
    #[allow(dead_code)]
    pub(crate) fn mint_composed(
        _cap: &SealApplier,
        ceiling: PolicyProfile,
        draft: PolicyProfile,
        project_schema: impl Into<String>,
        key: &[u8],
        nonce: Vec<u8>,
        issued_at: u64,
        ceiling_version: u64,
    ) -> Result<Self, SealError> {
        validate_mac_key(key)?;
        let effective_profile = PolicyProfile::meet_ceiling_draft(&ceiling, &draft)?;
        let mut sealed = Self {
            effective: SealedEffectiveProfile::from_profile(project_schema.into(), effective_profile)?,
            nonce,
            issued_at,
            ceiling_version,
            seal: Vec::new(),
        };
        sealed.seal = sealed.compute_mac(key)?;
        Ok(sealed)
    }

    /// Verify the MAC, ceiling freshness, and bounded issued-at age.
    ///
    /// # Errors
    /// Returns [`SealError::SupersededCeiling`] for stale ceiling versions and
    /// [`SealError::InvalidSeal`] for tampering or the wrong key.
    pub fn verify(&self, verifier: &SealVerifier) -> Result<(), SealError> {
        let now_unix_secs = verifier.now_unix_secs();
        if self.ceiling_version != verifier.current_ceiling_version {
            return Err(SealError::SupersededCeiling {
                sealed: self.ceiling_version,
                current: verifier.current_ceiling_version,
            });
        }
        if self.issued_at > now_unix_secs {
            return Err(SealError::IssuedInFuture {
                issued_at: self.issued_at,
                now: now_unix_secs,
            });
        }
        let age_secs = now_unix_secs - self.issued_at;
        if age_secs > verifier.max_age_secs {
            return Err(SealError::StaleSeal {
                issued_at: self.issued_at,
                now: now_unix_secs,
                max_age_secs: verifier.max_age_secs,
            });
        }
        let mac = self.build_mac(&verifier.key)?;
        mac.verify_slice(&self.seal)
            .map_err(|_| SealError::InvalidSeal)?;
        verifier.consume_nonce(
            &self.nonce,
            self.issued_at.saturating_add(verifier.max_age_secs),
        )?;
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

    #[cfg(test)]
    pub(crate) fn to_guard_config(&self) -> GuardConfig {
        self.effective.to_guard_config()
    }

    /// Ensure the sealed line-1 capability profile is backed by the executor's
    /// line-2 database role. A least-privilege `migrator_<app>` role is the
    /// DB-enforced floor; a sealed profile that grants platform-only capabilities
    /// must not be paired with a config that will `SET ROLE` into that floor.
    #[cfg(test)]
    pub(crate) fn reconcile_with_executor_config(
        &self,
        exec_cfg: &ExecutorConfig,
    ) -> Result<(), SealError> {
        let Some(role) = exec_cfg.pg.migrator_role.as_deref() else {
            return Ok(());
        };
        if let Some(capability) = self.effective.migrator_unbacked_capability() {
            return Err(SealError::MigratorRoleMismatch {
                capability,
                role: role.to_string(),
            });
        }
        Ok(())
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
        validate_mac_key(key)?;
        let mut mac =
            HmacSha256::new_from_slice(key).expect("HMAC-SHA256 accepts any key length");
        let payload = SealPayload {
            effective: SealPayloadEffective {
                posture: self.effective.posture.into(),
                project_schema: &self.effective.project_schema,
                capabilities: &self.effective.capabilities,
                operational: &self.effective.operational,
                data_security: &self.effective.data_security,
                system_shape: &self.effective.system_shape,
            },
            nonce: &self.nonce,
            issued_at: self.issued_at,
            ceiling_version: self.ceiling_version,
        };
        let bytes = serde_json::to_vec(&payload).map_err(SealError::Serialize)?;
        mac.update(&bytes);
        Ok(mac)
    }
}

/// Seal an already-composed effective profile for shared-infra apply.
///
/// This is the public server seam over the crate-private [`SealedProfile::mint`]
/// constructor. It generates the nonce and issued-at timestamp internally,
/// validates the MAC key, and can only produce a [`SealedProfile`], whose
/// posture type has no `Trusted`/belt-skip variant.
///
/// # Errors
/// Returns [`SealError`] when the key is invalid or the effective profile claims
/// a knob the engine cannot yet enforce.
pub fn seal_effective_profile(
    effective: PolicyProfile,
    project_schema: &str,
    mac_key: &[u8],
    ceiling_version: u64,
) -> Result<SealedProfile, SealError> {
    let cap = SealApplier::new();
    SealedProfile::mint(
        &cap,
        effective,
        project_schema,
        mac_key,
        uuid::Uuid::new_v4().as_bytes().to_vec(),
        current_unix_secs(),
        ceiling_version,
    )
}

impl SealedEffectiveProfile {
    #[cfg(test)]
    fn migrator_unbacked_capability(&self) -> Option<&'static str> {
        let caps = &self.capabilities;
        if caps.role.allow {
            return Some("role");
        }
        if caps.grant {
            return Some("grant");
        }
        if caps.schema {
            return Some("schema");
        }
        if caps.rls {
            return Some("rls");
        }
        if caps.partition {
            return Some("partition");
        }
        if caps.policy {
            return Some("policy");
        }
        if caps.extension {
            return Some("extension");
        }
        if caps.raw_sql {
            return Some("raw_sql");
        }
        if caps.raw_view_body {
            return Some("raw_view_body");
        }
        if caps.materialized_view {
            return Some("materialized_view");
        }
        if caps.cross_schema {
            return Some("cross_schema");
        }
        None
    }
}

/// Verification context for a sealed profile.
#[derive(Debug, Clone)]
pub struct SealVerifier {
    key: Vec<u8>,
    current_ceiling_version: u64,
    max_age_secs: u64,
    consumed_nonces: NonceSeenSet,
    #[cfg(test)]
    now_override: Option<Arc<std::sync::atomic::AtomicU64>>,
}

impl PartialEq for SealVerifier {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
            && self.current_ceiling_version == other.current_ceiling_version
            && self.max_age_secs == other.max_age_secs
    }
}

impl Eq for SealVerifier {}

/// Process-local consumed-nonce store for the in-process MAC contract.
///
/// This crate has no shared durable store handle at the seal verifier layer; the
/// durable server will need to back this with its own transactional table if the
/// sealer/runner split across processes. The in-crate verifier still enforces
/// exactly-once for every verifier instance and its clones within the freshness
/// window; `issued_at`/`max_age_secs` remains the cross-restart safety bound.
#[derive(Debug, Clone, Default)]
struct NonceSeenSet {
    inner: Arc<Mutex<HashMap<Vec<u8>, u64>>>,
}

impl SealVerifier {
    /// Build a verifier from the active in-process MAC key and ceiling version.
    ///
    /// # Errors
    /// Returns [`SealError::InvalidKey`] when the MAC key is shorter than 32
    /// bytes.
    pub fn new(key: impl AsRef<[u8]>, current_ceiling_version: u64) -> Result<Self, SealError> {
        let key = key.as_ref();
        validate_mac_key(key)?;
        Ok(Self {
            key: key.to_vec(),
            current_ceiling_version,
            max_age_secs: SEAL_MAX_AGE_SECS,
            consumed_nonces: NonceSeenSet::default(),
            #[cfg(test)]
            now_override: None,
        })
    }

    #[cfg(test)]
    fn with_now(
        key: impl AsRef<[u8]>,
        current_ceiling_version: u64,
        now_unix_secs: u64,
    ) -> Result<Self, SealError> {
        let mut verifier = Self::new(key, current_ceiling_version)?;
        verifier.now_override = Some(Arc::new(std::sync::atomic::AtomicU64::new(
            now_unix_secs,
        )));
        Ok(verifier)
    }

    fn now_unix_secs(&self) -> u64 {
        #[cfg(test)]
        if let Some(now) = &self.now_override {
            return now.load(std::sync::atomic::Ordering::SeqCst);
        }

        current_unix_secs()
    }

    #[cfg(test)]
    fn set_now_for_test(&self, now_unix_secs: u64) {
        let Some(now) = &self.now_override else {
            panic!("set_now_for_test requires a verifier built with with_now");
        };
        now.store(now_unix_secs, std::sync::atomic::Ordering::SeqCst);
    }

    fn consume_nonce(&self, nonce: &[u8], expires_at: u64) -> Result<(), SealError> {
        let mut seen = self
            .consumed_nonces
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now_unix_secs = self.now_unix_secs();
        seen.retain(|_, expiry| *expiry >= now_unix_secs);
        if seen.contains_key(nonce) {
            return Err(SealError::ReplayedNonce);
        }
        seen.insert(nonce.to_vec(), expires_at);
        Ok(())
    }
}

fn validate_mac_key(key: &[u8]) -> Result<(), SealError> {
    if key.len() < MIN_SEAL_MAC_KEY_BYTES {
        return Err(SealError::InvalidKey);
    }
    Ok(())
}

fn current_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Sealed-profile verification failure.
#[derive(Debug, thiserror::Error)]
pub enum SealError {
    /// The configured MAC key was invalid.
    #[error("invalid sealed-profile MAC key")]
    InvalidKey,
    /// A PG timeout knob disabled the guard or exceeded the platform ceiling.
    #[error("{knob} must be non-zero and <= {ceiling_ms} ms (got {value} ms)")]
    InvalidTimeout {
        /// Dot-path timeout knob.
        knob: &'static str,
        /// Supplied timeout in milliseconds.
        value: u64,
        /// Platform ceiling in milliseconds.
        ceiling_ms: u64,
    },
    /// The profile's MAC did not verify.
    #[error("sealed migration policy profile did not verify")]
    InvalidSeal,
    /// The sealed profile is older than the accepted replay window.
    #[error("sealed profile issued_at {issued_at} is older than the {max_age_secs}s replay window at verifier time {now}: re-submit required")]
    StaleSeal {
        /// Issued-at timestamp embedded in the seal.
        issued_at: u64,
        /// Verifier wall-clock timestamp.
        now: u64,
        /// Maximum accepted age in seconds.
        max_age_secs: u64,
    },
    /// The sealed profile was issued after the verifier's wall clock.
    #[error("sealed profile issued_at {issued_at} is in the future relative to verifier time {now}")]
    IssuedInFuture {
        /// Issued-at timestamp embedded in the seal.
        issued_at: u64,
        /// Verifier wall-clock timestamp.
        now: u64,
    },
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
    /// A draft tried to loosen a permission knob beyond the operator ceiling.
    #[error("migration policy exceeds tier ceiling: {knob}")]
    PolicyExceedsCeiling {
        /// Dot-path knob that exceeded the ceiling.
        knob: &'static str,
    },
    /// A profile knob would claim a policy the stage-1 engine cannot enforce.
    #[error("migration policy profile knob `{knob}` is not yet supported by this engine; refusing to seal unenforced authority")]
    UnsupportedProfileKnob {
        /// Dot-path knob or knob family.
        knob: &'static str,
    },
    /// The nonce was already consumed by this verifier's seen-set.
    #[error("sealed profile nonce has already been consumed within the freshness window: re-submit required")]
    ReplayedNonce,
    /// The sealed line-1 capability profile is not backed by the configured
    /// least-privilege migrator role.
    #[error("capability not backed by migrator role `{role}`: {capability}")]
    MigratorRoleMismatch {
        /// Capability family that the migrator role is not allowed to exercise.
        capability: &'static str,
        /// The configured migrator role.
        role: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::capability::VendorCapabilities;
    use crate::model::policy::TrustProfile;
    use crate::render::declarative::{build_table_snapshot, CollectionDescriptor};
    use zero_migrate_schema::query::SqlDialect;

    const KEY: &[u8] = b"stage-1 test key for sealed migrate policy";
    const ISSUED_AT: u64 = 1_719_792_000;
    const VERIFY_NOW: u64 = ISSUED_AT + 1;

    fn verifier_at(now: u64, ceiling_version: u64) -> SealVerifier {
        SealVerifier::with_now(KEY, ceiling_version, now).unwrap()
    }

    #[test]
    fn policy_profile_toml_round_trips_and_matches_platform_caps() {
        let parsed = PolicyProfile::from_toml(PLATFORM_PROFILE_TOML).unwrap();
        assert_eq!(parsed.vendor_capabilities(), VendorCapabilities::operator());
        assert_eq!(parsed.system_shape, TableSystemShapePolicy::platform());
        let encoded = toml::to_string(&parsed).unwrap();
        assert!(
            encoded.contains("[system_shape]"),
            "system_shape must be part of the serialized profile: {encoded}"
        );
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
        assert!(!CONFINED_PROFILE_TOML.trim().is_empty());
        assert!(!PLATFORM_PROFILE_TOML.trim().is_empty());
        assert_eq!(
            PolicyProfile::from_toml(CONFINED_PROFILE_TOML)
                .unwrap()
                .vendor_capabilities(),
            VendorCapabilities::confined()
        );
        assert_eq!(
            PolicyProfile::from_toml(PLATFORM_PROFILE_TOML)
                .unwrap()
                .vendor_capabilities(),
            VendorCapabilities::operator()
        );
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
    fn confined_system_shape_matches_current_snapshot_injection() {
        let confined = PolicyProfile::confined();
        assert_eq!(
            confined.system_shape.author_primary_key,
            AuthorPrimaryKeyPolicy::Forbid
        );
        assert_eq!(
            confined.system_shape.primary_key,
            TablePrimaryKeyPolicy::ExplicitColumns(vec!["id".to_string()])
        );

        let expected_columns = vec![
            ("id", "text", false),
            ("created_at", "timestamp with time zone", false),
            ("updated_at", "timestamp with time zone", false),
            ("created_by", "text", true),
            ("updated_by", "text", true),
            ("version", "integer", false),
            ("deleted_at", "timestamp with time zone", true),
        ];
        let policy_columns = confined
            .system_shape
            .columns
            .iter()
            .map(|c| (c.name.as_str(), c.data_type.as_str(), c.nullable))
            .collect::<Vec<_>>();
        assert_eq!(policy_columns, expected_columns);

        let expected_indexes = vec![vec!["deleted_at"], vec!["updated_at"], vec!["created_by"]];
        let policy_indexes = confined
            .system_shape
            .indexes
            .iter()
            .map(|idx| idx.columns.iter().map(String::as_str).collect::<Vec<_>>())
            .collect::<Vec<_>>();
        assert_eq!(policy_indexes, expected_indexes);

        let descriptor = CollectionDescriptor {
            name: "widgets".into(),
            owner_app: "app_test".into(),
            fields: Vec::new(),
            indexes: Vec::new(),
            runtime_options: Default::default(),
        };
        let snapshot = build_table_snapshot("app", &descriptor, SqlDialect::Postgres)
            .expect("empty table snapshot should build");

        let mut snapshot_columns = snapshot
            .columns
            .iter()
            .map(|c| (c.name.as_str(), c.data_type.as_str(), c.nullable))
            .collect::<Vec<_>>();
        let mut sorted_policy_columns = policy_columns.clone();
        snapshot_columns.sort_unstable();
        sorted_policy_columns.sort_unstable();
        assert_eq!(sorted_policy_columns, snapshot_columns);

        let mut snapshot_system_indexes = snapshot
            .indexes
            .iter()
            .filter(|idx| !idx.unique)
            .map(|idx| {
                assert_eq!(idx.access_method, "btree");
                assert_eq!(idx.predicate, None);
                idx.columns.iter().map(String::as_str).collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let mut sorted_policy_indexes = policy_indexes.clone();
        snapshot_system_indexes.sort_unstable();
        sorted_policy_indexes.sort_unstable();
        assert_eq!(sorted_policy_indexes, snapshot_system_indexes);

        assert!(snapshot.constraints.iter().any(|c| {
            c.name == "widgets_pkey"
                && c.kind == "PRIMARY KEY"
                && c.definition == "PRIMARY KEY (id)"
        }));
        assert!(snapshot.indexes.iter().any(|idx| {
            idx.name == "widgets_pkey" && idx.unique && idx.columns == ["id"]
        }));
    }

    #[test]
    fn platform_and_default_system_shapes_are_explicit() {
        assert_eq!(
            PolicyProfile::default().system_shape,
            PolicyProfile::confined().system_shape
        );

        let platform = PolicyProfile::platform();
        assert!(platform.system_shape.columns.is_empty());
        assert!(platform.system_shape.indexes.is_empty());
        assert_eq!(
            platform.system_shape.primary_key,
            TablePrimaryKeyPolicy::Author(PrimaryKeyAuthorPolicy::Author)
        );
        assert_eq!(
            platform.system_shape.author_primary_key,
            AuthorPrimaryKeyPolicy::Allow
        );
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
    fn policy_profile_rejects_zero_pg_timeouts_at_load() {
        let lock_err = PolicyProfile::from_toml(
            r#"
            [operational]
            lock_timeout_ms = 0
            "#,
        )
        .unwrap_err();
        assert!(
            lock_err.to_string().contains("operational.lock_timeout_ms"),
            "unexpected error: {lock_err}"
        );

        let statement_err = PolicyProfile::from_toml(
            r#"
            [operational]
            statement_timeout_ms = 0
            "#,
        )
        .unwrap_err();
        assert!(
            statement_err
                .to_string()
                .contains("operational.statement_timeout_ms"),
            "unexpected error: {statement_err}"
        );
    }

    #[test]
    fn policy_profile_rejects_pg_timeouts_above_platform_ceiling_at_load() {
        let err = PolicyProfile::from_toml(&format!(
            r#"
            [operational]
            lock_timeout_ms = {}
            "#,
            PLATFORM_LOCK_TIMEOUT_CEILING_MS + 1
        ))
        .unwrap_err();
        assert!(
            err.to_string().contains("<= 3000 ms"),
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
        assert!(PolicyProfile::polarity_table().iter().any(|k| {
            k.key == "operational.lock_timeout_ms"
                && k.polarity == PolicyPolarity::Permission
                && k.meet == PolicyMeet::MinNonZero
        }));
    }

    #[test]
    fn timeout_meet_picks_smaller_non_zero_value() {
        assert_eq!(min_non_zero_timeout_ms(3_000, 1_000), Some(1_000));
        assert_eq!(min_non_zero_timeout_ms(0, 1_000), Some(1_000));
        assert_eq!(min_non_zero_timeout_ms(3_000, 0), Some(3_000));
        assert_eq!(min_non_zero_timeout_ms(0, 0), None);
    }

    #[test]
    fn data_security_meet_preserves_obligations_and_rejects_permission_loosen() {
        let ceiling = DataSecurityConfig {
            require_rls: true,
            destructive_ops: DestructiveOps::Forbid,
            ..DataSecurityConfig::default()
        };
        let loosening_draft = DataSecurityConfig {
            require_rls: false,
            destructive_ops: DestructiveOps::Allow,
            ..DataSecurityConfig::default()
        };

        assert!(matches!(
            DataSecurityConfig::meet_ceiling_draft(&ceiling, &loosening_draft),
            Err(SealError::PolicyExceedsCeiling {
                knob: "data_security.destructive_ops"
            })
        ));

        let tight_draft = DataSecurityConfig {
            require_rls: false,
            destructive_ops: DestructiveOps::Forbid,
            ..DataSecurityConfig::default()
        };
        let effective = DataSecurityConfig::meet_ceiling_draft(&ceiling, &tight_draft).unwrap();
        assert!(effective.require_rls, "ceiling require_rls must survive draft=false");
        assert_eq!(effective.destructive_ops, DestructiveOps::Forbid);

        let ceiling = DataSecurityConfig {
            destructive_ops: DestructiveOps::Allow,
            ..DataSecurityConfig::default()
        };
        let draft = DataSecurityConfig {
            destructive_ops: DestructiveOps::Warn,
            ..DataSecurityConfig::default()
        };
        let effective = DataSecurityConfig::meet_ceiling_draft(&ceiling, &draft).unwrap();
        assert_eq!(effective.destructive_ops, DestructiveOps::Warn);
    }

    #[test]
    fn data_security_meet_keeps_require_approval_stricter_than_warn() {
        let ceiling = DataSecurityConfig {
            destructive_ops: DestructiveOps::RequireApproval,
            ..DataSecurityConfig::default()
        };
        let draft = DataSecurityConfig {
            destructive_ops: DestructiveOps::Warn,
            ..DataSecurityConfig::default()
        };

        let effective = DataSecurityConfig::meet_ceiling_draft(&ceiling, &draft).unwrap();

        assert_eq!(effective.destructive_ops, DestructiveOps::RequireApproval);
    }

    #[test]
    fn public_seam_composes_and_seals_tighter_profile_without_trusted_posture() {
        let ceiling = PolicyProfile::confined();
        let mut draft = PolicyProfile::confined();
        draft.operational.lock_timeout_ms = 1_000;
        draft.data_security.require_rls = true;
        draft.data_security.destructive_ops = DestructiveOps::Forbid;

        let effective = crate::PolicyProfile::meet_ceiling_draft(&ceiling, &draft)
            .expect("tighter draft should compose under the ceiling");

        assert_eq!(effective.operational.lock_timeout_ms, 1_000);
        assert!(effective.data_security.require_rls);
        assert_eq!(effective.data_security.destructive_ops, DestructiveOps::Forbid);
        assert_eq!(effective.vendor_capabilities(), VendorCapabilities::confined());

        let sealed = crate::seal_effective_profile(effective, "app", KEY, 7)
            .expect("composed effective profile should seal through the public seam");
        sealed.verify(&SealVerifier::new(KEY, 7).unwrap()).unwrap();
        assert_eq!(sealed.posture(), SealedPosture::Confined);

        let guard_cfg = sealed.to_guard_config();
        assert_ne!(guard_cfg.trust(), TrustProfile::Trusted);
        assert!(!guard_cfg.skips_denylist_belt());
        assert!(guard_cfg.require_rls());
        assert_eq!(guard_cfg.destructive_ops(), DestructiveOps::Forbid);

        let guard = crate::guard::SqlGuard::new(guard_cfg);
        assert!(matches!(
            guard.check("DROP TABLE users"),
            Err(crate::guard::GuardError::DataSecurityPolicy {
                rule: crate::guard::data_security_rule::DESTRUCTIVE_OPS_FORBID,
                ..
            })
        ));
    }

    #[test]
    fn policy_profile_meet_rejects_draft_permission_escalation() {
        let ceiling = PolicyProfile::confined();
        let mut draft = PolicyProfile::confined();
        draft.capabilities.raw_sql = true;

        assert!(matches!(
            PolicyProfile::meet_ceiling_draft(&ceiling, &draft),
            Err(SealError::PolicyExceedsCeiling {
                knob: "capabilities.raw_sql"
            })
        ));
    }

    #[test]
    fn policy_profile_meet_rejects_confined_ceiling_empty_system_shape_escape() {
        let ceiling = PolicyProfile::confined();
        let mut draft = PolicyProfile::confined();
        draft.system_shape = TableSystemShapePolicy::platform();

        assert!(matches!(
            PolicyProfile::meet_ceiling_draft(&ceiling, &draft),
            Err(SealError::PolicyExceedsCeiling {
                knob: "system_shape"
            })
        ));
    }

    #[test]
    fn operational_lowering_only_tightens_executor_timeouts() {
        let op = OperationalConfig::default();
        let mut exec_cfg = ExecutorConfig::new("prj_test", "app");
        exec_cfg.pg.lock_timeout = Duration::from_millis(500);
        exec_cfg.pg.statement_timeout = Duration::from_millis(1_000);

        op.apply_to_executor_config(&mut exec_cfg);

        assert_eq!(exec_cfg.pg.lock_timeout, Duration::from_millis(500));
        assert_eq!(
            exec_cfg.pg.statement_timeout,
            Duration::from_millis(1_000)
        );
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
            ISSUED_AT,
            7,
        )
        .unwrap();
        sealed.verify(&verifier_at(VERIFY_NOW, 7)).unwrap();
        assert_eq!(sealed.posture(), SealedPosture::Platform);
        assert_eq!(sealed.effective.system_shape, TableSystemShapePolicy::platform());
    }

    #[test]
    fn sealed_profile_mac_covers_system_shape() {
        let cap = SealApplier::new();
        let mut sealed = SealedProfile::mint(
            &cap,
            PolicyProfile::confined(),
            "app",
            KEY,
            b"nonce-system-shape".to_vec(),
            ISSUED_AT,
            7,
        )
        .unwrap();
        sealed.effective.system_shape = TableSystemShapePolicy::platform();

        assert!(matches!(
            sealed.verify(&verifier_at(VERIFY_NOW, 7)),
            Err(SealError::InvalidSeal)
        ));
    }

    #[test]
    fn sealed_profile_rejects_nonce_replay_within_freshness_window() {
        let cap = SealApplier::new();
        let sealed = SealedProfile::mint(
            &cap,
            PolicyProfile::platform(),
            "app",
            KEY,
            b"nonce-replay".to_vec(),
            ISSUED_AT,
            7,
        )
        .unwrap();
        let verifier = verifier_at(VERIFY_NOW, 7);

        sealed.verify(&verifier).unwrap();
        assert!(matches!(
            sealed.verify(&verifier),
            Err(SealError::ReplayedNonce)
        ));

        let cloned_verifier = verifier.clone();
        assert!(matches!(
            sealed.verify(&cloned_verifier),
            Err(SealError::ReplayedNonce)
        ));
    }

    #[test]
    fn sealed_profile_to_guard_config_never_enables_belt_skip() {
        let cap = SealApplier::new();
        for profile in [PolicyProfile::confined(), PolicyProfile::platform()] {
            let sealed = SealedProfile::mint(
                &cap,
                profile,
                "app",
                KEY,
                uuid::Uuid::new_v4().as_bytes().to_vec(),
                ISSUED_AT,
                7,
            )
            .unwrap();
            let guard = sealed.to_guard_config();
            assert_ne!(guard.trust(), TrustProfile::Trusted);
            assert!(
                !guard.skips_denylist_belt(),
                "SealedProfile lowering must never carry the Trusted belt-skip"
            );
        }
    }

    #[test]
    fn sealed_profile_reconciles_platform_caps_against_migrator_role_floor() {
        let cap = SealApplier::new();
        let sealed = SealedProfile::mint(
            &cap,
            PolicyProfile::platform(),
            "app",
            KEY,
            b"nonce-role-floor".to_vec(),
            ISSUED_AT,
            7,
        )
        .unwrap();
        let exec_cfg = ExecutorConfig::new("prj_app", "app").with_migrator_role("migrator_app");

        assert!(matches!(
            sealed.reconcile_with_executor_config(&exec_cfg),
            Err(SealError::MigratorRoleMismatch {
                capability: "role",
                role
            }) if role == "migrator_app"
        ));
    }

    #[test]
    fn sealed_confined_profile_is_backed_by_migrator_role_floor() {
        let cap = SealApplier::new();
        let sealed = SealedProfile::mint(
            &cap,
            PolicyProfile::confined(),
            "app",
            KEY,
            b"nonce-confined-floor".to_vec(),
            ISSUED_AT,
            7,
        )
        .unwrap();
        let exec_cfg = ExecutorConfig::new("prj_app", "app").with_migrator_role("migrator_app");

        sealed
            .reconcile_with_executor_config(&exec_cfg)
            .expect("confined sealed profile must agree with the least-privilege migrator floor");
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
            ISSUED_AT,
            7,
        )
        .unwrap();
        sealed.tamper_first_nonce_byte();
        assert!(matches!(
            sealed.verify(&verifier_at(VERIFY_NOW, 7)),
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
            ISSUED_AT,
            7,
        )
        .unwrap();
        assert!(matches!(
            sealed.verify(&verifier_at(VERIFY_NOW, 8)),
            Err(SealError::SupersededCeiling {
                sealed: 7,
                current: 8
            })
        ));
    }

    #[test]
    fn sealed_profile_rejects_stale_issued_at() {
        let cap = SealApplier::new();
        let now = 2_000_000_000;
        let issued_at = now - SEAL_MAX_AGE_SECS - 1;
        let sealed = SealedProfile::mint(
            &cap,
            PolicyProfile::platform(),
            "app",
            KEY,
            b"nonce-1".to_vec(),
            issued_at,
            7,
        )
        .unwrap();

        assert!(matches!(
            sealed.verify(&verifier_at(now, 7)),
            Err(SealError::StaleSeal {
                issued_at: got_issued_at,
                now: got_now,
                max_age_secs: SEAL_MAX_AGE_SECS
            }) if got_issued_at == issued_at && got_now == now
        ));
    }

    #[test]
    fn sealed_profile_verify_uses_verify_time_not_construction_time() {
        let cap = SealApplier::new();
        let constructed_at = 2_000_000_000;
        let verify_at = constructed_at + SEAL_MAX_AGE_SECS + 1;
        let sealed = SealedProfile::mint(
            &cap,
            PolicyProfile::platform(),
            "app",
            KEY,
            b"nonce-verify-time".to_vec(),
            constructed_at,
            7,
        )
        .unwrap();
        let verifier = verifier_at(constructed_at, 7);

        verifier.set_now_for_test(verify_at);

        assert!(matches!(
            sealed.verify(&verifier),
            Err(SealError::StaleSeal {
                issued_at,
                now,
                max_age_secs: SEAL_MAX_AGE_SECS
            }) if issued_at == constructed_at && now == verify_at
        ));
    }

    #[test]
    fn sealed_profile_rejects_short_mac_keys_at_constructor_and_mint() {
        assert!(matches!(
            SealVerifier::new(b"short", 7),
            Err(SealError::InvalidKey)
        ));

        let cap = SealApplier::new();
        assert!(matches!(
            SealedProfile::mint(
                &cap,
                PolicyProfile::platform(),
                "app",
                b"",
                b"nonce-1".to_vec(),
                ISSUED_AT,
                7,
            ),
            Err(SealError::InvalidKey)
        ));
    }

    #[test]
    fn sealed_profile_accepts_data_security_claims_and_lowers_to_enforcing_guard() {
        let cap = SealApplier::new();
        let mut profile = PolicyProfile::confined();
        profile.data_security.require_rls = true;
        profile.data_security.destructive_ops = DestructiveOps::Forbid;

        let sealed = SealedProfile::mint(
            &cap,
            profile,
            "app",
            KEY,
            b"nonce-data-security".to_vec(),
            ISSUED_AT,
            7,
        )
        .expect("require_rls/destructive_ops are now enforceable and sealable");
        let guard_cfg = sealed.to_guard_config();
        assert!(guard_cfg.require_rls());
        assert_eq!(guard_cfg.destructive_ops(), DestructiveOps::Forbid);

        let guard = crate::guard::SqlGuard::new(guard_cfg.clone());
        assert!(matches!(
            guard.check("DROP TABLE users"),
            Err(crate::guard::GuardError::DataSecurityPolicy {
                rule: crate::guard::data_security_rule::DESTRUCTIVE_OPS_FORBID,
                ..
            })
        ));

        let ir = crate::model::ir::MigrationIr {
            ir_version: crate::model::ir::CURRENT_IR_VERSION,
            name: "missing_rls".to_string(),
            owner_app: "app".to_string(),
            ops: vec![crate::model::ir::Op::CreateTable {
                name: "users".to_string(),
                columns: Vec::new(),
                primary_key: None,
                constraints: Vec::new(),
                indexes: Vec::new(),
                partition_by: None,
                runtime_options: None,
                schema: None,
                existence_guard: None,
            }],
            flags: Default::default(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
            checksum: None,
        };
        let err = crate::guard::check_ir_data_security_policy(&guard_cfg, &ir).unwrap_err();
        assert!(matches!(
            err.source,
            crate::guard::GuardError::DataSecurityPolicy {
                rule: crate::guard::data_security_rule::REQUIRE_RLS,
                ..
            }
        ));
    }

    #[test]
    fn sealed_profile_mints_composed_ceiling_draft_and_enforces_data_security() {
        let cap = SealApplier::new();
        let mut ceiling = PolicyProfile::confined();
        ceiling.data_security.require_rls = true;
        ceiling.data_security.destructive_ops = DestructiveOps::Forbid;
        let mut draft = PolicyProfile::confined();
        draft.data_security.require_rls = false;
        draft.data_security.destructive_ops = DestructiveOps::Forbid;

        let sealed = SealedProfile::mint_composed(
            &cap,
            ceiling,
            draft,
            "app",
            KEY,
            b"nonce-composed-data-security".to_vec(),
            ISSUED_AT,
            7,
        )
        .expect("ceiling obligations compose into the sealed profile");
        let guard_cfg = sealed.to_guard_config();

        assert!(guard_cfg.require_rls());
        assert_eq!(guard_cfg.destructive_ops(), DestructiveOps::Forbid);

        let guard = crate::guard::SqlGuard::new(guard_cfg.clone());
        assert!(matches!(
            guard.check("DROP VIEW users_view"),
            Err(crate::guard::GuardError::DataSecurityPolicy {
                rule: crate::guard::data_security_rule::DESTRUCTIVE_OPS_FORBID,
                ..
            })
        ));

        let ir = crate::model::ir::MigrationIr {
            ir_version: crate::model::ir::CURRENT_IR_VERSION,
            name: "composed_missing_rls".to_string(),
            owner_app: "app".to_string(),
            ops: vec![crate::model::ir::Op::CreateTable {
                name: "users".to_string(),
                columns: Vec::new(),
                primary_key: None,
                constraints: Vec::new(),
                indexes: Vec::new(),
                partition_by: None,
                runtime_options: None,
                schema: None,
                existence_guard: None,
            }],
            flags: Default::default(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
            checksum: None,
        };
        let err = crate::guard::check_ir_data_security_policy(&guard_cfg, &ir).unwrap_err();
        assert!(matches!(
            err.source,
            crate::guard::GuardError::DataSecurityPolicy {
                rule: crate::guard::data_security_rule::REQUIRE_RLS,
                ..
            }
        ));
    }

    #[test]
    fn sealed_platform_require_rls_rejects_raw_island_table_creation() {
        let cap = SealApplier::new();
        let ceiling = PolicyProfile::platform();
        let mut draft = PolicyProfile::platform();
        draft.data_security.require_rls = false;

        let sealed = SealedProfile::mint_composed(
            &cap,
            ceiling,
            draft,
            "zeroship",
            KEY,
            b"nonce-sealed-raw-rls".to_vec(),
            ISSUED_AT,
            7,
        )
        .expect("platform ceiling composes require_rls back into the draft");
        let guard_cfg = sealed.to_guard_config();
        assert!(guard_cfg.require_rls());

        let author = crate::render::lower::IrAuthor::new(
            "zeroship",
            "app_corpus",
            zero_migrate_schema::query::SqlDialect::Postgres,
        )
        .with_schema_scope(
            guard_cfg
                .schema_scope()
                .expect("sealed platform profile carries a schema scope"),
        );
        let ir = crate::model::ir::MigrationIr {
            ir_version: crate::model::ir::CURRENT_IR_VERSION,
            name: "sealed_raw_rls".to_string(),
            owner_app: "app_corpus".to_string(),
            ops: vec![crate::model::ir::Op::PgRaw {
                sql: "CREATE TABLE zeroship.raw_users AS SELECT 1 AS id".to_string(),
                reason: "require_rls raw table creation regression".to_string(),
            }],
            flags: Default::default(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
            checksum: None,
        };

        match author.lower_guarded(
            &ir,
            &guard_cfg,
            &crate::render::lower::LiveSchema::default(),
        ) {
            Err(crate::render::lower::IrGuardedLowerError::Denied(denial)) => {
                assert_eq!(denial.op_kind, "pgRaw");
                assert!(matches!(
                    denial.source,
                    crate::guard::GuardError::DataSecurityPolicy {
                        rule: crate::guard::data_security_rule::REQUIRE_RLS,
                        ..
                    }
                ));
            }
            other => panic!("sealed require_rls profile must fail-close pgRaw table creation; got {other:?}"),
        }
    }

    #[test]
    fn sealed_profile_rejects_partial_granular_capability_claims_not_yet_enforced() {
        let cap = SealApplier::new();
        let mut profile = PolicyProfile::confined();
        profile.capabilities.raw_sql = true;

        assert!(matches!(
            SealedProfile::mint(
                &cap,
                profile,
                "app",
                KEY,
                b"nonce-1".to_vec(),
                ISSUED_AT,
                7,
            ),
            Err(SealError::UnsupportedProfileKnob {
                knob: "capabilities.granular"
            })
        ));
    }

    #[test]
    fn sealed_profile_type_has_no_trusted_posture() {
        let sealed = crate::seal_effective_profile(PolicyProfile::platform(), "app", KEY, 7)
            .unwrap();
        match sealed.posture() {
            SealedPosture::Confined | SealedPosture::Platform => {}
        }
    }
}
