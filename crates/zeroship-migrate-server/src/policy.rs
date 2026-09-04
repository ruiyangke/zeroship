//! The managed-policy server: the monorepo's operator-ceiling ⊓ creator-draft
//! composition + seal, on top of the published `zero-migrate` Policy Decision Point.
//!
//! # The model (Phase 3 of the policy redesign)
//!
//! The engine's old `PolicyProfile` value-struct, its `meet_ceiling_draft` compose,
//! and its `seal_effective_profile` HMAC are DELETED. This module rebuilds the
//! managed server on the engine's surviving PDP:
//!
//! - the OPERATOR CEILING is a [`zeroship_migrate_policy::RootCharter`] — a [`PolicyDoc`]
//!   loaded [`LoadContext::RootCharter`] (the only layer that may carry a `mandatory`
//!   inject). The default ceiling is the monorepo-owned CONFINED document embedded
//!   below; named tiers add more ceilings to the [`ProfileCatalog`].
//! - the CREATOR DRAFT is an untrusted [`PolicyDoc`] loaded [`LoadContext::NonRootLayer`].
//! - the EFFECTIVE policy is [`admit`]`(ceiling, draft)` — operator ⊓ creator
//!   with ESCALATION-REJECT (a draft grant looser than the ceiling permits is
//!   rejected, never clamped). This is the direct replacement for `meet_ceiling_draft`.
//! - the SEAL is the `zeroship-migrate-policy` HMAC over the composed [`EffectivePolicy`]
//!   ([`fn@zeroship_migrate::seal`] / [`SealedPolicy::verify`]).
//!
//! The composed engine [`EffectivePolicy`] drives table-shape injection
//! (`resolve_create_table_policy`) and escalation-reject. The rendered-DDL guard uses
//! a separate authored confined charter with the same grants and exact app-schema
//! binding, but no inject rule: lower enforces the managed shape, while the guard
//! remains the confinement and dangerous-operation belt around the rendered plan.

use std::collections::BTreeMap;

use uuid::Uuid;
use zeroship_migrate::{effective_policy_from_charter_toml, seal, DestructiveOps, SealError, SealedPolicy};
use zeroship_migrate_ir::policy_approval::{require_approval_level, ApprovalLevel};
use zeroship_migrate_ir::policy_registry::{
    builtin_registry, KEY_CODE_EXTENSION, KEY_SAFETY_DESTRUCTIVE_OPS, KEY_SAFETY_REQUIRE_RLS,
    KEY_SCHEMA_CREATE_SCHEMA, KEY_SCHEMA_CREATE_TABLE, KEY_SCHEMA_CROSS_SCHEMA, KEY_SCHEMA_RENAME,
};
use zeroship_migrate_policy::{
    admit, ComposeError, EffectivePolicy as PdpPolicy, KnobKey, KnobValue, LoadContext,
    LoadError, ObjectName, PolicyDoc, RootCharter,
};

/// The seal binding: the scope matcher folds Postgres identifiers under this dialect
/// and matcher-algorithm version. Bound into the HMAC so a seal minted under one
/// matcher semantics cannot be replayed under another.
const SEAL_DIALECT: &str = "postgres";
/// The scope-matcher algorithm version bound into the seal.
const SEAL_MATCHER_VERSION: u32 = 1;

/// The monorepo-owned CONFINED ceiling — the default operator ceiling a creator app
/// gets (the successor to the engine's deleted `PolicyProfile::confined()`).
///
/// Assembled from two files. The grants are this crate's own; the mandatory
/// system-table `[[inject]]` rule is the platform-wide fragment in `policies/`,
/// which every other consumer of that rule also concatenates rather than copies.
/// `concat!` folds both `include_str!`s at compile time, so the fragment's bytes
/// are literally in this binary.
pub const CONFINED_CEILING_TOML: &str = concat!(
    include_str!("../policies/confined.policy.toml"),
    include_str!("../../../policies/confined-system-shape.inject.toml"),
);
/// The monorepo-owned no-inject charter used only to guard DDL rendered by managed
/// lowering. Its grants mirror [`CONFINED_CEILING_TOML`]; its inject block is omitted.
const CONFINED_GUARD_CHARTER_TOML: &str =
    include_str!("../policies/confined-guard.policy.toml");
/// The monorepo-owned PLATFORM ceiling — the operator-internal (author-owned,
/// no-inject) posture (the successor to `PolicyProfile::platform()`).
pub const PLATFORM_CEILING_TOML: &str = include_str!("../policies/platform.policy.toml");

pub const MIGRATE_POLICY_FILENAME: &str = "migrate-policy.toml";

/// The managed-policy server configuration: the tier catalog + the seal MAC key.
#[derive(Debug, Clone)]
pub struct ManagedPolicyConfig {
    catalog: ProfileCatalog,
    mac_key: Vec<u8>,
}

impl ManagedPolicyConfig {
    pub fn new(
        mac_key: impl Into<Vec<u8>>,
        catalog: ProfileCatalog,
    ) -> Result<Self, ManagedPolicyError> {
        let mac_key = mac_key.into();
        if mac_key.len() < 32 {
            return Err(ManagedPolicyError::SealKeyTooShort {
                actual: mac_key.len(),
            });
        }
        // Prove the default ceiling loads + composes at construction time (fail fast
        // on a malformed embedded/operator ceiling rather than per-request).
        catalog
            .default_ceiling()
            .effective_for_app_schema("__zeroship_policy_validation__")?;
        Ok(Self { catalog, mac_key })
    }

    pub fn default_confined(
        mac_key: impl Into<Vec<u8>>,
        ceiling_version: u64,
    ) -> Result<Self, ManagedPolicyError> {
        Self::new(mac_key, ProfileCatalog::default_confined(ceiling_version)?)
    }

