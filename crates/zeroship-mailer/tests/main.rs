//! Mail delivery and suppression against owned SMTP and migrated PostgreSQL servers.

#![recursion_limit = "256"]

mod common;
mod smtp;
mod sns;
mod suppressions;
