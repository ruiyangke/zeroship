//! The command builder the migration server's end-to-end suites share.
//!
//! Both suites launch the freshly built `zeroship-migrate-server` with the same
//! private environment: the credentials the dry run reads, and none inherited
//! from the test process. Building it in one place keeps the suites aligned.

use std::process::Command;

/// A strong value for the credentials the dry run reads.
pub const STRONG_HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A command for the service with a private environment.
///
/// Clears the inherited environment, then supplies the credentials the dry run
/// reads. Callers add their own arguments.
pub fn service_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zeroship-migrate-server"));
    command
        .env_clear()
        .env("ZEROSHIP_CONTROL_KEY", STRONG_HEX)
        .env("ZEROSHIP_MIGRATE_SERVER_POLICY_SEAL_KEY", STRONG_HEX)
        .env(
            "ZEROSHIP_MIGRATE_SERVER_DATABASE_URL",
            "postgresql://unused:unused@127.0.0.1:1/unused",
        )
        .env(
            "ZEROSHIP_MIGRATE_SERVER_PROVISION_DATABASE_URL",
            "postgresql://unused:unused@127.0.0.1:1/unused",
        );
    command
}
