//! `IdP` session management (the cookie at `auth.zeroship.ai`).
//!
//! Distinct from hydra's session and from per-RP sessions. We track
//! "this browser holds a verified zeroship user"; hydra tracks "an
//! `OIDC` subject has been confirmed at the AS". The two coordinate
//! through hydra's `accept_login` call.

pub mod login;
