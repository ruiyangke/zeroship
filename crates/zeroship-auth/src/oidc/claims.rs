//! Shared OIDC identity-claim projection.

use serde::Serialize;

use crate::store::users::UserRow;

/// OIDC identity claims unlocked by `email` and `profile` scopes.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ScopeGatedIdentityClaims {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email_verified: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub picture: Option<String>,
}

pub(super) struct IdentityProfile<'a> {
    pub email: &'a str,
    pub email_verified: bool,
    pub name: &'a str,
    pub picture: Option<&'a str>,
}

/// Build the identity claim subset for a user and granted OIDC scopes.
#[must_use]
pub fn scope_gated_identity_claims<'a>(
    user: &UserRow,
    granted_scopes: impl IntoIterator<Item = &'a str>,
) -> ScopeGatedIdentityClaims {
    IdentityProfile {
        email: &user.email,
        email_verified: user.email_verified_at.is_some(),
        name: &user.name,
        picture: user.avatar_url.as_deref(),
    }
    .for_scopes(granted_scopes)
}

impl IdentityProfile<'_> {
    pub(super) fn for_scopes<'a>(
        self,
        granted_scopes: impl IntoIterator<Item = &'a str>,
    ) -> ScopeGatedIdentityClaims {
        let mut want_email = false;
        let mut want_profile = false;
        for scope in granted_scopes {
            match scope {
                "email" => want_email = true,
                "profile" => want_profile = true,
                _ => {}
            }
        }

        ScopeGatedIdentityClaims {
            email: want_email.then(|| self.email.to_owned()),
            email_verified: want_email.then_some(self.email_verified),
            name: want_profile.then(|| self.name.to_owned()),
            picture: if want_profile {
                self.picture.map(str::to_owned)
            } else {
                None
            },
        }
    }
}
