//! `IdP` session management (the cookie at `auth.zeroship.ai`).
//!
//! Distinct from per-RP sessions. We track "this browser holds a verified
//! zeroship user"; app sessions are minted separately by the gateway.

pub mod login;
pub mod totp_challenge;
