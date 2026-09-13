//! Browser identity, refresh and revocation through the real gateway handlers.

#![allow(
    clippy::future_not_send,
    reason = "fixtures belong to their compio runtime"
)]

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use ed25519_dalek::SigningKey;
use ntex::web::{self, test};
use uuid::Uuid;
use zeroship_core::app_id::AppId;
use zeroship_core::user_id::UserId;

use crate::{
    anchors, backchannel_logout,
    blob_cache::{BlobCache, DiskBlobCache},
    enforce, idempotency,
    oidc_rp::{BrokerSecret, OidcRp},
    proxy::HashRing,
    session_token,
    sync::RouteCache,
    GateConfig, GateState,
};

use crate::db::tests::postgres::Database;

mod fixture;
mod op;
use fixture::*;
use op::*;

mod cookies;
mod refresh;
mod revocation;
mod rotation;
