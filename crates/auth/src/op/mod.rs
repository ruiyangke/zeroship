//! Platform OpenID Provider primitives.
//!
//! Platform OpenID Provider primitives and closed-world OAuth endpoints.

pub mod authorization_code;
pub mod issuer;
pub mod device_token;
pub mod metadata;
pub mod refresh;
pub mod signing;

pub use issuer::{
    AccessTokenClaims, AccessTokenMint, IdTokenClaims, IdTokenMint, Issuer,
    PrincipalAccessTokenMint, ACCESS_TOKEN_TYP, ACCESS_TOKEN_TTL_SECS, ID_TOKEN_TYP,
    ID_TOKEN_TTL_SECS,
};
