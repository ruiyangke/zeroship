//! Relay persistence and webhook delivery against an owned database.

#![allow(
    clippy::future_not_send,
    reason = "fixtures stay on their compio runtime"
)]

mod fixtures;
mod store;
