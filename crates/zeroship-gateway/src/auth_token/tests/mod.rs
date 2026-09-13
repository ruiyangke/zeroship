//! Browser identity, refresh and revocation through the real gateway handlers.

#![allow(
    clippy::future_not_send,
    reason = "fixtures belong to their compio runtime"
)]

use crate::db::tests::postgres::Database;
use crate::oidc_rp::{BrokerSecret, OidcRp};
use crate::tests::browser::*;
use crate::{anchors, GateState};
use ntex::web::{self, test};
use std::sync::{atomic::Ordering, Arc};
use uuid::Uuid;
use zeroship_core::{app_id::AppId, user_id::UserId};

mod cookies;
mod refresh;
mod revocation;
mod rotation;
