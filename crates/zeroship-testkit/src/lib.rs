//! The test harness's own logic, moved out of shell.
//!
//! WHAT THIS IS FOR. `tests/` is 110 shell files and 41,426 lines, and the part
//! of it that reasons about databases reached PostgreSQL by shelling out to
//! `psql` and reading its stdout back as a string. That surface is 132
//! `INSERT INTO`, 39 `CREATE DATABASE`, 34 `DROP DATABASE`, 18 `SELECT COUNT`
//! and 5 `ALTER DATABASE` -- ordinary database work expressed as text
//! round-trips through a subprocess, where a connection failure and an empty
//! result set are the same two bytes of nothing. This crate is where that work
//! moves; `compio-postgres` replaces `psql`.
//!
//! WHAT IS PORTED SO FAR, and it is deliberately three files rather than
//! sixteen -- the shape is the deliverable, not the coverage:
//!
//!   [`overlay`]      was `tests/lib/test_config.sh`
//!   [`fingerprint`]  was `zs_schema_fingerprint` (dir) and
//!                    `zs_fingerprint_of_ref` (git tree), which MUST agree
//!   [`suite_db`]     was `tests/lib/suite_db.sh`
//!   [`sweep`]        was `tests/lib/sweep_db.sh`
//!
//! HOW IT IS INVOKED. One binary, `zs-testkit`, with subcommands (see
//! `src/main.rs`). The three `tests/lib/*.sh` files still exist and still
//! export the same shell function names, but their bodies are now one call to
//! that binary each, so `tests/run_auth_suite.sh` and the other consumers did
//! not change at all. A harness nobody can invoke is worse than the shell it
//! replaced, and `cargo test` alone cannot be invoked from a `.sh`.
//!
//! ZERO TOKIO. `compio-postgres` is the driver and nothing here reaches for an
//! HTTP client; see the crate's own `cargo tree -i tokio -e normal`.

pub mod admin;
pub mod fingerprint;
pub mod lock;
pub mod overlay;
pub mod suite_db;
pub mod sweep;

/// The exit codes the shell library used, kept because callers switch on them.
///
/// `1` and `2` are NOT interchangeable here: the `suite-db exists` subcommand
/// returns "absent" as 1 and "could not tell" as 2, and the whole point of that
/// question is that the two are different answers.
pub mod exit {
    /// Success.
    pub const OK: i32 = 0;
    /// A negative but expected answer -- "absent", "no such family".
    pub const NO: i32 = 1;
    /// A refusal or a failure: bad input, unreachable server, lost race lost.
    pub const FATAL: i32 = 2;
}
