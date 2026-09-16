//! Platform database orchestration owned by xtask.
#![allow(dead_code)]
pub mod admin;
pub mod fingerprint;
pub mod live_db;
pub mod lock;
pub mod overlay;
pub mod suite_db;
pub mod sweep;
pub mod sweep_command;

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
    /// The check could not look. Distinct from [`OK`] on purpose: "scanned
    /// everything, found nothing to do" and "could not scan, so found nothing"
    /// used to be the same zero, and the sweeper dropped databases on the
    /// second one. See [`super::sweep::verdict`].
    pub const BLIND: i32 = 3;
}
