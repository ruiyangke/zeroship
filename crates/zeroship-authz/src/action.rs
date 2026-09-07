use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The closed action vocabulary.
///
/// Two families of names disappeared here, for the same reason and in the same
/// sweep. `TeamRead` / `TeamWrite` named an authority the platform never had a
/// route for; they are now [`Self::OrganizationMembersRead`] /
/// [`Self::OrganizationMembersWrite`], which the organization membership routes
/// gate on. `DeploymentsRollback` named an authority nothing enforced and
/// nothing granted, and it is deleted outright rather than renamed: no scope
/// token carries it, no policy band permits it, and no handler asks for it, so
/// keeping the variant would keep a word that authorizes nothing.
///
/// The word TEAM is reserved and names nothing in this crate. The abbreviation
/// `org` appears only inside an opaque typed-id VALUE, never in a name.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Action {
    AppsRead,
    AppsWrite,
    AppsDeploy,
    /// **Operator-only migration approval.** Authorizes passing the
    /// `?approved_versions=` go-live channel that COMPLETES an online-rename
    /// EXPAND / a scoped destructive migration op. Deliberately NOT in the
    /// creator scope vocabulary ([`crate::Scope`]) and permitted by NO policy
    /// band: every band is an allow-list, so the operator-vs-creator separation
    /// survives as the ABSENCE of a permit rather than as a forbid an
    /// evaluation error could disarm. A creator (or a prompt-injected AI
    /// deploying on their behalf) holding `apps:deploy` at any rank is refused
    /// the moment they pass a non-empty approval set.
    AppsApproveMigration,
    AppsArchive,
    DeploymentsRead,
    EnvRead,
    EnvWrite,
    SecretsRead,
    SecretsWrite,
    BillingRead,
    BillingWrite,
    OrganizationCreate,
    OrganizationRead,
    OrganizationWrite,
    /// Owner-only: seat the owners and admins of an organization. It appears in
    /// exactly one policy band, so it is satisfiable only at owner rank.
    OrganizationAdmin,
    OrganizationMembersRead,
    OrganizationMembersWrite,
    /// **Give up your OWN seat, and nobody else's.** It is a separate action
    /// rather than a case of [`Self::OrganizationMembersWrite`] because the two
    /// need opposite ranks: seating a member needs authority OVER the
    /// organization (`members:write` is banded at admin and above), while
    /// leaving one needs only a seat in it - and a viewer, who holds no
    /// `members:write` at any rank, must be able to leave.
    ///
    /// Reusing `organization:read` instead would have been the other way to
    /// reach a viewer, and it is worse: a client granted read-only consent
    /// could then delete its holder's seat. The route it gates
    /// (`DELETE /api/organizations/{id}/membership`) names no user at all, so
    /// the target is the bearer by construction rather than by a check.
    OrganizationMembersLeave,
    ProjectCreate,
    ProjectRead,
    ProjectWrite,
    ProjectMembersRead,
    ProjectMembersWrite,
    AccountRead,
    AccountWrite,
}

/// Every action, in declaration order. `from_wire` and [`Action::cedar_id`] are
/// reconciled against this in the crate tests, so a variant added without a
/// wire string is a test failure rather than a silent hole.
const ALL_ACTIONS: &[Action] = &[
    Action::AppsRead,
    Action::AppsWrite,
    Action::AppsDeploy,
    Action::AppsApproveMigration,
    Action::AppsArchive,
    Action::DeploymentsRead,
    Action::EnvRead,
    Action::EnvWrite,
    Action::SecretsRead,
    Action::SecretsWrite,
    Action::BillingRead,
    Action::BillingWrite,
    Action::OrganizationCreate,
    Action::OrganizationRead,
    Action::OrganizationWrite,
    Action::OrganizationAdmin,
    Action::OrganizationMembersRead,
    Action::OrganizationMembersWrite,
    Action::OrganizationMembersLeave,
    Action::ProjectCreate,
    Action::ProjectRead,
    Action::ProjectWrite,
    Action::ProjectMembersRead,
    Action::ProjectMembersWrite,
    Action::AccountRead,
    Action::AccountWrite,
];

impl Action {
    /// Every action in the closed vocabulary.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        ALL_ACTIONS
    }

    #[must_use]
    pub const fn cedar_id(&self) -> &'static str {
        match self {
            Self::AppsRead => "apps:read",
            Self::AppsWrite => "apps:write",
            Self::AppsDeploy => "apps:deploy",
            Self::AppsApproveMigration => "migrations:approve",
            Self::AppsArchive => "apps:archive",
            Self::DeploymentsRead => "deployments:read",
            Self::EnvRead => "env:read",
            Self::EnvWrite => "env:write",
            Self::SecretsRead => "secrets:read",
            Self::SecretsWrite => "secrets:write",
            Self::BillingRead => "billing:read",
            Self::BillingWrite => "billing:write",
            Self::OrganizationCreate => "organization:create",
            Self::OrganizationRead => "organization:read",
            Self::OrganizationWrite => "organization:write",
            Self::OrganizationAdmin => "organization:admin",
            Self::OrganizationMembersRead => "organization:members:read",
            Self::OrganizationMembersWrite => "organization:members:write",
            Self::OrganizationMembersLeave => "organization:members:leave",
            Self::ProjectCreate => "project:create",
            Self::ProjectRead => "project:read",
            Self::ProjectWrite => "project:write",
            Self::ProjectMembersRead => "project:members:read",
            Self::ProjectMembersWrite => "project:members:write",
            Self::AccountRead => "account:read",
            Self::AccountWrite => "account:write",
        }
    }

    #[must_use]
    pub fn from_wire(value: &str) -> Option<Self> {
        ALL_ACTIONS
            .iter()
            .copied()
            .find(|action| action.cedar_id() == value)
    }
}

impl Serialize for Action {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.cedar_id())
    }
}

impl<'de> Deserialize<'de> for Action {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::from_wire(&value)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown authz action: {value}")))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{Action, ALL_ACTIONS};

    #[test]
    fn every_action_has_a_unique_wire_id_that_round_trips() {
        let ids: HashSet<_> = ALL_ACTIONS.iter().map(Action::cedar_id).collect();
        assert_eq!(
            ids.len(),
            ALL_ACTIONS.len(),
            "two actions share one Cedar id"
        );
        for action in ALL_ACTIONS {
            assert_eq!(
                Action::from_wire(action.cedar_id()),
                Some(*action),
                "{} does not round-trip",
                action.cedar_id()
            );
        }
    }

    /// The two deleted words must not survive as parseable aliases. A renamed
    /// wire id that still parses is a back-compat shim by another name.
    #[test]
    fn deleted_vocabulary_does_not_parse() {
        for gone in ["team:read", "team:write", "deployments:rollback"] {
            assert!(
                Action::from_wire(gone).is_none(),
                "{gone} must not survive the sweep"
            );
        }
    }
}
