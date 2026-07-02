use std::collections::BTreeMap;

use uuid::Uuid;
use zeroship_migrate::{
    seal_effective_profile, DestructiveOps, PolicyProfile, SealError, SealVerifier, SealedProfile,
};

pub const MIGRATE_POLICY_FILENAME: &str = "migrate-policy.toml";

#[derive(Debug, Clone)]
pub struct ManagedPolicyConfig {
    catalog: ManagedProfileCatalog,
    mac_key: Vec<u8>,
}

impl ManagedPolicyConfig {
    pub fn new(
        mac_key: impl Into<Vec<u8>>,
        catalog: ManagedProfileCatalog,
    ) -> Result<Self, SealError> {
        let mac_key = mac_key.into();
        SealVerifier::new(&mac_key, catalog.default_ceiling().ceiling_version)?;
        Ok(Self { catalog, mac_key })
    }

    pub fn default_confined(
        mac_key: impl Into<Vec<u8>>,
        ceiling_version: u64,
    ) -> Result<Self, SealError> {
        Self::new(mac_key, ManagedProfileCatalog::default_confined(ceiling_version))
    }

    pub fn parse_draft(
        &self,
        draft: &CreatorPolicyDraft<'_>,
    ) -> Result<PolicyProfile, ManagedPolicyError> {
        if draft.filename != MIGRATE_POLICY_FILENAME {
            return Err(ManagedPolicyError::InvalidDraftFilename {
                filename: draft.filename.to_string(),
            });
        }
        PolicyProfile::from_toml(draft.body).map_err(|source| {
            ManagedPolicyError::MalformedDraft {
                filename: draft.filename.to_string(),
                message: source.to_string(),
            }
        })
    }

    pub fn compose_effective_for_app(
        &self,
        app_id: &Uuid,
        tier: Option<&str>,
        draft: Option<&PolicyProfile>,
    ) -> Result<EffectivePolicy, ManagedPolicyError> {
        let ceiling = self.catalog.resolve(app_id, tier);
        let effective = match draft {
            Some(draft) => PolicyProfile::meet_ceiling_draft(&ceiling.profile, draft)
                .map_err(ManagedPolicyError::Compose)?,
            None => ceiling.profile.clone(),
        };
        Ok(EffectivePolicy {
            ceiling_id: ceiling.id.clone(),
            ceiling_version: ceiling.ceiling_version,
            profile: effective,
        })
    }

    pub fn current_ceiling_for_app(&self, app_id: &Uuid, tier: Option<&str>) -> EffectivePolicy {
        let ceiling = self.catalog.resolve(app_id, tier);
        EffectivePolicy {
            ceiling_id: ceiling.id.clone(),
            ceiling_version: ceiling.ceiling_version,
            profile: ceiling.profile.clone(),
        }
    }

    pub fn seal_for_app(
        &self,
        app_id: &Uuid,
        tier: Option<&str>,
        draft: Option<CreatorPolicyDraft<'_>>,
    ) -> Result<SealedManagedPolicy, ManagedPolicyError> {
        let parsed_draft = draft
            .as_ref()
            .map(|draft| self.parse_draft(draft))
            .transpose()?;
        let effective = self.compose_effective_for_app(app_id, tier, parsed_draft.as_ref())?;
        self.seal_effective_for_app(app_id, effective)
    }

    pub fn seal_stored_for_app(
        &self,
        app_id: &Uuid,
        tier: Option<&str>,
        parsed_draft: &PolicyProfile,
        stored_effective: &PolicyProfile,
        pinned_ceiling_version: u64,
    ) -> Result<SealedManagedPolicy, ManagedPolicyError> {
        let ceiling = self.catalog.resolve(app_id, tier);
        let recomposed = PolicyProfile::meet_ceiling_draft(&ceiling.profile, parsed_draft)
            .map_err(ManagedPolicyError::Compose)?;
        let current_version = ceiling.ceiling_version;
        let effective_profile = if pinned_ceiling_version == current_version
            && stored_effective == &recomposed
        {
            stored_effective.clone()
        } else {
            recomposed
        };
        self.seal_effective_for_app(
            app_id,
            EffectivePolicy {
                ceiling_id: ceiling.id.clone(),
                ceiling_version: current_version,
                profile: effective_profile,
            },
        )
    }

    pub(crate) fn seal_effective_for_app(
        &self,
        app_id: &Uuid,
        effective: EffectivePolicy,
    ) -> Result<SealedManagedPolicy, ManagedPolicyError> {
        let project_schema = app_id.to_string();
        let sealed = seal_effective_profile(
            effective.profile.clone(),
            &project_schema,
            &self.mac_key,
            effective.ceiling_version,
        )
        .map_err(ManagedPolicyError::SealMint)?;
        let verifier = SealVerifier::new(&self.mac_key, effective.ceiling_version)
            .map_err(ManagedPolicyError::SealVerifier)?;
        Ok(SealedManagedPolicy {
            ceiling_id: effective.ceiling_id,
            ceiling_version: effective.ceiling_version,
            effective_profile: effective.profile,
            sealed,
            verifier,
        })
    }
}

impl EffectivePolicy {
    #[must_use]
    pub fn requires_operator_approval(&self) -> bool {
        self.profile.data_security.destructive_ops == DestructiveOps::RequireApproval
    }

    #[must_use]
    pub fn project_for_approved_apply(&self) -> Self {
        let mut projected = self.clone();
        if projected.profile.data_security.destructive_ops == DestructiveOps::RequireApproval {
            projected.profile.data_security.destructive_ops = DestructiveOps::Allow;
        }
        projected
    }

