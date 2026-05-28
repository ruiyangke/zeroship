use std::fmt::{Display, Formatter};

use crate::{Action, Effect, Policy, Resource, Statement};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    AppsRead,
    AppsWrite,
    AppsDeploy,
    AppsDelete,
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
        Self::AppsDelete,
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
            Self::AppsDelete => Action::AppsDelete,
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
            Self::AppsDelete => "apps:delete",
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
            "apps:delete" => Self::AppsDelete,
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

/// Parse a Hydra scope string (space-separated).
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
    fn unknown_scope_rejects() {
        assert!(parse_scope_string("apps:read bogus:scope").is_err());
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
