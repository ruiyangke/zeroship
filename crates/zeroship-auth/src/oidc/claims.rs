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

/// Build the identity claim subset for a user and granted OIDC scopes.
#[must_use]
pub fn scope_gated_identity_claims<'a>(
    user: &UserRow,
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
        email: want_email.then(|| user.email.clone()),
        email_verified: want_email.then_some(user.email_verified_at.is_some()),
        name: want_profile.then(|| user.name.clone()),
        picture: if want_profile {
            user.avatar_url.clone()
        } else {
            None
        },
    }
}
