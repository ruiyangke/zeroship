//! The lifecycle classifier, and the opaque identity it compares.
//!
//! SC-1: every authority observation runs through **one total classifier**
//! before any data SQL - the read `Preparing` waits on, the read each operation
//! takes before its own data SQL, and any unsolicited lifecycle observation a
//! publisher submits. It returns exactly one of three verdicts.
//!
//! ## The identity is opaque on purpose
//!
//! [`AuthorityIdentity`] carries an opaque key and an incarnation, and the
//! classifier only ever compares it for equality. Today a binding is per app,
//! so callers build one from an app id; the operator is considering decoupling
//! apps from databases (one app to many databases, many apps to one database),
//! which would re-key this onto the database or the grant. **The state machine
//! is axis-independent and must stay that way** - nothing below inspects the
//! inside of the key, so that move is a change at the construction sites and
//! nowhere else.

use std::fmt;

/// The opaque subject a binding's authority is about.
///
/// Equality is the only operation. Construct it from whatever the deployment's
/// identity axis is; [`AuthorityIdentity::for_app`] is the constructor for
/// today's per-app axis and is named for the axis rather than being the only
/// one possible.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AuthorityIdentity {
    /// Opaque. Never parsed, never split, never compared except for equality.
    key: Box<str>,
    /// Bumped when the subject is re-provisioned. Part of the identity rather
    /// than beside it: a handle carrying the old value names a subject that no
    /// longer exists, which is a different subject, not the same one in a
    /// different state.
    incarnation: u64,
}

impl AuthorityIdentity {
    /// The identity of one app incarnation - today's axis.
    #[must_use]
    pub fn for_app(app_id: impl Into<Box<str>>, incarnation: u64) -> Self {
        Self {
            key: app_id.into(),
            incarnation,
        }
    }

    /// The opaque key, for diagnostics only. Never branch on its contents.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// The incarnation, for diagnostics only.
    #[must_use]
    pub const fn incarnation(&self) -> u64 {
        self.incarnation
    }
}

impl fmt::Display for AuthorityIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{}", self.key, self.incarnation)
    }
}

/// The cluster and recovery timeline answering.
///
/// On PostgreSQL this is `(system_identifier, timeline_id)`. SC-2 records that
/// a local SQLite file has neither, so the dev tier has no domain to compare
/// and no PITR-resurrection defence; a binding on that tier carries the same
/// domain value it captured and the comparison below is trivially satisfied.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AuthorityDomain {
    system_identifier: u64,
    timeline_id: u32,
}

impl AuthorityDomain {
    #[must_use]
    pub const fn new(system_identifier: u64, timeline_id: u32) -> Self {
        Self {
            system_identifier,
            timeline_id,
        }
    }
}

impl fmt::Display for AuthorityDomain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.system_identifier, self.timeline_id)
    }
}

/// The schema epoch an observation carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SchemaEpoch(u64);

impl SchemaEpoch {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// The subject's lifecycle state, as the authority record reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleState {
    /// Settled and serving. The only state that can reach `Current`.
    Stable,
    /// Mid-transition - provisioning, deprovisioning, migrating. Retryable:
    /// the caller re-resolves rather than following the change in place.
    Changing,
    /// A permanent tombstone. There is nothing to re-resolve to, now or later.
    Deprovisioned,
}

/// A mask ceiling, folded by [`MaskCeiling::meet`].
///
/// Modelled as the set of unmask actor kinds the ceiling permits, because that
/// makes `meet` a real greatest-lower-bound (intersection) rather than a
/// stand-in. SC-1 invariant 8 only needs the algebra: a raise is ignored, a
/// lower value tightens.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MaskCeiling {
    allowed: std::collections::BTreeSet<Box<str>>,
}

