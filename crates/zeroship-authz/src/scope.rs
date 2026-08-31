use std::fmt::{Display, Formatter};

use crate::{Action, Effect, Policy, Resource, Statement};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    AppsRead,
    AppsWrite,
    AppsDeploy,
    AppsArchive,
    EnvRead,
    EnvWrite,
    SecretsRead,
    SecretsWrite,
    BillingRead,
    BillingWrite,
    TeamRead,
    TeamWrite,
    AccountRead,
    AccountWrite,
    DeploymentsRead,
    DeploymentsRollback,
}

impl Scope {
    pub const ALL: &'static [Self] = &[
        Self::AppsRead,
        Self::AppsWrite,
        Self::AppsDeploy,
        Self::AppsArchive,
        Self::EnvRead,
        Self::EnvWrite,
        Self::SecretsRead,
        Self::SecretsWrite,
        Self::BillingRead,
        Self::BillingWrite,
        Self::TeamRead,
        Self::TeamWrite,
        Self::AccountRead,
        Self::AccountWrite,
        Self::DeploymentsRead,
        Self::DeploymentsRollback,
    ];

    /// 1:1 mapping to Cedar Actions per the design.
    #[must_use]
    pub const fn action(self) -> Action {
        match self {
            Self::AppsRead => Action::AppsRead,
            Self::AppsWrite => Action::AppsWrite,
            Self::AppsDeploy => Action::AppsDeploy,
            Self::AppsArchive => Action::AppsArchive,
            Self::EnvRead => Action::EnvRead,
            Self::EnvWrite => Action::EnvWrite,
            Self::SecretsRead => Action::SecretsRead,
            Self::SecretsWrite => Action::SecretsWrite,
            Self::BillingRead => Action::BillingRead,
            Self::BillingWrite => Action::BillingWrite,
            Self::TeamRead => Action::TeamRead,
            Self::TeamWrite => Action::TeamWrite,
            Self::AccountRead => Action::AccountRead,
            Self::AccountWrite => Action::AccountWrite,
            Self::DeploymentsRead => Action::DeploymentsRead,
            Self::DeploymentsRollback => Action::DeploymentsRollback,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AppsRead => "apps:read",
            Self::AppsWrite => "apps:write",
            Self::AppsDeploy => "apps:deploy",
            Self::AppsArchive => "apps:archive",
            Self::EnvRead => "env:read",
            Self::EnvWrite => "env:write",
            Self::SecretsRead => "secrets:read",
            Self::SecretsWrite => "secrets:write",
            Self::BillingRead => "billing:read",
            Self::BillingWrite => "billing:write",
            Self::TeamRead => "team:read",
            Self::TeamWrite => "team:write",
            Self::AccountRead => "account:read",
            Self::AccountWrite => "account:write",
            Self::DeploymentsRead => "deployments:read",
            Self::DeploymentsRollback => "deployments:rollback",
        }
    }

    /// The consent-screen copy for each scope (`crates/auth/src/ui/consent.rs`
    /// and `device.rs` render exactly these strings).
    ///
    /// `env:*` covers everything that configures an app without redeploying
    /// it, which since creator self-service egress
    /// (`/api/apps/{id}/egress-rules`) includes the raw-TCP host allowlist. The
    /// label names both, because a screen that says only "environment
    /// variables" while the token can widen an app's network reach is a lie
    /// told at the exact moment consent is given. No `net:*` scope was added:
    /// the vocabulary is closed and shown to humans, and the capability is
    /// already bounded by plan caps and the operator's suffix catalog.
    #[must_use]
    pub const fn human_label(self) -> &'static str {
        match self {
            Self::AppsRead => "View your apps",
            Self::AppsWrite => "Create and modify your apps",
            Self::AppsDeploy => "Deploy code to your apps",
            Self::AppsArchive => "Archive and restore your apps",
            Self::EnvRead => "Read environment variables and allowed network hosts",
            Self::EnvWrite => "Modify environment variables and allowed network hosts",
            Self::SecretsRead => "Read secrets (names only)",
            Self::SecretsWrite => "Set and rotate secrets",
            Self::BillingRead => "View billing and earnings",
            Self::BillingWrite => "Manage payouts and billing",
            Self::TeamRead => "View team members",
            Self::TeamWrite => "Invite or remove team members",
            Self::AccountRead => "View your account profile",
            Self::AccountWrite => "Update your account profile",
            Self::DeploymentsRead => "List deployment history",
            Self::DeploymentsRollback => "Roll back deployments",
        }
    }

    /// Parses a scope token from the Phase 10 OAuth scope vocabulary.
    ///
    /// # Errors
    ///
    /// Returns [`ParseScopeError::Unknown`] when `s` is not one of the
    /// closed-vocabulary scope strings.
    pub fn parse(s: &str) -> Result<Self, ParseScopeError> {
        Ok(match s {
            "apps:read" => Self::AppsRead,
            "apps:write" => Self::AppsWrite,
            "apps:deploy" => Self::AppsDeploy,
            "apps:archive" => Self::AppsArchive,
            "env:read" => Self::EnvRead,
            "env:write" => Self::EnvWrite,
            "secrets:read" => Self::SecretsRead,
            "secrets:write" => Self::SecretsWrite,
            "billing:read" => Self::BillingRead,
            "billing:write" => Self::BillingWrite,
            "team:read" => Self::TeamRead,
            "team:write" => Self::TeamWrite,
            "account:read" => Self::AccountRead,
            "account:write" => Self::AccountWrite,
            "deployments:read" => Self::DeploymentsRead,
            "deployments:rollback" => Self::DeploymentsRollback,
            _ => return Err(ParseScopeError::Unknown(s.to_owned())),
        })
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
///   action: AnyOf(scopes.actions()), resource: Any, conditions: [] }] }`
/// per design section 10.2.
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
/// dropped.
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

    #[test]
    fn unknown_scope_rejects() {
        assert!(parse_scope_string("apps:read bogus:scope").is_err());
        assert!(
            Scope::parse("apps:delete").is_err(),
            "the removed delete authority must not survive as an alias"
        );
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
