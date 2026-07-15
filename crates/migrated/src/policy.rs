//! The managed-policy server: the monorepo's operator-ceiling ⊓ creator-draft
//! composition + seal, on top of the published `zero-migrate` Policy Decision Point.
//!
//! # The model (Phase 3 of the policy redesign)
//!
//! The engine's old `PolicyProfile` value-struct + its `meet_ceiling_draft` compose
//! + its `seal_effective_profile` HMAC are DELETED. This module rebuilds the managed
//! server on the engine's surviving PDP:
//!
//! - the OPERATOR CEILING is a [`zero_migrate_policy::RootCeiling`] — a [`PolicyDoc`]
//!   loaded [`LoadContext::RootCeiling`] (the only layer that may carry a `mandatory`
//!   inject). The default ceiling is the monorepo-owned CONFINED document embedded
//!   below; named tiers add more ceilings to the [`ProfileCatalog`].
//! - the CREATOR DRAFT is an untrusted [`PolicyDoc`] loaded [`LoadContext::NonRootLayer`].
//! - the EFFECTIVE policy is [`compose_strict`]`(ceiling, draft)` — operator ⊓ creator
//!   with ESCALATION-REJECT (a draft grant looser than the ceiling permits is
//!   rejected, never clamped). This is the direct replacement for `meet_ceiling_draft`.
//! - the SEAL is the `zero-migrate-policy` HMAC over the composed [`EffectivePolicy`]
//!   ([`zero_migrate::seal`] / [`SealedPolicy::verify`]).
//!
//! The composed engine [`EffectivePolicy`] drives table-shape injection
//! (`resolve_create_table_policy`) and escalation-reject. The three MANAGED knobs a
//! confined app's guard still needs (`require_rls`, `destructive_ops`,
//! `extensions`) are read back out of the composed policy into a [`ManagedPosture`]
//! so the per-app `GuardConfig::confined(app_schema)` can be tightened the same way
//! the old `guard_config_for_profile` did — one source of truth, no drift.

use std::collections::BTreeMap;

use uuid::Uuid;
use zero_migrate::{effective_policy_from_ceiling_toml, seal, DestructiveOps, SealError, SealedPolicy};
use zero_migrate_ir::policy_registry::{
    builtin_registry, KEY_PG_EXTENSIONS, KEY_SEC_DESTRUCTIVE_OPS, KEY_SEC_REQUIRE_RLS,
};
use zero_migrate_policy::{
    compose_strict, ComposeError, EffectivePolicy as PdpPolicy, KnobKey, KnobValue, LoadContext,
    LoadError, ObjectName, PolicyDoc, RootCeiling,
};

/// The seal binding: the scope matcher folds Postgres identifiers under this dialect
/// and matcher-algorithm version. Bound into the HMAC so a seal minted under one
/// matcher semantics cannot be replayed under another.
const SEAL_DIALECT: &str = "postgres";
/// The scope-matcher algorithm version bound into the seal.
const SEAL_MATCHER_VERSION: u32 = 1;

