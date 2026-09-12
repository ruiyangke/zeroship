//! Store behavior against owned `PostgreSQL` under the auth role.

mod email_verification;
mod identities;
mod magic_completions;
mod magic_links;
mod password_reset;
mod rate_limit;
mod session_visibility;
mod sessions;
mod totp;
