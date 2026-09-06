use std::fmt::{Display, Formatter};

use crate::{Action, Effect, Policy, Resource, Statement};

/// The OAuth consent vocabulary: the actions a human can delegate to a client.
///
/// It is exactly [`Action`] minus [`Action::AppsApproveMigration`], which is
/// operator-only and has no consent copy because no creator may delegate it.
/// `Scope::TeamRead` / `TeamWrite` are renamed to the organization spelling;
/// `Scope::DeploymentsRollback` is DELETED. It rendered "Roll back deployments"
/// on the consent screen for an authority no handler ever checked and no policy
/// ever granted - a promise the platform would have started honouring for every
/// already-issued token the day a rollback route landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    AppsRead,
    AppsWrite,
    AppsDeploy,
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
    OrganizationAdmin,
    OrganizationMembersRead,
    OrganizationMembersWrite,
    ProjectCreate,
    ProjectRead,
    ProjectWrite,
    ProjectMembersRead,
    ProjectMembersWrite,
    AccountRead,
    AccountWrite,
}

impl Scope {
    pub const ALL: &'static [Self] = &[
        Self::AppsRead,
        Self::AppsWrite,
        Self::AppsDeploy,
        Self::AppsArchive,
        Self::DeploymentsRead,
        Self::EnvRead,
        Self::EnvWrite,
        Self::SecretsRead,
        Self::SecretsWrite,
        Self::BillingRead,
        Self::BillingWrite,
        Self::OrganizationCreate,
        Self::OrganizationRead,
        Self::OrganizationWrite,
        Self::OrganizationAdmin,
        Self::OrganizationMembersRead,
        Self::OrganizationMembersWrite,
        Self::ProjectCreate,
        Self::ProjectRead,
        Self::ProjectWrite,
        Self::ProjectMembersRead,
        Self::ProjectMembersWrite,
        Self::AccountRead,
        Self::AccountWrite,
    ];

    /// 1:1 mapping to Cedar Actions per the design.
    #[must_use]
    pub const fn action(self) -> Action {
        match self {
            Self::AppsRead => Action::AppsRead,
            Self::AppsWrite => Action::AppsWrite,
            Self::AppsDeploy => Action::AppsDeploy,
            Self::AppsArchive => Action::AppsArchive,
            Self::DeploymentsRead => Action::DeploymentsRead,
            Self::EnvRead => Action::EnvRead,
            Self::EnvWrite => Action::EnvWrite,
            Self::SecretsRead => Action::SecretsRead,
            Self::SecretsWrite => Action::SecretsWrite,
            Self::BillingRead => Action::BillingRead,
            Self::BillingWrite => Action::BillingWrite,
            Self::OrganizationCreate => Action::OrganizationCreate,
            Self::OrganizationRead => Action::OrganizationRead,
            Self::OrganizationWrite => Action::OrganizationWrite,
            Self::OrganizationAdmin => Action::OrganizationAdmin,
            Self::OrganizationMembersRead => Action::OrganizationMembersRead,
            Self::OrganizationMembersWrite => Action::OrganizationMembersWrite,
            Self::ProjectCreate => Action::ProjectCreate,
            Self::ProjectRead => Action::ProjectRead,
            Self::ProjectWrite => Action::ProjectWrite,
            Self::ProjectMembersRead => Action::ProjectMembersRead,
            Self::ProjectMembersWrite => Action::ProjectMembersWrite,
            Self::AccountRead => Action::AccountRead,
            Self::AccountWrite => Action::AccountWrite,
        }
    }

    /// The wire token. It is the mapped action's Cedar id, so the two cannot
    /// drift: a scope whose token disagreed with its action would be a consent
    /// screen naming one authority and a policy evaluating another.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.action().cedar_id()
    }

    /// The consent-screen copy for each scope (`crates/zeroship-auth/src/ui/consent.rs`
    /// and `device.rs` render exactly these strings, both by way of this
    /// function - neither carries a second copy of the wording).
    ///
    /// `env:*` covers everything that configures an app without redeploying
    /// it, which since creator self-service egress
    /// (`/api/apps/{id}/egress-rules`) includes the raw-TCP host allowlist. The
    /// label names both, because a screen that says only "environment
    /// variables" while the token can widen an app's network reach is a lie
    /// told at the exact moment consent is given. No `net:*` scope was added:
    /// the vocabulary is closed and shown to humans, and the capability is
    /// already bounded by plan caps and the operator's suffix catalog.
    ///
    /// The organization and project labels are written to the SAME standard:
    /// each names the authority its policy band actually confers, and none of
    /// them names an authority no band grants.
    #[must_use]
    pub const fn human_label(self) -> &'static str {
        match self {
            Self::AppsRead => "View your apps",
            Self::AppsWrite => "Create and modify your apps",
            Self::AppsDeploy => "Deploy code to your apps",
            Self::AppsArchive => "Archive and restore your apps",
            Self::DeploymentsRead => "List deployment history",
            Self::EnvRead => "Read environment variables and allowed network hosts",
            Self::EnvWrite => "Modify environment variables and allowed network hosts",
            Self::SecretsRead => "Read secrets (names only)",
            Self::SecretsWrite => "Set and rotate secrets",
            Self::BillingRead => "View billing and earnings",
            Self::BillingWrite => "Manage payouts and billing",
            Self::OrganizationCreate => "Create new organizations",
            Self::OrganizationRead => "View organizations you belong to",
            Self::OrganizationWrite => "Rename an organization and change its billing contact",
            Self::OrganizationAdmin => "Add or remove organization owners and admins",
            Self::OrganizationMembersRead => "View organization members",
            Self::OrganizationMembersWrite => "Invite, remove and re-role organization members",
            Self::ProjectCreate => "Create projects in your organizations",
            Self::ProjectRead => "View projects",
            Self::ProjectWrite => "Rename and delete projects",
            Self::ProjectMembersRead => "View project members",
            Self::ProjectMembersWrite => "Add, remove and re-role project members",
            Self::AccountRead => "View your account profile",
            Self::AccountWrite => "Update your account profile",
        }
    }

    /// Parses a scope token from the closed OAuth scope vocabulary.
    ///
    /// # Errors
    ///
    /// Returns [`ParseScopeError::Unknown`] when `s` is not one of the
    /// closed-vocabulary scope strings.
    pub fn parse(s: &str) -> Result<Self, ParseScopeError> {
        Self::ALL
            .iter()
            .copied()
            .find(|scope| scope.as_str() == s)
            .ok_or_else(|| ParseScopeError::Unknown(s.to_owned()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParseScopeError {
    Unknown(String),
}

impl Display for ParseScopeError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown(scope) => write!(f, "unknown OAuth scope: {scope}"),
        }
    }
}

