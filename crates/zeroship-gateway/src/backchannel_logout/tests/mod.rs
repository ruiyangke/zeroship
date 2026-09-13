#![allow(
    clippy::future_not_send,
    reason = "fixtures belong to their compio runtime"
)]

use std::sync::Arc;

use compio_postgres::Client;
use ntex::http::StatusCode;
use ntex::web::{self, test};
use serde_json::{Value, json};
use uuid::Uuid;
use zeroship_core::{app_id::AppId, user_id::UserId};

use crate::db::tests::postgres::Database;
use crate::tests::browser::*;

mod fixture;
mod replay;
mod retry;
mod revocation;
mod scope;
mod validation;

use fixture::*;
