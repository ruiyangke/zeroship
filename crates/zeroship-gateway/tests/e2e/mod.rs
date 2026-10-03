//! The gateway's end-to-end suites, linked into a single test executable.
//!
//! Every module here spawns the shipped `zeroship-gate` binary and observes its
//! startup and `--check-config` behaviour from outside the process.

mod config_env_tier;
mod credential_boot;
mod peer_boot;