    /// Parse an untrusted creator draft into a [`ParsedDraft`]: the non-root
    /// [`PolicyDoc`] of grant/inject/require/validate rules, composed by the engine PDP.
    ///
    /// Approval is a NORMAL sealed obligation now — a creator (or the operator ceiling)
    /// authors `[[require]] key = "safety.require_approval"` like any other knob; there is
    /// no managed-only overlay to strip. The engine's `deny_unknown_fields` loader
    /// validates the whole draft.
    pub fn parse_draft(&self, draft: &CreatorPolicyDraft<'_>) -> Result<ParsedDraft, ManagedPolicyError> {
        if draft.filename != MIGRATE_POLICY_FILENAME {
            return Err(ManagedPolicyError::InvalidDraftFilename {
                filename: draft.filename.to_string(),
            });
        }
        parse_draft_body(draft.body, draft.filename)
    }

    /// Compose the operator ceiling ⊓ the (optional) creator draft into the effective
    /// managed policy. A draft that escalates beyond the ceiling is REJECTED here
    /// (`admit`), not clamped.
    pub fn compose_effective_for_app(
        &self,
        app_id: &Uuid,
        tier: Option<&str>,
        draft: Option<&ParsedDraft>,
    ) -> Result<EffectivePolicy, ManagedPolicyError> {
        let ceiling = self.catalog.resolve(app_id, tier);
        let app_base = ceiling.effective_for_app_schema(&app_id.to_string())?;
        let policy = match draft {
            // Admit the untrusted creator draft only after the operator charter has
            // been bound to this app's exact schema. Any grant outside it is rejected.
            Some(draft) => admit(&app_base, &draft.doc, &builtin_registry())
                .map_err(ManagedPolicyError::Compose)?,
            None => app_base,
        };
        Ok(EffectivePolicy::new(
            ceiling.id.clone(),
            ceiling.ceiling_version,
            policy,
        ))
    }

    /// The current operator ceiling for the app (no creator draft) — the composed
    /// ceiling-only effective policy.
    pub fn current_ceiling_for_app(
        &self,
        app_id: &Uuid,
        tier: Option<&str>,
    ) -> Result<EffectivePolicy, ManagedPolicyError> {
        self.compose_effective_for_app(app_id, tier, None)
    }

    pub fn seal_for_app(
        &self,
        app_id: &Uuid,
        tier: Option<&str>,
        draft: Option<CreatorPolicyDraft<'_>>,
    ) -> Result<SealedManagedPolicy, ManagedPolicyError> {
        let parsed_draft = draft.as_ref().map(|draft| self.parse_draft(draft)).transpose()?;
        let effective = self.compose_effective_for_app(app_id, tier, parsed_draft.as_ref())?;
        self.seal_effective_for_app(effective)
    }

    pub(crate) fn seal_effective_for_app(
        &self,
        effective: EffectivePolicy,
    ) -> Result<SealedManagedPolicy, ManagedPolicyError> {
        let nonce = mint_nonce();
        let sealed = seal(
            &effective.policy,
            &self.mac_key,
            nonce,
            SEAL_DIALECT,
            SEAL_MATCHER_VERSION,
            effective.ceiling_version,
        );
        let verifier = SealVerifier {
            mac_key: self.mac_key.clone(),
            registry_digest: effective.policy.registry().digest(),
            ceiling_version: effective.ceiling_version,
        };
        Ok(SealedManagedPolicy {
            ceiling_id: effective.ceiling_id,
            ceiling_version: effective.ceiling_version,
            effective: effective.policy,
            managed: effective.managed,
            sealed,
            verifier,
        })
    }
}

/// A per-request non-secret nonce for the seal (freshness). Derived from a UUIDv7 so
/// it is monotonic + unique without pulling a full RNG dependency.
fn mint_nonce() -> [u8; 16] {
    *Uuid::now_v7().as_bytes()
}

/// The effective managed policy for one app: the composed engine PDP policy (the
/// source for injection + escalation-reject) plus the ceiling identity and its
/// derived managed posture for approval decisions and audit records.
#[derive(Debug, Clone)]
pub struct EffectivePolicy {
    pub ceiling_id: String,
    pub ceiling_version: u64,
    /// The composed, unforgeable engine PDP policy — the source for
    /// `resolve_create_table_policy`, lowering, and managed approval decisions.
    pub policy: PdpPolicy,
    /// The three managed knobs (`require_rls`, `destructive_ops`, `extensions`) read
    /// back out of `policy` for approval decisions and audit records.
    pub managed: ManagedPosture,
}

impl EffectivePolicy {
    fn new(ceiling_id: String, ceiling_version: u64, policy: PdpPolicy) -> Self {
        let managed = ManagedPosture::from_policy(&policy);
        Self { ceiling_id, ceiling_version, policy, managed }
    }

    /// The effective `safety.require_approval` obligation level for this app's schema —
    /// the SEALED approval obligation (`never`/`on_destructive`/`always`) the engine
    /// only DECLARES. Resolved at the object the app owns (`app_schema`); the host
    /// enforces it as the state machine. Replaces the deleted `require_approval`
    /// overlay bool.
    #[must_use]
    pub fn approval_level(&self, app_schema: &str) -> ApprovalLevel {
        require_approval_level(&self.policy, &schema_object(app_schema))
    }

}

/// The managed knobs read from the composed engine policy for approval decisions and
/// audit records. Approval is NOT one of these — it is the separate sealed
/// `safety.require_approval` obligation the host enforces (see
/// [`EffectivePolicy::approval_level`]), never a `destructive_ops` state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedPosture {
    pub require_rls: bool,
    pub destructive_ops: DestructiveOps,
    pub extensions: Vec<String>,
}

