//! The auth-provider vocabulary, spelled once for every binary that names it.
//!
//! This is a VOCABULARY, not a behaviour. It says which auth backend a
//! deployment runs; it does not verify a token, mint one, or serve a login
//! page. The behaviour deliberately stays split, because the two sides of the
//! decision genuinely differ:
//!
//!   * `zeroship-auth` SERVES a provider. `Native` means it runs its own
//!     OAuth2/OIDC OP; `Supabase` means it drives browser-side GoTrue instead.
//!   * `zeroship-control`, `zeroship-authn` and `zeroship-migrated` VERIFY
//!     tokens through [`crate::auth_provider`], which the auth service does not
//!     use at all. `Native` means the platform OP is the issuer they trust.
//!
//! Before this enum moved here the same deployment decision had two names with
//! two value sets - `ZEROSHIP_AUTH_PROVIDER` = `native|supabase` on the auth
//! side, `ZEROSHIP_CONTROL_AUTH_PROVIDER` = `platform|supabase` on control's -
//! and nothing kept them coherent. `native` and `platform` were the same state
//! described from opposite vantage points, so an operator could set the pair to
//! disagree and learn about it at request time rather than at boot. One enum
//! behind one shared canonical name (`auth.provider`) makes the disagreement
//! unrepresentable.
//!
//! DUAL-ISSUER TRUST IS NOT A THIRD VALUE. Control still builds a
//! `DualIssuerProvider` when the value is `Supabase` AND a platform issuer is
//! configured; that stays DERIVED from two settings exactly as it was, so the
//! vocabulary keeps two states.

use std::fmt;

use clap::ValueEnum;
use serde::Deserialize;

/// Which auth backend a deployment runs.
///
/// `Deserialize` is present so a TOML overlay accepts the same spellings the
/// flag does; `rename_all` makes those spellings `native` and `supabase`, which
/// is exactly what `clap::ValueEnum` derives from the variant names.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum AuthProviderKind {
    /// The platform's own OP is the issuer: auth runs it, the verifiers trust
    /// it.
    #[default]
    Native,
    /// Supabase Auth / GoTrue is the issuer: auth drives its browser flow, the
    /// verifiers accept its tokens.
    Supabase,
}

impl AuthProviderKind {
    /// Return the canonical operator-facing spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Supabase => "supabase",
        }
    }
}

impl fmt::Display for AuthProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::AuthProviderKind;

    #[test]
    fn the_flag_the_overlay_and_the_report_agree_on_one_spelling() {
        // One vocabulary means the clap spelling, the TOML spelling and the
        // printed spelling are the same string. A rename that touched only one
        // of the three is what this pins.
        for (kind, spelling) in [
            (AuthProviderKind::Native, "native"),
            (AuthProviderKind::Supabase, "supabase"),
        ] {
            assert_eq!(kind.as_str(), spelling);
            assert_eq!(kind.to_string(), spelling);
            assert_eq!(
                toml::Value::String(spelling.to_owned())
                    .try_into::<AuthProviderKind>()
                    .expect("overlay accepts the flag spelling"),
                kind
            );
        }
    }

    #[test]
    fn the_retired_control_vocabulary_is_rejected() {
        // `platform` was control's word for the state now spelled `native`.
        // Accepting it silently would leave the two vocabularies alive.
        toml::Value::String("platform".to_owned())
            .try_into::<AuthProviderKind>()
            .expect_err("`platform` is not a value of this vocabulary");
    }

    #[test]
    fn the_default_is_the_platforms_own_op() {
        assert_eq!(AuthProviderKind::default(), AuthProviderKind::Native);
    }
}
