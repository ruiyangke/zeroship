//! Deadlines for Control's enforcement cycles.
//!
//! Both enforcement cycles - the spend reconcile ([`crate::spend::SpendEngine::evaluate_all`])
//! and the recompute's period write
//! ([`crate::metering::Metering::replace_period_snapshot`]) - run their
//! database work under a [`CycleDeadline`] derived from the cycle's configured
//! interval ([`crate::cron::spend_recompute::SpendRecomputeConfig::interval`]).
//!
//! Two bounds, because neither alone is enough:
//!
//! * a SERVER-SIDE `statement_timeout`, which `PostgreSQL` itself enforces. It
//!   cancels the statement and, inside a transaction, aborts and rolls that
//!   transaction back. A lock wait, a stalled planner or a `pg_sleep` are all
//!   cut on the server, which also releases whatever the statement held.
//! * a CLIENT-SIDE backstop, slightly above the server bound, for a network
//!   that never answers at all: the server can never report a cancellation it
//!   never got to run, so the future is abandoned and its connection dropped.
//!   Closing the socket aborts the server-side transaction too.
//!
//! Every connection a cycle opens for its own work carries the server bound
//! where the cycle opens it - the sweep's session, the plan-catalog read, the
//! pricing reads and the period-rewrite transaction - and the backstop covers
//! the cycle as a whole, including a read that opens a connection of its own.
//!
//! A cycle that cannot finish before the next one is due is cut this way; its
//! transaction is rolled back, and the next tick starts clean and retries.

use std::future::Future;
use std::time::Duration;

use crate::registry::RegistryError;

/// The fraction of the cycle interval the server-side bound is allowed.
///
/// The server is given three quarters of the interval and the client backstop
/// the whole of it. The quarter in between is the room between the two: the
/// server cancels a slow statement first, and only a network that never
/// answers reaches the backstop. An interval is a wall-clock cadence, so a
/// cycle that used all of it would push every later cycle past its slot; three
/// quarters keeps a cut cycle inside its slot with the sleep it owes.
const SERVER_NUMERATOR: u128 = 3;
const SERVER_DENOMINATOR: u128 = 4;

/// Which of a cycle's two bounds ended it.
///
/// Named so a cut reports the bound that actually fired: a server
/// `statement_timeout` cancels and rolls back on the server, while the client
/// backstop drops a connection whose server never answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeadlineBound {
    /// The server-side `statement_timeout`; `PostgreSQL` canceled the statement
    /// and rolled its transaction back.
    Server,
    /// The client-side backstop; the connection was dropped.
    ClientBackstop,
}

impl DeadlineBound {
    /// The bound's name, for a log field or an error message.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::ClientBackstop => "client backstop",
        }
    }
}

impl std::fmt::Display for DeadlineBound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The deadline one enforcement cycle's database work runs under.
///
/// Constructed from the cycle's configured interval, so the same knob that
/// sets the cadence sets the bound. [`Self::server`] is the `statement_timeout`
/// installed on the cycle's connection or transaction; [`Self::backstop`] is
/// the client-side timeout wrapped around the whole cycle's database work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CycleDeadline {
    interval: Duration,
}

impl CycleDeadline {
    /// Derive a cycle's deadlines from its configured interval.
    ///
    /// A zero interval (no caller sets one today - the cron clamps to a second
    /// and the compiled default is an hour) is lifted to a millisecond so the
    /// server bound cannot be spelled as `PostgreSQL`'s `0`, which means "no
    /// limit at all".
    #[must_use]
    pub fn from_interval(interval: Duration) -> Self {
        Self {
            interval: interval.max(Duration::from_millis(1)),
        }
    }

    /// The server-side bound: three quarters of the interval, in whole
    /// milliseconds and never zero.
    #[must_use]
    pub fn server(&self) -> Duration {
        Duration::from_millis(self.server_ms())
    }

    /// The server-side bound as whole milliseconds, at least one.
    ///
    /// Whole milliseconds because `PostgreSQL`'s `statement_timeout` has no
    /// finer resolution; at least one because a zero is "wait forever".
    #[must_use]
    pub fn server_ms(&self) -> u64 {
        let ms = self.interval.as_millis() * SERVER_NUMERATOR / SERVER_DENOMINATOR;
        u64::try_from(ms).unwrap_or(u64::MAX).max(1)
    }