impl ManagedPosture {
    /// Derive the managed posture from the composed engine policy's decision queries.
    ///
    /// `destructive_ops` maps the engine's `safety.destructive_ops` OrderedEnum
    /// (`forbid`/`warn`/`allow`) back onto [`DestructiveOps`]. Approval is composed
    /// SEPARATELY as the `safety.require_approval` obligation (see
    /// [`EffectivePolicy::approval_level`]) — it is not folded into this posture.
    fn from_policy(policy: &PdpPolicy) -> Self {
        let all = ObjectName::table(b"any".to_vec(), b"any".to_vec());
        let destructive_ops = match policy
            .grants(&key(KEY_SAFETY_DESTRUCTIVE_OPS), &all)
        {
            Some(KnobValue::Str(v)) if v == "allow" => DestructiveOps::Allow,
            Some(KnobValue::Str(v)) if v == "warn" => DestructiveOps::Warn,
            // `forbid` (the tightest) or any unexpected shape → fail closed to Forbid.
            _ => DestructiveOps::Forbid,
        };
        let require_rls = policy
            .obligations(&all)
            .into_iter()
            .any(|(k, v)| k.as_str() == KEY_SAFETY_REQUIRE_RLS && matches!(v, KnobValue::Bool(true)));
        // `code.extension` is a Global StrSet grant (the allowlist IS the capability);
        // query it at any in-scope object.
        let extensions = match policy.grants(&key(KEY_CODE_EXTENSION), &all) {
            Some(KnobValue::StrSet(names)) => names,
            _ => Vec::new(),
        };
        Self { require_rls, destructive_ops, extensions }
    }
}

impl ManagedPosture {
    /// A stable JSON audit view of the managed posture (persisted in the policy /
    /// migration stores' `*_profile` JSONB columns, read as human/query audit — the
    /// engine `EffectivePolicy` is not itself serde-serializable, and the composed
    /// policy is always re-derivable from the stored draft TOML + the current ceiling).
    #[must_use]
    pub fn to_audit_json(&self) -> serde_json::Value {
        serde_json::json!({
            "require_rls": self.require_rls,
            "destructive_ops": format!("{:?}", self.destructive_ops),
            "extensions": self.extensions,
        })
    }
}

fn key(k: &str) -> KnobKey {
    KnobKey::parse(k).expect("builtin knob key is well-formed")
}

fn schema_object(schema: &str) -> ObjectName {
    ObjectName::schema(schema.as_bytes().to_vec())
}

/// The named-tier ceiling catalog: a default ceiling + optional per-tier ceilings.
#[derive(Debug, Clone)]
pub struct ProfileCatalog {
    default_ceiling: ManagedCeiling,
    tiers: BTreeMap<String, ManagedCeiling>,
}

impl ProfileCatalog {
    pub fn default_confined(ceiling_version: u64) -> Result<Self, ManagedPolicyError> {
        Ok(Self {
            default_ceiling: ManagedCeiling::confined(ceiling_version)?,
            tiers: BTreeMap::new(),
        })
    }

    #[must_use]
    pub fn with_tier_ceiling(mut self, tier: impl Into<String>, ceiling: ManagedCeiling) -> Self {
        self.tiers.insert(tier.into(), ceiling);
        self
    }

    #[must_use]
    pub fn default_ceiling(&self) -> &ManagedCeiling {
        &self.default_ceiling
    }

    #[must_use]
    pub fn resolve(&self, _app_id: &Uuid, tier: Option<&str>) -> &ManagedCeiling {
        tier.and_then(|tier| self.tiers.get(tier))
            .unwrap_or(&self.default_ceiling)
    }
}

/// One operator ceiling: an id + monotonic version + the parsed [`RootCharter`] and
/// its source TOML (kept so the ceiling-only effective policy can be composed through
/// the engine's own `effective_policy_from_charter_toml`).
#[derive(Debug, Clone)]
pub struct ManagedCeiling {
    pub id: String,
    pub tier: Option<String>,
    pub ceiling_version: u64,
    root: RootCharter,
    toml: String,
    bind_app_schema: bool,
}

impl ManagedCeiling {
    /// Parse a ceiling from a `RootCharter` TOML document.
    pub fn from_toml(
        id: impl Into<String>,
        tier: Option<String>,
        ceiling_version: u64,
        toml: &str,
    ) -> Result<Self, ManagedPolicyError> {
        let root = RootCharter::parse_toml(toml, &builtin_registry())
            .map_err(ManagedPolicyError::CeilingLoad)?;
        Ok(Self {
            id: id.into(),
            tier,
            ceiling_version,
            root,
            toml: toml.to_string(),
            bind_app_schema: false,
        })
    }

    /// The monorepo-owned confined default ceiling.
    pub fn confined(ceiling_version: u64) -> Result<Self, ManagedPolicyError> {
        let mut ceiling =
            Self::from_toml("confined-default", None, ceiling_version, CONFINED_CEILING_TOML)?;
        ceiling.bind_app_schema = true;
        Ok(ceiling)
    }

    /// The monorepo-owned platform (author-owned) ceiling.
    pub fn platform(ceiling_version: u64) -> Result<Self, ManagedPolicyError> {
        Self::from_toml("platform-default", None, ceiling_version, PLATFORM_CEILING_TOML)
    }

    #[must_use]
    pub fn root(&self) -> &RootCharter {
        &self.root
    }

    /// Compose the ceiling against a grant-only draft extracted from itself to obtain
    /// its effective policy (the no-creator-draft path). Injects/requires/validates
    /// survive from the root ceiling; grants become effective through the ceiling's own
    /// grant rules. Delegates to the engine's `effective_policy_from_charter_toml`.
    fn effective(&self) -> Result<PdpPolicy, ManagedPolicyError> {
        effective_policy_from_charter_toml(&self.toml).map_err(ManagedPolicyError::CeilingCompose)
    }

    fn effective_for_app_schema(&self, schema: &str) -> Result<PdpPolicy, ManagedPolicyError> {
        if !self.bind_app_schema {
            return self.effective();
        }
        let charter = bind_confined_charter_to_schema(&self.toml, schema)
            .map_err(ManagedPolicyError::CeilingCompose)?;
        effective_policy_from_charter_toml(&charter).map_err(ManagedPolicyError::CeilingCompose)
    }
}

