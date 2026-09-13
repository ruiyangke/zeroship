//! Mail delivery and suppression against owned SMTP servers and migrated databases.

#![recursion_limit = "256"]

mod common;
mod smtp;
mod sns;
mod suppressions;