    /// The client-side backstop: the whole interval, and always above the
    /// server bound so the server gets the first chance to cancel and roll back.
    #[must_use]
    pub fn backstop(&self) -> Duration {
        self.interval.max(self.server() + Duration::from_millis(1))
    }

    /// The client-side backstop as whole milliseconds, at least one.
    #[must_use]
    pub fn backstop_ms(&self) -> u64 {
        u64::try_from(self.backstop().as_millis())
            .unwrap_or(u64::MAX)
            .max(1)
    }

    /// The value of `bound` in whole milliseconds.
    #[must_use]
    pub fn bound_ms(&self, bound: DeadlineBound) -> u64 {
        match bound {
            DeadlineBound::Server => self.server_ms(),
            DeadlineBound::ClientBackstop => self.backstop_ms(),
        }
    }

    /// The value for a `SET statement_timeout = ...`, quoted with its unit.
    #[must_use]
    pub fn statement_timeout(&self) -> String {
        format!("'{}ms'", self.server_ms())
    }

    /// The error a cut cycle reports, naming the operation, the bound that
    /// fired, and its value.
    #[must_use]
    pub fn violation(&self, operation: &'static str, bound: DeadlineBound) -> RegistryError {
        RegistryError::CycleDeadline {
            operation,
            deadline_ms: self.bound_ms(bound),
            bound,
        }
    }

    /// Relabel a server-side cancellation as the deadline it was installed for,
    /// leaving every other error unchanged.
    ///
    /// [`RegistryError::QueryCanceled`] is what `statement_timeout` surfaces; on
    /// a connection this cycle set the bound on, it means the SERVER bound
    /// fired.
    #[must_use]
    pub fn classify(&self, operation: &'static str, error: RegistryError) -> RegistryError {
        match error {
            RegistryError::QueryCanceled(_) => self.violation(operation, DeadlineBound::Server),
            other => other,
        }
    }
}

/// Install `deadline` as the SESSION `statement_timeout` on `conn`.
///
/// Session scope, not `SET LOCAL`: [`crate::spend::SpendEngine::evaluate_all`]
/// runs its fleet read in autocommit, outside any transaction, so a
/// transaction-local setting would never reach it. The connection is opened for
/// one cycle and dropped with it, so session scope IS cycle scope here, and the
/// per-app write transactions inherit it and are aborted and rolled back by the
/// server when it fires.
///
/// # Errors
/// Returns the connection's error if the `SET` cannot be issued.
pub(crate) async fn set_session_statement_timeout(
    conn: &compio_postgres::Client,
    deadline: CycleDeadline,
) -> Result<(), RegistryError> {
    conn.batch_execute(&format!(
        "SET statement_timeout = {}",
        deadline.statement_timeout()
    ))
    .await?;
    Ok(())
}

/// Run a cycle's database work under `deadline`'s client-side backstop.
///
/// Returns the work's own error unchanged, except that a server-side
/// cancellation ([`RegistryError::QueryCanceled`]) is reported as the deadline
/// it was installed for. A backstop expiry is reported as the deadline too, and
/// abandoning the future drops the connection, which rolls back whatever the
/// server had not.
///
/// # Errors
/// The work's own error, or [`RegistryError::CycleDeadline`] when the deadline
/// was what ended it.
pub(crate) async fn within<T, F>(
    deadline: CycleDeadline,
    operation: &'static str,
    work: F,
) -> Result<T, RegistryError>
where
    F: Future<Output = Result<T, RegistryError>>,
{
    let outcome = compio::time::timeout(deadline.backstop(), work).await;
    let (cut, bound) = match outcome {
        Ok(Ok(value)) => return Ok(value),
        Ok(Err(error)) => match deadline.classify(operation, error) {
            violation @ RegistryError::CycleDeadline { .. } => {
                (violation, DeadlineBound::Server)
            }
            other => return Err(other),
        },
        Err(_elapsed) => (
            deadline.violation(operation, DeadlineBound::ClientBackstop),
            DeadlineBound::ClientBackstop,
        ),
    };
    tracing::error!(
        operation,
        deadline_ms = deadline.bound_ms(bound),
        bound = bound.as_str(),
        "control enforcement cycle cut at its deadline; the transaction was rolled back \
         and the next tick retries"
    );
    Err(cut)
}

