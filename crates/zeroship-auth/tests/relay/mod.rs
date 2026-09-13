//! Relay persistence and webhook delivery against an owned database.

#![allow(
    clippy::future_not_send,
    reason = "fixtures stay on their compio runtime"
)]

mod delivery;
mod fixtures;
mod http;
mod rate_limit;
mod store;