/// The monorepo-owned CONFINED ceiling — the default operator ceiling a creator app
/// gets (the successor to the engine's deleted `PolicyProfile::confined()`).
pub const CONFINED_CEILING_TOML: &str = include_str!("../policies/confined.policy.toml");
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
    pub fn new(mac_key: impl Into<Vec<u8>>, catalog: ProfileCatalog) -> Result<Self, ManagedPolicyError> {
        // Prove the default ceiling loads + composes at construction time (fail fast
        // on a malformed embedded/operator ceiling rather than per-request).
        catalog.default_ceiling().effective()?;
        Ok(Self { catalog, mac_key: mac_key.into() })
    }

    pub fn default_confined(
        mac_key: impl Into<Vec<u8>>,
        ceiling_version: u64,
    ) -> Result<Self, ManagedPolicyError> {
        Self::new(mac_key, ProfileCatalog::default_confined(ceiling_version)?)
    }

    /// Parse an untrusted creator draft into a [`ParsedDraft`]: the non-root
    /// [`PolicyDoc`] (grant/inject/require/validate rules, composed by the engine PDP)
    /// plus the MANAGED-ONLY `require_approval` directive.
    ///
    /// `require_approval` has NO engine-knob equivalent — the PDP `sec.destructive_ops`
    /// enum is `forbid`/`warn`/`allow` only. It is a managed-server posture: "gate
    /// EVERY migration for operator approval, destructive or not". We strip it from the
    /// draft TOML before handing the remainder to the `deny_unknown_fields` PDP loader,
    /// then overlay it onto the composed policy's [`ManagedPosture`].
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
    /// (`compose_strict`), not clamped.
    pub fn compose_effective_for_app(
        &self,
        app_id: &Uuid,
        tier: Option<&str>,
        draft: Option<&ParsedDraft>,
    ) -> Result<EffectivePolicy, ManagedPolicyError> {
        let ceiling = self.catalog.resolve(app_id, tier);
        let (policy, require_approval) = match draft {
            Some(draft) => (
                compose_strict(ceiling.root(), &draft.doc, &builtin_registry())
                    .map_err(ManagedPolicyError::Compose)?,
                draft.require_approval,
            ),
            None => (ceiling.effective()?, false),
        };
        Ok(EffectivePolicy::new(
            ceiling.id.clone(),
            ceiling.ceiling_version,
            policy,
            require_approval,
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
/// source for injection + escalation-reject) plus the ceiling identity and the
/// derived managed posture the per-app guard is tightened with.
#[derive(Debug, Clone)]
pub struct EffectivePolicy {
    pub ceiling_id: String,
    pub ceiling_version: u64,
    /// The composed, unforgeable engine PDP policy — the source for
    /// `resolve_create_table_policy` and every guard decision.
    pub policy: PdpPolicy,
    /// The three managed knobs (`require_rls`, `destructive_ops`, `extensions`) read
    /// back out of `policy`, used to tighten the per-app `GuardConfig::confined`.
    pub managed: ManagedPosture,
}

impl EffectivePolicy {
    fn new(
        ceiling_id: String,
        ceiling_version: u64,
        policy: PdpPolicy,
        require_approval: bool,
    ) -> Self {
        let mut managed = ManagedPosture::from_policy(&policy);
        // The managed-only `require_approval` directive overlays the composed
        // destructive posture: it forces the tightest managed value `RequireApproval`,
        // which gates EVERY migration (the engine PDP has no such grant).
        if require_approval {
            managed.destructive_ops = DestructiveOps::RequireApproval;
        }
        Self { ceiling_id, ceiling_version, policy, managed }
    }

    #[must_use]
    pub fn requires_operator_approval(&self) -> bool {
        self.managed.destructive_ops == DestructiveOps::RequireApproval
    }

    /// Project for an approved apply: `RequireApproval` → `Allow` (the operator has
    /// approved, so destructive ops proceed).
    #[must_use]
    pub fn project_for_approved_apply(&self) -> Self {
        let mut projected = self.clone();
        if projected.managed.destructive_ops == DestructiveOps::RequireApproval {
            projected.managed.destructive_ops = DestructiveOps::Allow;
        }
        projected
    }

    /// Project for preflight: anything other than `Forbid` → `Warn` (preflight surfaces
    /// destructive ops as gated, without blocking the classification pass).
    #[must_use]
    pub fn project_for_preflight(&self) -> Self {
        let mut projected = self.clone();
        if projected.managed.destructive_ops != DestructiveOps::Forbid {
            projected.managed.destructive_ops = DestructiveOps::Warn;
        }
        projected
    }
}

/// The managed knobs the confined per-app guard is tightened with. Read out of the
/// composed engine policy so there is a single source of truth (except
/// `destructive_ops`, which carries the managed-only `RequireApproval` overlay the
/// engine PDP cannot express — see [`ManagedPosture::from_policy`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedPosture {
    pub require_rls: bool,
    pub destructive_ops: DestructiveOps,
    pub extensions: Vec<String>,
}

impl ManagedPosture {
    /// Derive the managed posture from the composed engine policy's decision queries.
    ///
    /// `destructive_ops` maps the engine's `sec.destructive_ops` OrderedEnum
    /// (`forbid`/`warn`/`allow`) back onto [`DestructiveOps`]. The managed-only
    /// `RequireApproval` value has NO engine-knob equivalent (the PDP enum omits it),
    /// so a composed policy can only surface `forbid`/`warn`/`allow`; a ceiling that
    /// wants approval-gating expresses it as a tier concern, not a composed grant.
    fn from_policy(policy: &PdpPolicy) -> Self {
        let all = ObjectName::table(b"any".to_vec(), b"any".to_vec());
        let destructive_ops = match policy
            .grants(&key(KEY_SEC_DESTRUCTIVE_OPS), &all)
        {
            Some(KnobValue::Str(v)) if v == "allow" => DestructiveOps::Allow,
            Some(KnobValue::Str(v)) if v == "warn" => DestructiveOps::Warn,
            // `forbid` (the tightest) or any unexpected shape → fail closed to Forbid.
            _ => DestructiveOps::Forbid,
        };
        let require_rls = policy
            .obligations(&all)
            .into_iter()
            .any(|(k, v)| k.as_str() == KEY_SEC_REQUIRE_RLS && matches!(v, KnobValue::Bool(true)));
        let extensions = match policy.grants(&key(KEY_PG_EXTENSIONS), &schema_object("public")) {
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

/// One operator ceiling: an id + monotonic version + the parsed [`RootCeiling`] and
/// its source TOML (kept so the ceiling-only effective policy can be composed through
/// the engine's own `effective_policy_from_ceiling_toml`).
#[derive(Debug, Clone)]
pub struct ManagedCeiling {
    pub id: String,
    pub tier: Option<String>,
    pub ceiling_version: u64,
    root: RootCeiling,
    toml: String,
}

impl ManagedCeiling {
    /// Parse a ceiling from a `RootCeiling` TOML document.
    pub fn from_toml(
        id: impl Into<String>,
        tier: Option<String>,
        ceiling_version: u64,
        toml: &str,
    ) -> Result<Self, ManagedPolicyError> {
        let root = RootCeiling::parse_toml(toml, &builtin_registry())
            .map_err(ManagedPolicyError::CeilingLoad)?;
        Ok(Self { id: id.into(), tier, ceiling_version, root, toml: toml.to_string() })
    }

    /// The monorepo-owned confined default ceiling.
    pub fn confined(ceiling_version: u64) -> Result<Self, ManagedPolicyError> {
        Self::from_toml("confined-default", None, ceiling_version, CONFINED_CEILING_TOML)
    }

    /// The monorepo-owned platform (author-owned) ceiling.
    pub fn platform(ceiling_version: u64) -> Result<Self, ManagedPolicyError> {
        Self::from_toml("platform-default", None, ceiling_version, PLATFORM_CEILING_TOML)
    }

    #[must_use]
    pub fn root(&self) -> &RootCeiling {
        &self.root
    }

    /// Compose the ceiling against a grant-only draft extracted from itself to obtain
    /// its effective policy (the no-creator-draft path). Injects/requires/validates
    /// survive from the root ceiling; grants become effective through the ceiling's own
    /// grant rules. Delegates to the engine's `effective_policy_from_ceiling_toml`.
    fn effective(&self) -> Result<PdpPolicy, ManagedPolicyError> {
        effective_policy_from_ceiling_toml(&self.toml).map_err(ManagedPolicyError::CeilingCompose)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CreatorPolicyDraft<'a> {
    pub filename: &'a str,
    pub body: &'a str,
}

/// A parsed creator draft: the PDP [`PolicyDoc`] (composed by the engine) plus the
/// MANAGED-ONLY directives the engine PDP cannot express.
#[derive(Debug, Clone)]
pub struct ParsedDraft {
    /// The grant/inject/require/validate rules, validated against the builtin registry.
    pub doc: PolicyDoc,
    /// The managed `require_approval` directive: gate EVERY migration for operator
    /// approval (not just destructive ops). No engine-knob equivalent.
    pub require_approval: bool,
}

/// Parse a creator draft TOML body (filename already validated) into a [`ParsedDraft`].
/// Shared by the ingress (`ManagedPolicyConfig::parse_draft`) and the stored-policy
/// re-hydration (`AppPolicyRecord::parsed_policy_draft`) so both split the managed
/// directives + validate the PDP body identically.
pub fn parse_draft_body(body: &str, filename: &str) -> Result<ParsedDraft, ManagedPolicyError> {
    let (stripped, require_approval) = split_managed_directives(body, filename)?;
    let doc = PolicyDoc::parse_toml(&stripped, &builtin_registry(), LoadContext::NonRootLayer)
        .map_err(|source| ManagedPolicyError::MalformedDraft {
            filename: filename.to_string(),
            message: format!("{source:?}"),
        })?;
    Ok(ParsedDraft { doc, require_approval })
}

/// Split a creator draft TOML into (grant-only PDP body, `require_approval`). The
/// managed server owns a small set of TOP-LEVEL managed directives that the PDP loader
/// (which is `deny_unknown_fields`) would otherwise reject; we strip them here and
/// leave the grant/inject/require/validate sections for `PolicyDoc::parse_toml`.
///
/// Today the only managed directive is `require_approval = <bool>`.
fn split_managed_directives(
    body: &str,
    filename: &str,
) -> Result<(String, bool), ManagedPolicyError> {
    let mut value: toml::Value = toml::from_str(body).map_err(|err| {
        ManagedPolicyError::MalformedDraft {
            filename: filename.to_string(),
            message: format!("parse draft toml: {err}"),
        }
    })?;
    let mut require_approval = false;
    if let Some(table) = value.as_table_mut() {
        if let Some(directive) = table.remove("require_approval") {
            require_approval = directive.as_bool().ok_or_else(|| {
                ManagedPolicyError::MalformedDraft {
                    filename: filename.to_string(),
                    message: "require_approval must be a boolean".to_string(),
                }
            })?;
        }
    }
    let stripped = toml::to_string(&value).map_err(|err| ManagedPolicyError::MalformedDraft {
        filename: filename.to_string(),
        message: format!("re-serialize draft: {err}"),
    })?;
    Ok((stripped, require_approval))
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
    /// The composed engine policy the seal covers (the apply/guard source).
    pub effective: PdpPolicy,
    /// The managed posture derived from `effective`.
    pub managed: ManagedPosture,
    pub sealed: SealedPolicy,
    pub verifier: SealVerifier,
}

#[derive(Debug, thiserror::Error)]
pub enum ManagedPolicyError {
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
            Self::CeilingLoad(_) | Self::CeilingCompose(_) => false,
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
        assert!(!effective.requires_operator_approval());
    }

    #[test]
    fn tighter_draft_composes_to_the_draft() {
        let cfg = config();
        let app_id = Uuid::new_v4();
        // A draft that only TIGHTENS: forbid destructive ops + a tighter lock timeout.
        let draft_toml = r#"policy_version = 1

[[grant]]
key = "sec.destructive_ops"
value = "forbid"
scope = "all"

[[grant]]
key = "op.lock_timeout_ms"
value = 1000
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
        // The confined ceiling does NOT grant `core.raw_sql`; a draft that does
        // escalates beyond the ceiling.
        let draft_toml = r#"policy_version = 1

[[grant]]
key = "core.raw_sql"
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
    fn malformed_draft_is_rejected_fail_closed() {
        let cfg = config();
        let err = cfg
            .parse_draft(&CreatorPolicyDraft {
                filename: MIGRATE_POLICY_FILENAME,
                // an unknown field → deny_unknown_fields parse error.
                body: "policy_version = 1\n[[grant]]\nkez = \"core.raw_sql\"\nvalue = true\nscope = \"all\"\n",
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
        // pg.role is granted (privileged posture) — proves the platform grants loaded.
        assert!(matches!(
            policy.grants(&key(zero_migrate_ir::policy_registry::KEY_PG_ROLE), &all),
            Some(KnobValue::Bool(true))
        ));
    }
}