/// Compose the fixed no-inject guard charter after binding its schema authority to
/// one exact app schema. Managed shape injection remains on the separate composed
/// ceiling/draft policy used by [`EffectivePolicy`] and `IrAuthor` lowering.
pub(crate) fn confined_guard_policy_for_schema(
    schema: &str,
) -> Result<PdpPolicy, ManagedPolicyError> {
    let charter = bind_confined_charter_to_schema(CONFINED_GUARD_CHARTER_TOML, schema)
        .map_err(ManagedPolicyError::CeilingCompose)?;
    effective_policy_from_charter_toml(&charter).map_err(ManagedPolicyError::CeilingCompose)
}

fn schema_scope_value(schema: &str) -> toml::Value {
    let mut scope = toml::map::Map::new();
    scope.insert(
        "include".to_string(),
        toml::Value::Array(vec![toml::Value::String(schema.to_string())]),
    );
    toml::Value::Table(scope)
}

/// Bind the reusable confined charter to one exact app schema. Validate the expected
/// template shape so a later policy edit fails closed instead of changing authority.
fn bind_confined_charter_to_schema(source: &str, schema: &str) -> Result<String, String> {
    let mut doc: toml::Value = toml::from_str(source)
        .map_err(|error| format!("parse confined charter for schema binding: {error}"))?;
    let grants = doc
        .as_table_mut()
        .and_then(|root| root.get_mut("grant"))
        .and_then(toml::Value::as_array_mut)
        .ok_or_else(|| "confined charter must contain [[grant]] rules".to_string())?;

    let mut create_table_rules = 0_u8;
    let mut rename_rules = 0_u8;
    for (index, value) in grants.iter_mut().enumerate() {
        let rule = value
            .as_table_mut()
            .ok_or_else(|| format!("confined charter grant {index} must be a table"))?;
        let key = rule
            .get("key")
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("confined charter grant {index} must have a string key"))?;
        match key.as_str() {
            KEY_SCHEMA_CREATE_TABLE | KEY_SCHEMA_RENAME => {
                if rule.get("value").and_then(toml::Value::as_bool) != Some(true)
                    || rule.get("scope").and_then(toml::Value::as_str) != Some("all")
                {
                    return Err(format!(
                        "confined charter {key} must remain true at scope=all before app binding"
                    ));
                }
                rule.insert("scope".to_string(), schema_scope_value(schema));
                if key == KEY_SCHEMA_CREATE_TABLE {
                    create_table_rules += 1;
                } else {
                    rename_rules += 1;
                }
            }
            KEY_SCHEMA_CROSS_SCHEMA => {
                return Err(
                    "confined charter must not carry schema.cross_schema before app binding"
                        .to_string(),
                );
            }
            // Any OTHER schema-scoped grant is refused rather than passed through.
            // This arm used to be `_ => {}`, which let a grant this function does not
            // know how to bind reach the composed charter still carrying its authored
            // `scope = "all"` - authority over every schema, from a function whose whole
            // job is confining authority to one. The registry already defines three such
            // keys beyond the three handled above (`schema.create_schema` PerSchema,
            // `schema.alter_injected` PerTable, `schema.partition` Global), so this is
            // not a guard against a hypothetical future key.
            //
            // Refusing means adding one to the charter is a compile-free but LOUD
            // failure: whoever adds it must come here and say how it binds. That is the
            // decision we want made deliberately, including for `schema.partition`,
            // whose Global object model may well make `scope = "all"` correct.
            other if other.starts_with("schema.") => {
                return Err(format!(
                    "confined charter grant {index} carries schema-scoped key {other}, which \
                     app binding does not know how to confine; bind it explicitly or remove it"
                ));
            }
            // Non-schema keys (`safety.*`, `runtime.*`, `code.*`) are not schema-scoped,
            // so `scope = "all"` is their intended shape and they pass through unchanged.
            _ => {}
        }
    }
    if create_table_rules != 1 || rename_rules != 1 {
        return Err(format!(
            "confined charter app binding requires one create-table and one rename grant; \
             found {create_table_rules} and {rename_rules}"
        ));
    }

    let mut cross_schema = toml::map::Map::new();
    cross_schema.insert(
        "key".to_string(),
        toml::Value::String(KEY_SCHEMA_CROSS_SCHEMA.to_string()),
    );
    cross_schema.insert("value".to_string(), toml::Value::Boolean(true));
    cross_schema.insert("scope".to_string(), schema_scope_value(schema));
    grants.push(toml::Value::Table(cross_schema));

    toml::to_string(&doc).map_err(|error| format!("serialize app-bound charter: {error}"))
}

#[derive(Debug, Clone, Copy)]
pub struct CreatorPolicyDraft<'a> {
    pub filename: &'a str,
    pub body: &'a str,
}

/// A parsed creator draft: the PDP [`PolicyDoc`] the engine composes. Approval rides
/// inside it as a normal `safety.require_approval` obligation — there are no managed-only
/// out-of-band directives.
#[derive(Debug, Clone)]
pub struct ParsedDraft {
    /// The grant/inject/require/validate rules, validated against the builtin registry.
    pub doc: PolicyDoc,
}

/// Parse a creator draft TOML body (filename already validated) into a [`ParsedDraft`].
/// Shared by the ingress (`ManagedPolicyConfig::parse_draft`) and the stored-policy
/// re-hydration (`AppPolicyRecord::parsed_policy_draft`). The whole body is a PDP
/// document — the engine `deny_unknown_fields` loader validates it directly.
pub fn parse_draft_body(body: &str, filename: &str) -> Result<ParsedDraft, ManagedPolicyError> {
    let doc = PolicyDoc::parse_toml(body, &builtin_registry(), LoadContext::NonRootLayer)
        .map_err(|source| ManagedPolicyError::MalformedDraft {
            filename: filename.to_string(),
            message: format!("{source:?}"),
        })?;
    Ok(ParsedDraft { doc })
}