impl std::error::Error for ParseScopeError {}

/// Build an implicit policy from a scope set.
///
/// Maps to: `Policy { effect: Allow, statements: [Statement {
///   action: AnyOf(scopes.actions()), resource: Any, conditions: [] }] }`.
///
/// The resource is ALWAYS `Any`, which `lower_resources` renders as a bare
/// `resource` matching every resource TYPE. A bearer's wrapper therefore
/// narrows the ACTION and nothing else - the rank comparison in the static
/// bands is the only thing standing between a token and an organization it has
/// no seat in. That is why no band may permit an organization action without a
/// rank comparison, and why `self_service.cedar` carries only actions that
/// confer no authority over an existing organization.
#[must_use]
pub fn scopes_to_policy(scopes: &[Scope]) -> Policy {
    Policy {
        name: "oauth_scopes".to_owned(),
        statements: vec![Statement {
            effect: Effect::Allow,
            actions: scopes.iter().map(|scope| scope.action()).collect(),
            resources: vec![Resource::Any],
            conditions: Vec::new(),
        }],
    }
}

/// Parse an OAuth scope string (space-separated).
///
/// Unknown scopes return [`ParseScopeError::Unknown`]; they are not silently
/// dropped. One unknown token fails the WHOLE string, so a token minted against
/// the pre-sweep vocabulary is refused outright rather than narrowed.
///
/// # Errors
///
/// Returns [`ParseScopeError::Unknown`] if any token in `raw` is outside the
/// closed-vocabulary scope list.
pub fn parse_scope_string(raw: &str) -> Result<Vec<Scope>, ParseScopeError> {
    raw.split_whitespace().map(Scope::parse).collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use crate::{Action, Effect, Resource};

    use super::{parse_scope_string, scopes_to_policy, Scope};

    #[test]
    fn every_scope_has_unique_action() {
        let actions: HashSet<_> = Scope::ALL.iter().map(|s| s.action()).collect();
        assert_eq!(actions.len(), Scope::ALL.len(), "scope->action must be 1:1");
    }

    /// The scope vocabulary is the action vocabulary minus exactly the
    /// operator-only approval action. Stating it as a set difference (rather
    /// than as two hand-maintained lists) is what makes a newly added action
    /// either consent-visible on purpose or excluded on purpose, never
    /// forgotten.
    #[test]
    fn scope_vocabulary_is_the_action_vocabulary_minus_operator_approval() {
        let scoped: HashSet<Action> = Scope::ALL.iter().map(|s| s.action()).collect();
        let expected: HashSet<Action> = Action::all()
            .iter()
            .copied()
            .filter(|a| *a != Action::AppsApproveMigration)
            .collect();
        assert_eq!(scoped, expected);
    }

    #[test]
    fn round_trips_through_string() {
        for s in Scope::ALL {
            assert_eq!(Scope::parse(s.as_str()).unwrap(), *s);
        }
    }

    #[test]
    fn human_labels_are_consent_copy() {
        assert_eq!(Scope::AppsDeploy.human_label(), "Deploy code to your apps");
        assert_eq!(
            Scope::AppsArchive.human_label(),
            "Archive and restore your apps"
        );
        assert_eq!(
            Scope::SecretsRead.human_label(),
            "Read secrets (names only)"
        );
    }

    /// Every scope's copy must be distinct and non-empty. Two scopes rendering
    /// the same sentence is a consent screen that cannot tell a human which of
    /// two authorities they are approving.
    #[test]
    fn consent_copy_is_present_and_distinct() {
        let labels: HashSet<_> = Scope::ALL.iter().map(|s| s.human_label()).collect();
        assert_eq!(labels.len(), Scope::ALL.len(), "duplicate consent copy");
        for scope in Scope::ALL {
            assert!(!scope.human_label().is_empty(), "{scope:?}");
        }
    }

    /// `env:*` authorizes `/api/apps/{id}/egress-rules` as well as vars, so the
    /// consent copy has to name the network capability. This pins the wording
    /// against a future edit that trims it back to "environment variables"
    /// and silently understates what the user is approving.
    ///
    /// What this does NOT check: that the copy is accurate about anything
    /// else, or that the endpoint still uses `env:write`. It fails only on the
    /// label losing the network clause.
    #[test]
    fn env_scope_labels_name_the_network_capability() {
        for scope in [Scope::EnvRead, Scope::EnvWrite] {
            let label = scope.human_label();
            assert!(
                label.contains("network hosts"),
                "{} consent copy must name egress hosts, got {label:?}",
                scope.as_str()
            );
        }
    }

    /// The consent screen must not say "team" anywhere: the word is reserved
    /// and names nothing the platform has. This is the copy half of the
    /// vocabulary sweep - the ids can be renamed while the sentence a human
    /// reads still promises team management.
    #[test]
    fn no_consent_copy_says_team() {
        for scope in Scope::ALL {
            let label = scope.human_label().to_lowercase();
            assert!(
                !label.contains("team"),
                "{} consent copy still says team: {label:?}",
                scope.as_str()
            );
        }
    }

    #[test]
    fn unknown_scope_rejects() {
        assert!(parse_scope_string("apps:read bogus:scope").is_err());
        assert!(
            Scope::parse("apps:delete").is_err(),
            "the removed delete authority must not survive as an alias"
        );
        for gone in ["team:read", "team:write", "deployments:rollback"] {
            assert!(
                Scope::parse(gone).is_err(),
                "{gone} must not survive the vocabulary sweep"
            );
        }
    }

    #[test]
    fn scopes_to_policy_allows_only_listed_actions() {
        let pol = scopes_to_policy(&[Scope::AppsRead, Scope::AppsDeploy]);

        assert_eq!(pol.statements.len(), 1);
        let statement = &pol.statements[0];
        assert_eq!(statement.effect, Effect::Allow);
        assert_eq!(
            statement.actions,
            vec![Action::AppsRead, Action::AppsDeploy]
        );
        assert_eq!(statement.resources, vec![Resource::Any]);
        assert!(statement.conditions.is_empty());
    }
}
