//! User identity flows. Phase 2 = password. Phase 4 adds federation
//! (Google/GitHub OAuth). Phase 5 adds magic-link + email verification.

pub mod credentials;
pub mod email;
pub mod eligibility;
pub mod linker;
pub mod magic_link;
pub mod oauth;
pub mod password;
pub mod password_reset;
pub mod totp;
pub mod verification;