/// A verifier for a [`SealedPolicy`]: the MAC key + the registry digest + ceiling
/// version the seal was minted under. `verify` recomposes the MAC and hard-fails on
/// any tamper or binding mismatch.
#[derive(Debug, Clone)]
pub struct SealVerifier {
    mac_key: Vec<u8>,
    registry_digest: [u8; 32],
    ceiling_version: u64,
}

impl SealVerifier {
    /// Verify a sealed policy against a freshly composed engine policy.
    pub fn verify(&self, sealed: &SealedPolicy, policy: &PdpPolicy) -> Result<(), SealError> {
        sealed.verify(
            &self.mac_key,
            policy,
            &self.registry_digest,
            SEAL_DIALECT,
            SEAL_MATCHER_VERSION,
            self.ceiling_version,
        )
    }
}

#[derive(Debug)]
pub struct SealedManagedPolicy {
    pub ceiling_id: String,
    pub ceiling_version: u64,
    /// The composed engine policy the seal covers (the shape/lowering source).
    pub effective: PdpPolicy,
    /// The managed posture derived from `effective`.
    pub managed: ManagedPosture,
    pub sealed: SealedPolicy,
    pub verifier: SealVerifier,
}

#[derive(Debug, thiserror::Error)]
pub enum ManagedPolicyError {
    #[error("migration policy seal key must be at least 32 bytes (got {actual})")]
    SealKeyTooShort { actual: usize },
    #[error("invalid policy draft filename {filename:?}: expected {MIGRATE_POLICY_FILENAME}")]
    InvalidDraftFilename { filename: String },
    #[error("parse {filename}: {message}")]
    MalformedDraft { filename: String, message: String },
    #[error("load operator ceiling: {0:?}")]
    CeilingLoad(LoadError),
    #[error("compose operator ceiling: {0}")]
    CeilingCompose(String),
    #[error("compose migration policy: {0:?}")]
    Compose(ComposeError),
}

