//! OAuth/OIDC federation clients (Google, GitHub).
//!
//! Each provider module owns the upstream-specific dance: building the
//! authorize URL, exchanging the code, and normalising the upstream profile
//! into a shape the [`crate::identity::linker`] can consume. Cookie / CSRF /
//! stash handling lives in the HTTP layer (`crate::ui::oauth_google` etc.),
//! not here.

pub mod github;
pub mod google;
