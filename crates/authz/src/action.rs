use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
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
    PlatformPoliciesWrite,
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
            Self::PlatformPoliciesWrite => "platform_policies:write",
        }
    }

    #[must_use]
    pub fn from_wire(value: &str) -> Option<Self> {
        Some(match value {
            "apps:read" => Self::AppsRead,
            "apps:write" => Self::AppsWrite,
            "apps:deploy" => Self::AppsDeploy,
            "apps:delete" => Self::AppsDelete,
            "deployments:read" => Self::DeploymentsRead,
            "deployments:rollback" => Self::DeploymentsRollback,
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
            "platform_policies:write" => Self::PlatformPoliciesWrite,
            _ => return None,
        })
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
        Self::from_wire(&value).ok_or_else(|| {
            serde::de::Error::unknown_variant(
                &value,
                &[
                    "apps:read",
                    "apps:write",
                    "apps:deploy",
                    "apps:delete",
                    "deployments:read",
                    "deployments:rollback",
                    "env:read",
                    "env:write",
                    "secrets:read",
                    "secrets:write",
                    "billing:read",
                    "billing:write",
                    "team:read",
                    "team:write",
                    "account:read",
                    "account:write",
                    "platform_policies:write",
                ],
            )
        })
    }
}
