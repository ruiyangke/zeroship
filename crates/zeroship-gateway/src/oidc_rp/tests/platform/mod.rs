//! Gateway identity contracts exercised against the real platform provider.

#![allow(
    clippy::future_not_send,
    reason = "fixtures belong to their compio runtime"
)]

use crate::db::tests::postgres::Database;
use crate::oidc_rp::{BrokerSecret, BrowserAuthorizeParams, OidcRp, OidcRpError};
use ntex::http::StatusCode;
use ntex::web::{self, test};
use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;
use zeroship_core::{app_id::AppId, user_id::UserId};

mod bearer;
mod callback;
mod fixture;
mod login;
mod provider;
mod revocation;
mod worker;

use fixture::*;
use login::*;
use provider::Provider;

const ISSUER: &str = "https://auth.zeroship.ai/oauth2";
const REDIRECT_URI: &str = "http://127.0.0.1:9999/__zeroship/auth/callback";
const SECTOR: &str = "https://gateway-e2e.zeroship.test";
const APP_HOST: &str = "gateway-e2e.zeroship.test";
const APP_NAME: &str = "gateway-e2e";
const PASSWORD: &str = "gateway-test-password-with-enough-bytes-1234";
const BROKER_MASTER: &[u8] = b"gateway-oidc-rp-fixture-broker-master-32-bytes";
const PAIRWISE_SALT: [u8; 32] = [9; 32];

fn rp(base: &str) -> OidcRp {
    OidcRp::new(
        base,
        BrokerSecret::from_bytes(BROKER_MASTER.to_vec()).unwrap(),
        b"gateway-fixture-stash-signing-key-32-bytes".to_vec(),
    )
    .with_issuer(ISSUER)
}
