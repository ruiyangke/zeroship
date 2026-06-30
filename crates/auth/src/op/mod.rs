//! Platform OpenID Provider primitives.
//!
//! This module is the P1a foundation only: file-custodied Ed25519 signing,
//! platform JWT minting, JWKS publication, and discovery metadata. OAuth grant
//! endpoints are wired in later slices.

pub mod issuer;
pub mod metadata;
pub mod signing;

pub use issuer::{
    AccessTokenClaims, AccessTokenMint, IdTokenClaims, IdTokenMint, Issuer,
    ACCESS_TOKEN_TYP, ACCESS_TOKEN_TTL_SECS, ID_TOKEN_TYP, ID_TOKEN_TTL_SECS,
};
