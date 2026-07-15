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
//! - the EFFECTIVE policy is [`admit`]`(ceiling, draft)` — operator ⊓ creator
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
use zero_migrate_ir::policy_approval::{require_approval_level, ApprovalLevel};
use zero_migrate_ir::policy_registry::{
    builtin_registry, KEY_CODE_EXTENSION, KEY_SAFETY_DESTRUCTIVE_OPS, KEY_SAFETY_REQUIRE_RLS,
};
use zero_migrate_policy::{
    admit, ComposeError, EffectivePolicy as PdpPolicy, KnobKey, KnobValue, LoadContext,
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
        let policy = match draft {
            // The `ceiling` operand is a finalized ceiling: `RootCeiling` itself is a
            // valid finalized single-layer ceiling (it implements `AdmitCeiling`), so
            // it may be `admit`'s ceiling directly.
            Some(draft) => admit(ceiling.root(), &draft.doc, &builtin_registry())
                .map_err(ManagedPolicyError::Compose)?,
            None => ceiling.effective()?,
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
/// composed engine policy so there is a single source of truth. Approval is NOT one of
/// these — it is the separate sealed `safety.require_approval` obligation the host
/// enforces (see [`EffectivePolicy::approval_level`]), never a `destructive_ops` state.
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
        // No `safety.require_approval` obligation on the default confined ceiling.
        assert_eq!(effective.approval_level(&app_id.to_string()), ApprovalLevel::Never);
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
        // A draft that only TIGHTENS: forbid destructive ops + a tighter lock timeout.
        let draft_toml = r#"policy_version = 1

[[grant]]
key = "safety.destructive_ops"
value = "forbid"
scope = "all"

[[grant]]
key = "runtime.lock_timeout_ms"
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
            policy.grants(&key(zero_migrate_ir::policy_registry::KEY_ACCESS_ROLE), &all),
            Some(KnobValue::Bool(true))
        ));
    }
}
