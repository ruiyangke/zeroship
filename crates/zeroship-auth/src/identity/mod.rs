//! User identity flows: password, federation (Google/GitHub OAuth),
//! magic-link + email verification.

pub mod credentials;
pub mod deletion_cancel;
pub mod email;
pub mod eligibility;
pub mod linker;
pub mod magic_link;
pub mod oauth;
pub mod password;
pub mod password_reset;
pub mod totp;
pub mod verification;
