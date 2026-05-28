use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    AppsRead,
    AppsWrite,
    AppsDeploy,
    AppsDelete,
    DeploymentsRead,
    DeploymentsRollback,
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
}

impl Action {
    #[must_use]
    pub const fn cedar_id(&self) -> &'static str {
        match self {
            Self::AppsRead => "apps:read",
            Self::AppsWrite => "apps:write",
            Self::AppsDeploy => "apps:deploy",
            Self::AppsDelete => "apps:delete",
            Self::DeploymentsRead => "deployments:read",
            Self::DeploymentsRollback => "deployments:rollback",
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
        }
    }
}