    #[must_use]
    pub fn project_for_preflight(&self) -> Self {
        let mut projected = self.clone();
        if projected.profile.data_security.destructive_ops != DestructiveOps::Forbid {
            projected.profile.data_security.destructive_ops = DestructiveOps::Warn;
        }
        projected
    }
}

#[derive(Debug, Clone)]
pub struct ManagedProfileCatalog {
    default_ceiling: ManagedCeiling,
    tiers: BTreeMap<String, ManagedCeiling>,
}

impl ManagedProfileCatalog {
    pub fn default_confined(ceiling_version: u64) -> Self {
        Self {
            default_ceiling: ManagedCeiling {
                id: "confined-default".to_string(),
                tier: None,
                ceiling_version,
                profile: PolicyProfile::confined(),
            },
            tiers: BTreeMap::new(),
        }
    }

    pub fn with_tier_ceiling(mut self, tier: impl Into<String>, ceiling: ManagedCeiling) -> Self {
        self.tiers.insert(tier.into(), ceiling);
        self
    }

    pub fn default_ceiling(&self) -> &ManagedCeiling {
        &self.default_ceiling
    }

    pub fn resolve(&self, _app_id: &Uuid, tier: Option<&str>) -> &ManagedCeiling {
        tier.and_then(|tier| self.tiers.get(tier))
            .unwrap_or(&self.default_ceiling)
    }
}

#[derive(Debug, Clone)]
pub struct ManagedCeiling {
    pub id: String,
    pub tier: Option<String>,
    pub ceiling_version: u64,
    pub profile: PolicyProfile,
}

#[derive(Debug, Clone, Copy)]
pub struct CreatorPolicyDraft<'a> {
    pub filename: &'a str,
    pub body: &'a str,
}

#[derive(Debug, Clone)]
pub struct EffectivePolicy {
    pub ceiling_id: String,
    pub ceiling_version: u64,
    pub profile: PolicyProfile,
}

#[derive(Debug)]
pub struct SealedManagedPolicy {
    pub ceiling_id: String,
    pub ceiling_version: u64,
    pub effective_profile: PolicyProfile,
    pub sealed: SealedProfile,
    pub verifier: SealVerifier,
}

#[derive(Debug, thiserror::Error)]
pub enum ManagedPolicyError {
    #[error("invalid policy draft filename {filename:?}: expected {MIGRATE_POLICY_FILENAME}")]
    InvalidDraftFilename { filename: String },
    #[error("parse {filename}: {message}")]
    MalformedDraft { filename: String, message: String },
    #[error("compose migration policy: {0}")]
    Compose(SealError),
    #[error("seal migration policy: {0}")]
    SealMint(SealError),
    #[error("build sealed-profile verifier: {0}")]
    SealVerifier(SealError),
}

impl ManagedPolicyError {
    pub fn is_creator_fault(&self) -> bool {
        match self {
            Self::InvalidDraftFilename { .. } | Self::MalformedDraft { .. } => true,
            Self::Compose(err) | Self::SealMint(err) => is_creator_seal_error(err),
            Self::SealVerifier(_) => false,
        }
    }
}

fn is_creator_seal_error(err: &SealError) -> bool {
    matches!(
        err,
        SealError::InvalidTimeout { .. }
            | SealError::PolicyExceedsCeiling { .. }
            | SealError::UnsupportedProfileKnob { .. }
    )
}

#[cfg(test)]
mod tests {
    use zeroship_migrate::{DestructiveOps, SealedPosture};

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
        assert_eq!(effective.profile, PolicyProfile::confined());
    }

    #[test]
    fn tighter_draft_composes_to_the_draft() {
        let cfg = config();
        let app_id = Uuid::new_v4();
        let mut draft = PolicyProfile::confined();
        draft.operational.lock_timeout_ms = 1_000;
        draft.data_security.destructive_ops = DestructiveOps::Forbid;

        let effective = cfg
            .compose_effective_for_app(&app_id, None, Some(&draft))
            .expect("strict draft should compose");

        assert_eq!(effective.profile, draft);
    }

    #[test]
    fn draft_permission_escalation_is_rejected_not_clamped() {
        let cfg = config();
        let app_id = Uuid::new_v4();
        let mut draft = PolicyProfile::confined();
        draft.capabilities.raw_sql = true;

        let err = cfg
            .compose_effective_for_app(&app_id, None, Some(&draft))
            .expect_err("raw_sql exceeds the confined ceiling");

        assert!(matches!(
            err,
            ManagedPolicyError::Compose(SealError::PolicyExceedsCeiling {
                knob: "capabilities.raw_sql"
            })
        ));
        assert!(err.is_creator_fault());
    }

    #[test]
    fn malformed_draft_is_rejected_fail_closed() {
        let cfg = config();
        let err = cfg
            .parse_draft(&CreatorPolicyDraft {
                filename: MIGRATE_POLICY_FILENAME,
                body: "[capabilities]\nraw_sq = true\n",
            })
            .expect_err("unknown policy key must be a parse error");

        assert!(matches!(err, ManagedPolicyError::MalformedDraft { .. }));
        assert!(err.is_creator_fault());
    }

    #[test]
    fn sealed_effective_profile_has_no_trusted_posture() {
        let cfg = config();
        let app_id = Uuid::new_v4();

        let sealed = cfg
            .seal_for_app(&app_id, None, None)
            .expect("default confined profile should seal");

        sealed.sealed.verify(&sealed.verifier).expect("seal verifies");
        assert_eq!(sealed.ceiling_version, 42);
        match sealed.sealed.posture() {
            SealedPosture::Confined | SealedPosture::Platform => {}
        }
    }
}