impl ManagedPolicyError {
    #[must_use]
    pub fn is_creator_fault(&self) -> bool {
        match self {
            // A bad draft filename, a malformed draft, or a draft that escalates
            // beyond the operator ceiling are all the creator's fault.
            Self::InvalidDraftFilename { .. }
            | Self::MalformedDraft { .. }
            | Self::Compose(_) => true,
            // A malformed OPERATOR ceiling is an operator/infra fault, not the creator's.
            Self::SealKeyTooShort { .. } | Self::CeilingLoad(_) | Self::CeilingCompose(_) => {
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"migrated test seal key 32 bytes min";

    fn config() -> ManagedPolicyConfig {
        ManagedPolicyConfig::default_confined(KEY, 42).expect("test policy config")
    }

    /// A config whose "probe" tier carries an arbitrary ceiling TOML, so a test can
    /// compose against a charter shape our own `policies/*.toml` do not use. The
    /// ceiling is built with `from_toml`, so `bind_app_schema` is false and the
    /// template validation in `bind_confined_charter_to_schema` does not apply.
    fn config_with_probe_ceiling(toml: &str) -> ManagedPolicyConfig {
        let ceiling = ManagedCeiling::from_toml("probe", Some("probe".into()), 1, toml)
            .expect("probe ceiling parses");
        let catalog = ProfileCatalog::default_confined(42)
            .expect("test profile catalog")
            .with_tier_ceiling("probe", ceiling);
        ManagedPolicyConfig::new(KEY, catalog).expect("probe policy config")
    }

    /// WITHIN ONE LAYER, A `value = false` RULE DOES NOT CARVE ANYTHING OUT, and
    /// `exclude` does. Both arms are here on purpose: the second is the one-variable
    /// control that proves the first is measuring the carve-out and not something else.
    ///
    /// Reported by zero-migrate (ZERO-MIGRATE-2026-08-12-002) while answering a
    /// different question, and re-run here against OUR pin (`cb1bcb59`) and OUR
    /// compose path rather than taken on their report. A grant resolves to the JOIN of
    /// every covering rule in the same document, so `false` never lowers a `true`; only
    /// a LOWER LAYER can mask. `exclude` removes the region from the rule's scope, which
    /// is the mechanism that actually restricts inside one document.
    ///
    /// WHY THIS TEST EXISTS even though nothing is broken today: our three ceilings
    /// (`policies/{confined,confined-guard,platform}.policy.toml`) contain NO
    /// `value = false` grant - checked, all three, 2026-08-12 - so the trap is latent,
    /// not live. It is worth pinning because our charters are single-layer by
    /// construction, which makes a `false` rule the natural thing to reach for when
    /// someone wants to except one table, and it would land looking like a restriction
    /// while enforcing nothing.
    ///
    /// WHAT THIS TEST DOES NOT COVER: multi-layer charters, where `false` DOES mask.
    /// We author none, so that arm is unreachable here and is not asserted.
    #[test]
    fn within_one_layer_a_false_grant_does_not_carve_out_but_exclude_does() {
        let app_id = Uuid::new_v4();
        let schema = app_id.to_string();
        let draft_toml = format!(
            r#"policy_version = 1

[[grant]]
key = "schema.create_table"
value = true
scope = {{ include = ["{schema}.secret"] }}
"#
        );

        // ARM A: the carve-out written as a second rule at `value = false`.
        let false_carve_out = format!(
            r#"policy_version = 1

[[grant]]
key = "schema.create_table"
value = true
scope = {{ include = ["{schema}"] }}

[[grant]]
key = "schema.create_table"
value = false
scope = {{ include = ["{schema}.secret"] }}
"#
        );
        let cfg = config_with_probe_ceiling(&false_carve_out);
        let draft = cfg
            .parse_draft(&CreatorPolicyDraft {
                filename: MIGRATE_POLICY_FILENAME,
                body: &draft_toml,
            })
            .expect("draft parses");
        assert!(
            cfg.compose_effective_for_app(&app_id, Some("probe"), Some(&draft)).is_ok(),
            "a `value = false` rule in the SAME layer is expected to be inert; if this \
             now REFUSES, the engine gained within-layer masking and the warning in \
             this test (and any charter relying on it) needs rewriting"
        );

        // ARM B, one variable: the same carve-out expressed as `exclude`.
        let exclude_carve_out = format!(
            r#"policy_version = 1

[[grant]]
key = "schema.create_table"
value = true
scope = {{ include = ["{schema}"], exclude = ["{schema}.secret"] }}
"#
        );
        let cfg = config_with_probe_ceiling(&exclude_carve_out);
        let draft = cfg
            .parse_draft(&CreatorPolicyDraft {
                filename: MIGRATE_POLICY_FILENAME,
                body: &draft_toml,
            })
            .expect("draft parses");
        let err = cfg
            .compose_effective_for_app(&app_id, Some("probe"), Some(&draft))
            .expect_err("`exclude` DOES restrict, so the excluded table must be refused");
        assert!(matches!(err, ManagedPolicyError::Compose(_)));
    }

    #[test]
    fn managed_policy_config_requires_at_least_32_byte_mac_key() {
        let catalog = ProfileCatalog::default_confined(1).expect("test profile catalog");

        for key_len in [0, 1, 31] {
            assert!(
                ManagedPolicyConfig::new(vec![0; key_len], catalog.clone()).is_err(),
                "{key_len}-byte MAC key must be rejected"
            );
        }

        ManagedPolicyConfig::new(vec![0; 32], catalog)
            .expect("32-byte MAC key must be accepted");
    }

    #[test]
    fn no_draft_uses_default_confined_ceiling() {
        let cfg = config();
        let app_id = Uuid::new_v4();

        let effective = cfg
            .compose_effective_for_app(&app_id, None, None)
            .expect("no draft should resolve to default ceiling");

        assert_eq!(effective.ceiling_id, "confined-default");
        assert_eq!(effective.ceiling_version, 42);
        // The default confined ceiling grants destructive `allow` and injects the
        // system shape.
        assert_eq!(effective.managed.destructive_ops, DestructiveOps::Allow);
        // No `safety.require_approval` obligation on the default confined ceiling.
        assert_eq!(effective.approval_level(&app_id.to_string()), ApprovalLevel::Never);
        let app_schema = app_id.to_string();
        // `from_policy`, not the removed `confined_with_effective(schema, policy)`:
        // the schema is no longer passed alongside the policy because
        // `schema_scope()` derives it from the policy itself
        // (`owned_schemas_from_effective`). The assertion below is what proves the
        // derivation still yields this app's schema.
        let guard = zeroship_migrate::guard::GuardConfig::from_policy(
            effective.policy.clone(),
            zeroship_migrate_postgres::DIALECT,
        );
        assert_eq!(
            guard.schema_scope(),
            Some(zeroship_migrate::SchemaScope::Single(app_schema)),
            "the effective policy must retain the exact app-schema boundary"
        );
    }

    /// App binding must refuse a schema-scoped grant it cannot confine.
    ///
    /// `bind_confined_charter_to_schema` rewrites `schema.create_table` and
    /// `schema.rename` from `scope = "all"` to this app's schema, and appends a bound
    /// `schema.cross_schema`. Every other key fell to `_ => {}` and passed through
    /// untouched. For a non-schema key (`safety.*`, `runtime.*`) that is right. For a
    /// schema-scoped one it is the exact opposite of the function's purpose: the grant
    /// reaches the composed charter still saying `scope = "all"`.
    ///
    /// The registry defines three such keys today that this function does not handle,
    /// so the case below uses a real one (`schema.create_schema`, PerSchema) rather
    /// than an invented key - a made-up key would prove the arm fires without proving
    /// it fires on anything that can actually appear.
    #[test]
    fn app_binding_refuses_a_schema_key_it_cannot_confine() {
        let app_schema = Uuid::new_v4().to_string();
        let charter = format!(
            "{CONFINED_GUARD_CHARTER_TOML}\n\
             [[grant]]\n\
             key = \"{KEY_SCHEMA_CREATE_SCHEMA}\"\n\
             value = true\n\
             scope = \"all\"\n"
        );

        let err = bind_confined_charter_to_schema(&charter, &app_schema)
            .expect_err("an unbindable schema-scoped grant must fail closed");
        assert!(
            err.contains(KEY_SCHEMA_CREATE_SCHEMA),
            "the error must name the offending key, got: {err}"
        );

        // POSITIVE CONTROL. The assertion above is satisfied by a function that
        // rejects every charter, including the real one. The unmodified guard charter
        // must still bind.
        bind_confined_charter_to_schema(CONFINED_GUARD_CHARTER_TOML, &app_schema)
            .expect("the shipped guard charter must still bind");
    }

    #[test]
    fn confined_guard_preserves_schema_bound_grants_without_inject() {
        let app_schema = Uuid::new_v4().to_string();
        let guard_policy = confined_guard_policy_for_schema(&app_schema)
            .expect("fixed no-inject guard charter composes");
        let lower_charter = bind_confined_charter_to_schema(CONFINED_CEILING_TOML, &app_schema)
            .expect("fixed confined ceiling binds");
        let lower_policy = effective_policy_from_charter_toml(&lower_charter)
            .expect("fixed confined ceiling composes");

        let owned_schema = ObjectName::schema(app_schema.as_bytes().to_vec());
        let owned_table =
            ObjectName::table(app_schema.as_bytes().to_vec(), b"widgets".to_vec());
        let foreign_table = ObjectName::table(b"other_app".to_vec(), b"widgets".to_vec());
        let objects = [&owned_schema, &owned_table, &foreign_table];
        // `runtime.lock_timeout_ms` / `runtime.statement_timeout_ms` used to be in
        // this list. Both charters now grant NEITHER -- the engine registers every
        // `runtime.*` knob `DeclaredOnly` and refuses a document that raises one
        // above its default, which is what stopped `zeroship-migrate-server` from
        // starting at all (see the note in policies/confined.policy.toml). Keeping
        // them here would compare "not granted" against "not granted": a row that
        // passes because neither side has the knob, not because they agree on it.
        for key_name in [
            KEY_SCHEMA_CREATE_TABLE,
            KEY_SCHEMA_RENAME,
            KEY_SCHEMA_CROSS_SCHEMA,
            KEY_SAFETY_DESTRUCTIVE_OPS,
        ] {
            let knob = key(key_name);
            for object in objects {
                assert_eq!(
                    guard_policy.grants(&knob, object),
                    lower_policy.grants(&knob, object),
                    "guard grant {key_name} must match the confined ceiling at {object:?}"
                );
            }
        }

        assert!(
            !lower_policy.injects_for(&owned_table).is_empty(),
            "managed lower must retain the mandatory system-shape inject"
        );
        assert!(
            guard_policy.injects_for(&owned_table).is_empty(),
            "rendered-DDL guard must not carry inject coverage"
        );
        let guard = zeroship_migrate::guard::GuardConfig::from_policy(
            guard_policy,
            zeroship_migrate_postgres::DIALECT,
        );
        assert_eq!(
            guard.schema_scope(),
            Some(zeroship_migrate::SchemaScope::Single(app_schema)),
            "guard must remain confined to the exact app schema"
        );
    }

    #[test]
    fn require_approval_obligation_is_read_from_the_composed_policy() {
        let cfg = config();
        let app_id = Uuid::new_v4();
        // A creator draft that authors the sealed approval obligation as a normal
        // `[[require]]` — `always`, scoped to the whole DB.
        let draft_toml = r#"policy_version = 1

[[require]]
key = "safety.require_approval"
value = "always"
scope = "all"
"#;
        let draft = cfg
            .parse_draft(&CreatorPolicyDraft { filename: MIGRATE_POLICY_FILENAME, body: draft_toml })
            .expect("approval obligation draft parses");
        let effective = cfg
            .compose_effective_for_app(&app_id, None, Some(&draft))
            .expect("obligation draft composes (composes UP)");
        assert_eq!(
            effective.approval_level(&app_id.to_string()),
            ApprovalLevel::Always,
            "the composed policy must surface the sealed require_approval obligation"
        );
    }

    #[test]
    fn tighter_draft_composes_to_the_draft() {
        let cfg = config();
        let app_id = Uuid::new_v4();
        // A draft that only TIGHTENS: forbid destructive ops.
        //
        // This draft also carried `runtime.lock_timeout_ms = 1000` as a second,
        // "tighter timeout" tightening. It cannot: the engine registers every
        // `runtime.*` knob `DeclaredOnly` with default 1, and the load gate refuses
        // ANY value above the default -- in an operator ceiling OR a creator draft.
        // So 1000 is not a tightening the creator is allowed to author, it is a
        // document the loader rejects, and the only admissible value (1) is a no-op.
        // A creator cannot bound migration lock/statement timeouts at all today.
        // See policies/confined.policy.toml.
        let draft_toml = r#"policy_version = 1

[[grant]]
key = "safety.destructive_ops"
value = "forbid"
scope = "all"
"#;
        let draft = cfg
            .parse_draft(&CreatorPolicyDraft { filename: MIGRATE_POLICY_FILENAME, body: draft_toml })
            .expect("tightening draft parses");

        let effective = cfg
            .compose_effective_for_app(&app_id, None, Some(&draft))
            .expect("strict draft should compose");

        assert_eq!(effective.managed.destructive_ops, DestructiveOps::Forbid);
    }

    #[test]
    fn draft_permission_escalation_is_rejected_not_clamped() {
        let cfg = config();
        let app_id = Uuid::new_v4();
        // The confined ceiling does NOT grant `sql.raw`; a draft that does
        // escalates beyond the ceiling.
        let draft_toml = r#"policy_version = 1

[[grant]]
key = "sql.raw"
value = true
scope = "all"
"#;
        let draft = cfg
            .parse_draft(&CreatorPolicyDraft { filename: MIGRATE_POLICY_FILENAME, body: draft_toml })
            .expect("escalating draft still parses (rejected at compose)");

        let err = cfg
            .compose_effective_for_app(&app_id, None, Some(&draft))
            .expect_err("raw_sql exceeds the confined ceiling");

        assert!(matches!(err, ManagedPolicyError::Compose(_)));
        assert!(err.is_creator_fault());
    }

    #[test]
    fn draft_cannot_escape_the_app_schema_boundary() {
        let cfg = config();
        let app_id = Uuid::new_v4();
        let draft = cfg
            .parse_draft(&CreatorPolicyDraft {
                filename: MIGRATE_POLICY_FILENAME,
                body: r#"policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = "all"
"#,
            })
            .expect("cross-schema draft parses before admission");

        let err = cfg
            .compose_effective_for_app(&app_id, None, Some(&draft))
            .expect_err("authority outside the app schema must be rejected");
        assert!(matches!(err, ManagedPolicyError::Compose(_)));
        assert!(err.is_creator_fault());
    }

    /// A draft asking `scope = "all"` for a key the ceiling grants ONLY at one exact
    /// literal must refuse with `UncoveredRegionNotRepresentable`, not the generic
    /// escalation error.
    ///
    /// This is the residual I said in ZEROSHIP-2026-08-12-249 could not be tested until
    /// the pin moved, on the belief that the variant was new in zero-migrate's
    /// 8c254fa8. They corrected that (ZERO-MIGRATE-2026-08-12-003): the variant is
    /// ORIGINAL behaviour, constructed at `boundary.rs:174` and `:204` in our vendored
    /// pin, and what 8c254fa8 added was a SECOND cause for it. They explicitly had NOT
    /// run it at cb1bcb59 and asked me to, since I am already on that commit. This test
    /// is that run.
    ///
    /// WHY THIS INPUT AND NOT THE OTHER TWO ESCALATION TESTS ABOVE: those use `sql.raw`
    /// and `schema.cross_schema`, which the confined ceiling does not grant AT ALL, so
    /// there is no covering rule to subtract and they take a different arm.
    /// `schema.create_table` IS granted, but `bind_confined_charter_to_schema` rewrites
    /// its scope to one exact literal - so `All` minus that literal is the subtraction
    /// with no representation, which is the arm this variant guards.
    ///
    /// It asserts the VARIANT, not merely that composition failed, because the whole
    /// point of the exchange was that the two refusals carry different information.
    #[test]
    fn draft_all_scope_on_a_literal_bound_key_refuses_as_not_representable() {
        let cfg = config();
        let app_id = Uuid::new_v4();
        let draft = cfg
            .parse_draft(&CreatorPolicyDraft {
                filename: MIGRATE_POLICY_FILENAME,
                body: r#"policy_version = 1

[[grant]]
key = "schema.create_table"
value = true
scope = "all"
"#,
            })
            .expect("all-scoped draft parses before admission");

        let err = cfg
            .compose_effective_for_app(&app_id, None, Some(&draft))
            .expect_err("scope=all against a literal-bound rule must be refused");
        assert!(
            matches!(
                err,
                ManagedPolicyError::Compose(ComposeError::UncoveredRegionNotRepresentable { .. })
            ),
            "expected UncoveredRegionNotRepresentable, got: {err:?}"
        );
        assert!(err.is_creator_fault());
    }

    /// A draft whose scope is a GLOB whose literal prefix lands INSIDE the granted
    /// region. Every other escalation test here uses `scope = "all"`, which the
    /// admission check catches wherever it samples; this one is built so a sampled
    /// witness would land on the one object the ceiling does grant.
    ///
    /// The shape comes from zero-migrate's ZERO-MIGRATE-2026-08-12-001: their `admit`
    /// proved a draft within a charter by sampling ONE object per charter-partitioned
    /// region, and the sampled witness of a glob is built from its literal prefix. Our
    /// confined ceiling binds `schema.create_table` to the app schema EXACTLY
    /// (`bind_confined_charter_to_schema` rewrites the scope in place), so
    /// `<app_schema>*` has the app schema itself as its prefix witness while also
    /// covering `<app_schema>_evil`, which the ceiling does not grant.
    ///
    /// This test is a CANARY, not a reproduction: it passes at the vendored pin
    /// (`cb1bcb59`, which predates their fix). Its value is that it fails if a future
    /// pin move, or an edit to the confined charter that introduces a glob or a second
    /// layer, makes the prefix-witness hole reachable here.
    ///
    /// What it does NOT cover: the layered-charter escalation their message actually
    /// reproduces needs an upper layer that LOWERS a value over a sub-region, and our
    /// charter is single-layer by construction, so no test in this file can exercise
    /// that arm today.
    #[test]
    fn draft_glob_scope_anchored_on_the_granted_schema_is_still_rejected() {
        let cfg = config();
        let app_id = Uuid::new_v4();
        let app_schema = app_id.to_string();
        // The glob's literal prefix IS the granted schema; the glob also covers
        // sibling schemas the ceiling never granted.
        let draft_toml = format!(
            r#"policy_version = 1

[[grant]]
key = "schema.create_table"
value = true
scope = {{ include = ["{app_schema}*"] }}
"#
        );
        let draft = cfg
            .parse_draft(&CreatorPolicyDraft {
                filename: MIGRATE_POLICY_FILENAME,
                body: &draft_toml,
            })
            .expect("glob-scoped draft parses before admission");

        let err = cfg
            .compose_effective_for_app(&app_id, None, Some(&draft))
            .expect_err("a glob reaching beyond the bound app schema must be rejected");
        assert!(matches!(err, ManagedPolicyError::Compose(_)));
        assert!(err.is_creator_fault());
    }

    #[test]
    fn malformed_draft_is_rejected_fail_closed() {
        let cfg = config();
        let err = cfg
            .parse_draft(&CreatorPolicyDraft {
                filename: MIGRATE_POLICY_FILENAME,
                // an unknown field → deny_unknown_fields parse error.
                body: "policy_version = 1\n[[grant]]\nkez = \"sql.raw\"\nvalue = true\nscope = \"all\"\n",
            })
            .expect_err("unknown policy key must be a parse error");

        assert!(matches!(err, ManagedPolicyError::MalformedDraft { .. }));
        assert!(err.is_creator_fault());
    }

    #[test]
    fn sealed_effective_policy_round_trips() {
        let cfg = config();
        let app_id = Uuid::new_v4();

        let sealed = cfg
            .seal_for_app(&app_id, None, None)
            .expect("default confined policy should seal");

        // The seal verifies against the exact composed policy it covers.
        sealed
            .verifier
            .verify(&sealed.sealed, &sealed.effective)
            .expect("seal verifies");
        assert_eq!(sealed.ceiling_version, 42);

        // A tampered ceiling version fails the binding check.
        let mut wrong = sealed.verifier.clone();
        wrong.ceiling_version = 43;
        assert!(wrong.verify(&sealed.sealed, &sealed.effective).is_err());
    }

    #[test]
    fn platform_ceiling_is_author_owned_no_inject() {
        // The platform ceiling loads + composes and grants the privileged vendor set.
        let ceiling = ManagedCeiling::platform(1).expect("platform ceiling loads");
        let policy = ceiling.effective().expect("platform ceiling composes");
        let all = ObjectName::table(b"any".to_vec(), b"any".to_vec());
        // access.role is granted (privileged posture) — proves the platform grants loaded.
        assert!(matches!(
            policy.grants(&key(zeroship_migrate_ir::policy_registry::KEY_ACCESS_ROLE), &all),
            Some(KnobValue::Bool(true))
        ));
    }
}