impl MaskCeiling {
    /// A ceiling permitting exactly `kinds`.
    pub fn of<I, S>(kinds: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<Box<str>>,
    {
        Self {
            allowed: kinds.into_iter().map(Into::into).collect(),
        }
    }

    /// The greatest lower bound of two ceilings.
    ///
    /// Folding a newly read ceiling into the effective one with this is what
    /// makes invariant 8 hold by construction: the result is a subset of both,
    /// so a mid-transaction raise cannot broaden anything.
    #[must_use]
    pub fn meet(&self, other: &Self) -> Self {
        Self {
            allowed: self.allowed.intersection(&other.allowed).cloned().collect(),
        }
    }

    /// Is every kind this ceiling permits also permitted by `other`?
    #[must_use]
    pub fn is_no_broader_than(&self, other: &Self) -> bool {
        self.allowed.is_subset(&other.allowed)
    }

    /// Does the ceiling permit `kind`?
    #[must_use]
    pub fn permits(&self, kind: &str) -> bool {
        self.allowed.contains(kind)
    }
}

/// What the binding captured, and what every observation is compared against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedAuthority {
    pub identity: AuthorityIdentity,
    pub domain: AuthorityDomain,
    pub epoch: SchemaEpoch,
}

/// One authority observation, from any of the three sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedAuthority {
    pub identity: AuthorityIdentity,
    pub domain: AuthorityDomain,
    pub epoch: SchemaEpoch,
    pub lifecycle: LifecycleState,
    pub ceiling: MaskCeiling,
}

// `DenyReason` moved to `crate::error` on 2026-08-31. It is domain vocabulary -
// the reason a session was refused, creator-visible and non-retryable - not a
// reducer implementation detail. Keeping it here made `error.rs` (core) import
// from `transaction::reducer` (engine), i.e. the contract crate would have
// depended on the engine that depends on it. Moving the TYPE down resolves the
// cycle without moving any logic.
pub use zeroship_data_core::error::DenyReason;

/// The classifier's verdict. Exactly one of three, and the classifier is
/// total: every observation produces one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Identity matches, the lifecycle is stable, and the epoch is the
    /// expected one. **Not forcing**: it never claims the gate. Its ceiling is
    /// folded into the effective ceiling by `meet`, so it can only tighten.
    Current { ceiling: MaskCeiling },
    /// Identity matches, but the lifecycle state is *changing*, or the epoch
    /// differs. **Retryable.** The attempt is rolled back and the caller
    /// re-resolves to a fresh key; the entry never follows the new epoch in
    /// place.
    ReResolve,
    /// **Terminal.** No `BEGIN`, no data SQL, no following the new subject.
    /// The caller receives the *specific* denial, never a collapsed one.
    Deny(DenyReason),
}

impl Verdict {
    /// `ReResolve` and `Deny` are forcing publishers; `Current` is not.
    #[must_use]
    pub const fn is_forcing(&self) -> bool {
        !matches!(self, Self::Current { .. })
    }
}