/// Run `work` under `deadline`'s client-side backstop, building the cut error
/// with `cut`.
///
/// The peer of [`within`] for a caller whose errors are not
/// [`RegistryError`]: a backstop expiry becomes
/// `cut(operation, deadline_ms, bound)` with [`DeadlineBound::ClientBackstop`],
/// and the work's own error is returned unchanged, so the caller relabels a
/// server-side cancellation with [`CycleDeadline::classify`] where it knows the
/// operation.
///
/// # Errors
/// The work's own error, or whatever `cut` builds when the backstop expired.
pub(crate) async fn backstop<T, E, F, C>(
    deadline: CycleDeadline,
    operation: &'static str,
    work: F,
    cut: C,
) -> Result<T, E>
where
    F: Future<Output = Result<T, E>>,
    C: FnOnce(&'static str, u64, DeadlineBound) -> E,
{
    match compio::time::timeout(deadline.backstop(), work).await {
        Ok(result) => result,
        Err(_elapsed) => {
            let bound = DeadlineBound::ClientBackstop;
            tracing::error!(
                operation,
                deadline_ms = deadline.bound_ms(bound),
                bound = bound.as_str(),
                "control enforcement cycle cut at its deadline; the transaction was rolled back \
                 and the next tick retries"
            );
            Err(cut(operation, deadline.bound_ms(bound), bound))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_server_bound_is_three_quarters_of_the_interval() {
        let deadline = CycleDeadline::from_interval(Duration::from_mins(1));
        assert_eq!(deadline.server_ms(), 45_000);
        assert_eq!(deadline.statement_timeout(), "'45000ms'");
    }

    #[test]
    fn the_backstop_is_above_the_server_bound_and_inside_the_interval() {
        let deadline = CycleDeadline::from_interval(Duration::from_mins(1));
        assert!(
            deadline.backstop() > deadline.server(),
            "the backstop must be above the server bound so the server cancels first"
        );
        assert_eq!(
            deadline.backstop(),
            Duration::from_mins(1),
            "the backstop is the whole interval, so a cycle is cut before the next is due"
        );
    }

    #[test]
    fn a_sub_millisecond_interval_still_yields_a_positive_server_bound() {
        // `PostgreSQL` spells "no limit" as `0`; a deadline that rounded down to
        // zero would disable the very bound it exists to install.
        let deadline = CycleDeadline::from_interval(Duration::ZERO);
        assert_eq!(deadline.server_ms(), 1);
        assert!(deadline.backstop() > deadline.server());
    }

    /// The client-side backstop is the only bound that can end a future the
    /// server never gets to cancel, such as a network that never answers. The
    /// outer `timeout` is a detection bound, so a `within` without its backstop
    /// fails by name instead of hanging the suite. The reported bound must be
    /// the backstop, not the server bound below it.
    #[compio::test]
    async fn the_backstop_cuts_a_future_that_never_answers() {
        let deadline = CycleDeadline::from_interval(Duration::from_millis(200));
        let outcome = compio::time::timeout(
            Duration::from_secs(5),
            within(deadline, "never_answers", async {
                std::future::pending::<()>().await;
                Ok::<(), RegistryError>(())
            }),
        )
        .await
        .expect("within returned within the detection bound");
        match outcome {
            Err(RegistryError::CycleDeadline {
                deadline_ms, bound, ..
            }) => {
                assert_eq!(bound, DeadlineBound::ClientBackstop);
                assert_eq!(
                    deadline_ms,
                    deadline.backstop_ms(),
                    "the backstop cut must report the bound that fired, not the server bound"
                );
            }
            other => panic!("a future that never answers must be cut at the backstop, got {other:?}"),
        }
    }

    /// A server cancellation reports the SERVER bound and its value.
    #[test]
    fn classify_reports_the_server_bound() {
        let deadline = CycleDeadline::from_interval(Duration::from_mins(1));
        match deadline.classify("evaluate_all", RegistryError::QueryCanceled("57014".into())) {
            RegistryError::CycleDeadline {
                deadline_ms, bound, ..
            } => {
                assert_eq!(bound, DeadlineBound::Server);
                assert_eq!(deadline_ms, deadline.server_ms());
            }
            other => panic!("a cancellation must classify as a server-bound cut, got {other:?}"),
        }
    }
}