/// Classify one observation against what the binding captured.
///
/// **The order is load-bearing: identity is compared before lifecycle state.**
/// An observation that names a *different* subject is denied for the identity
/// mismatch rather than for whatever that other subject's lifecycle happens to
/// be. That is what makes SC-1 invariant 1's split - epoch mismatch
/// re-resolves, domain or incarnation mismatch denies terminally - a
/// consequence of one function rather than a second rule that can drift away
/// from it.
///
/// It is also what bounds what `Deny` leaks. Because identity is compared
/// first, `Deny(AppDeprovisioned)` is only ever returned to a caller whose
/// identity matched, so it tells a subject that its **own** authority record
/// carries a tombstone. It is not an oracle over other tenants, and it does not
/// distinguish "another subject is deprovisioned" from "no such subject" -
/// that question is answered by the identity comparison, uniformly, for every
/// value it could take.
#[must_use]
pub fn classify(observed: &ObservedAuthority, expected: &ExpectedAuthority) -> Verdict {
    // --- identity, first and entirely ---
    //
    // The domain leads because it is the coarsest: if the cluster answering is
    // not the one the binding captured, nothing else the record says is about
    // our subject at all, including its key.
    if observed.domain != expected.domain {
        return Verdict::Deny(DenyReason::AuthorityDomainMismatch);
    }
    // Key and incarnation together. A record naming a different key is as dead
    // a handle as one naming a newer incarnation, and the same next action -
    // resolve a fresh binding - is correct for both.
    if observed.identity != expected.identity {
        return Verdict::Deny(DenyReason::StaleAppIncarnation);
    }

    // --- only now, lifecycle ---
    match observed.lifecycle {
        LifecycleState::Deprovisioned => return Verdict::Deny(DenyReason::AppDeprovisioned),
        LifecycleState::Changing => return Verdict::ReResolve,
        LifecycleState::Stable => {}
    }

    // --- and the epoch, which re-resolves rather than denying ---
    if observed.epoch != expected.epoch {
        return Verdict::ReResolve;
    }

    Verdict::Current {
        ceiling: observed.ceiling.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn domain() -> AuthorityDomain {
        AuthorityDomain::new(7_262_000_000_000_000_001, 1)
    }

    fn expected() -> ExpectedAuthority {
        ExpectedAuthority {
            identity: AuthorityIdentity::for_app("app_alpha", 4),
            domain: domain(),
            epoch: SchemaEpoch::new(11),
        }
    }

    fn observed_matching() -> ObservedAuthority {
        ObservedAuthority {
            identity: AuthorityIdentity::for_app("app_alpha", 4),
            domain: domain(),
            epoch: SchemaEpoch::new(11),
            lifecycle: LifecycleState::Stable,
            ceiling: MaskCeiling::of(["support", "auto"]),
        }
    }

    #[test]
    fn a_matching_observation_is_current_and_carries_its_ceiling() {
        let verdict = classify(&observed_matching(), &expected());
        match verdict {
            Verdict::Current { ceiling } => {
                assert!(ceiling.permits("support"));
                assert!(!ceiling.permits("operator"));
            }
            other => panic!("expected Current, got {other:?}"),
        }
        assert!(
            !classify(&observed_matching(), &expected()).is_forcing(),
            "Current must not be a forcing publisher: it never claims the gate"
        );
    }

    /// **The identity-before-lifecycle order, and the only fixture that can
    /// see it.**
    ///
    /// The observation names a *different* subject AND is deprovisioned, so
    /// the two candidate orders return different reasons over the same input:
    /// identity-first denies `STALE_APP_INCARNATION`, lifecycle-first denies
    /// `APP_DEPROVISIONED`.
    ///
    /// Where this fails today: `classify` did not exist. It fails on any
    /// implementation that checks lifecycle before identity - which is the
    /// natural order to write, because the lifecycle field is the interesting
    /// one - and it is the only arm here that can distinguish the two.
    ///
    /// What this fixture deliberately does NOT cover: an observation that
    /// matches identity and is deprovisioned. That is the arm below, and it
    /// passes under both orders, which is exactly why it cannot stand in for
    /// this one.
    #[test]
    fn identity_is_compared_before_lifecycle_state() {
        let other_subject_and_deprovisioned = ObservedAuthority {
            identity: AuthorityIdentity::for_app("app_beta", 4),
            lifecycle: LifecycleState::Deprovisioned,
            ..observed_matching()
        };
        assert_eq!(
            classify(&other_subject_and_deprovisioned, &expected()),
            Verdict::Deny(DenyReason::StaleAppIncarnation),
            "an observation naming a different subject is denied for the identity mismatch, \
             never for that other subject's lifecycle"
        );

        // The same test over the incarnation axis: a newer incarnation that is
        // also deprovisioned is still an identity denial.
        let newer_incarnation_and_deprovisioned = ObservedAuthority {
            identity: AuthorityIdentity::for_app("app_alpha", 5),
            lifecycle: LifecycleState::Deprovisioned,
            ..observed_matching()
        };
        assert_eq!(
            classify(&newer_incarnation_and_deprovisioned, &expected()),
            Verdict::Deny(DenyReason::StaleAppIncarnation)
        );

        // And over the domain axis, which leads: a foreign cluster reporting a
        // tombstone is a domain mismatch, not a tombstone.
        let foreign_domain_and_deprovisioned = ObservedAuthority {
            domain: AuthorityDomain::new(7_262_000_000_000_000_002, 1),
            lifecycle: LifecycleState::Deprovisioned,
            ..observed_matching()
        };
        assert_eq!(
            classify(&foreign_domain_and_deprovisioned, &expected()),
            Verdict::Deny(DenyReason::AuthorityDomainMismatch)
        );
    }

    /// The control for the arm above: same lifecycle, matching identity.
    ///
    /// Where this fails today: `classify` did not exist. It passes under both
    /// candidate orders, which is why it is a control and not evidence for the
    /// ordering.
    #[test]
    fn a_matching_identity_that_is_deprovisioned_denies_for_the_tombstone() {
        let deprovisioned = ObservedAuthority {
            lifecycle: LifecycleState::Deprovisioned,
            ..observed_matching()
        };
        assert_eq!(
            classify(&deprovisioned, &expected()),
            Verdict::Deny(DenyReason::AppDeprovisioned)
        );
    }

    /// Where this fails today: `classify` did not exist. It fails on an
    /// implementation that denies an epoch change, or that follows it in
    /// place.
    #[test]
    fn a_changing_lifecycle_or_a_differing_epoch_re_resolves() {
        let changing = ObservedAuthority {
            lifecycle: LifecycleState::Changing,
            ..observed_matching()
        };
        assert_eq!(classify(&changing, &expected()), Verdict::ReResolve);

        let newer_epoch = ObservedAuthority {
            epoch: SchemaEpoch::new(12),
            ..observed_matching()
        };
        assert_eq!(classify(&newer_epoch, &expected()), Verdict::ReResolve);

        // An OLDER epoch re-resolves too. "Differs" is not "is newer": a
        // replica that has fallen behind is as unusable as one that has moved
        // on, and treating older as acceptable is how a stale read passes.
        let older_epoch = ObservedAuthority {
            epoch: SchemaEpoch::new(10),
            ..observed_matching()
        };
        assert_eq!(classify(&older_epoch, &expected()), Verdict::ReResolve);
    }

    /// Where this fails today: `DenyReason` did not exist. It fails on any
    /// implementation that collapses two reasons onto one code, or that marks
    /// one retryable.
    #[test]
    fn denial_reasons_are_distinct_and_none_is_retryable() {
        let codes: std::collections::BTreeSet<&str> =
            DenyReason::ALL.iter().map(|reason| reason.code()).collect();
        assert_eq!(
            codes.len(),
            DenyReason::ALL.len(),
            "collapsing two reasons onto one code makes the only distinguishable \
             signal a log line the creator cannot read"
        );
        assert_eq!(
            codes,
            [
                "APP_DEPROVISIONED",
                "AUTHORITY_DOMAIN_MISMATCH",
                "GRANT_REVOKED",
                "STALE_APP_INCARNATION",
            ]
                .into_iter()
                .collect()
        );
        for reason in DenyReason::ALL {
            assert!(!reason.retryable(), "{reason} must be terminal");
        }
    }

    /// Both `ReResolve` and `Deny` claim the gate; `Current` does not.
    #[test]
    fn re_resolve_and_deny_are_forcing_and_current_is_not() {
        assert!(Verdict::ReResolve.is_forcing());
        for reason in DenyReason::ALL {
            assert!(Verdict::Deny(reason).is_forcing());
        }
        assert!(!Verdict::Current {
            ceiling: MaskCeiling::default()
        }
        .is_forcing());
    }

    /// Invariant 8's algebra, checked on the fold itself.
    ///
    /// Where this fails today: `MaskCeiling` did not exist. It fails on a fold
    /// that takes the newly read value rather than meeting with it, which is
    /// the shape that lets a mid-transaction raise broaden authorization.
    #[test]
    fn meet_never_broadens_and_a_raise_changes_nothing() {
        let begin = MaskCeiling::of(["support", "auto"]);
        let effective = begin.clone();

        // A raise: the record now permits more than BEGIN did.
        let raised = MaskCeiling::of(["support", "auto", "operator"]);
        let folded = begin.meet(&effective).meet(&raised);
        assert_eq!(folded, MaskCeiling::of(["support", "auto"]));
        assert!(folded.is_no_broader_than(&begin));
        assert!(!folded.permits("operator"), "a raise must change nothing");

        // A lower value tightens the next authorization.
        let lowered = MaskCeiling::of(["support"]);
        let tightened = begin.meet(&folded).meet(&lowered);
        assert_eq!(tightened, MaskCeiling::of(["support"]));
        assert!(tightened.is_no_broader_than(&begin));
        assert!(!tightened.permits("auto"));
    }
}
