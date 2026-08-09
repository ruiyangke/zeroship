//! The finite-timeout-budget rule the apply path enforces on every dialect.
//!
//! PostgreSQL and MySQL both spell "no limit" as `0`: `SET lock_timeout = 0` and
//! `SET statement_timeout = 0` read back as `0` (not `0ms`), and MySQL's
//! `max_execution_time = 0` is likewise unbounded. A budget of zero therefore
//! does not tighten the limit, it REMOVES it, and the DDL waits forever while
//! holding whatever it already acquired.
//!
//! Zero is already outside the contract the engine states for itself:
//! [`ExecutorConfig`](crate::conn::ExecutorConfig) documents the PG
//! `statement_timeout` + `lock_timeout` as **mandatory** (no indefinite locks /
//! DoS), and
//! [`MigrationFlags::lock_timeout_ms`](crate::model::migration::MigrationFlags::lock_timeout_ms)
//! documents the maintenance-window override as raising a FINITE budget.
//!
//! **Why the refusal lives here, at the value's resolution, and not only at the
//! IR load gate.** The load gate is early author feedback, not a boundary: the IR
//! path is not the only way a zero reaches a session render.
//! [`Migration`](crate::model::migration::Migration) and
//! [`MigrationFlags`](crate::model::migration::MigrationFlags) are public serde
//! structs with public fields, so an embedder can build `lock_timeout_ms: Some(0)`,
//! compute a matching checksum, and call [`apply`](crate::apply::executor::apply)
//! with no loader involved. A zero can also arrive from CONFIG, which no IR
//! validation can see: `ExecutorConfig::statement_timeout_ms` is
//! `Duration::as_millis`, and `Duration::from_micros(500).as_millis() == 0`.
//! [`resolve_timeout_ms`] sits at the one point every one of those paths
//! converges on -- where the effective value is computed and handed to the
//! session render -- so each path is refused on its own evidence.
//!
//! **Why it refuses instead of clamping.** `timeout_ms` and `lock_timeout_ms` are
//! folded into the migration checksum, so substituting a different budget would
//! run a migration whose behaviour no longer matches its checksummed identity.
//! Refusing keeps the applied artifact and its identity in agreement.

/// A resolved timeout budget of `0`, which the target engine reads as "no limit".
///
/// Carries where the zero came from so the operator knows which of the two knobs
/// to fix: the migration's own override, or the executor configuration.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "migration {version} would run with {setting} = 0, which the database reads as \
     \"no limit\" rather than a zero budget: the statement would wait indefinitely \
     while holding the locks it already took. The zero comes from {origin}. Set a \
     finite budget (the timeouts are mandatory), or drop the override to inherit \
     the executor default; the engine refuses rather than substituting a budget the \
     migration checksum does not describe."
)]
pub struct IndefiniteTimeoutError {
    /// The migration whose effective budget resolved to zero.
    pub version: String,
    /// The engine setting the budget feeds (`lock_timeout`, `statement_timeout`,
    /// `max_execution_time`, `innodb_lock_wait_timeout`).
    pub setting: &'static str,
    /// Where the zero came from -- the per-migration override or the config.
    pub origin: TimeoutOrigin,
}

/// Which knob produced a zero budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutOrigin {
    /// The migration's own `flags.timeout_ms` / `flags.lock_timeout_ms` override.
    MigrationFlag(&'static str),
    /// The executor-wide default, i.e. the corresponding `ExecutorConfig` field.
    /// A sub-millisecond `Duration` truncates to zero whole milliseconds here.
    ExecutorConfig(&'static str),
}

impl std::fmt::Display for TimeoutOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MigrationFlag(field) => {
                write!(f, "the migration's own `flags.{field}` override")
            }
            Self::ExecutorConfig(field) => write!(
                f,
                "the executor configuration `{field}` (a sub-millisecond duration \
                 truncates to zero whole milliseconds)"
            ),
        }
    }
}

/// Resolve a migration's effective timeout budget in milliseconds: its
/// per-migration `override_ms` when set, else the executor-wide `default_ms` --
/// refusing a zero from either source.
///
/// `setting` is the engine setting the budget feeds, and `flag_field` /
/// `config_field` name the two knobs, so the refusal points at the one that
/// actually produced the zero.
///
/// # Errors
/// [`IndefiniteTimeoutError`] when the resolved budget is `0`.
pub(crate) fn resolve_timeout_ms(
    version: &str,
    setting: &'static str,
    override_ms: Option<u64>,
    flag_field: &'static str,
    default_ms: u64,
    config_field: &'static str,
) -> Result<u64, IndefiniteTimeoutError> {
    let (ms, origin) = match override_ms {
        Some(ms) => (ms, TimeoutOrigin::MigrationFlag(flag_field)),
        None => (default_ms, TimeoutOrigin::ExecutorConfig(config_field)),
    };
    if ms == 0 {
        return Err(IndefiniteTimeoutError {
            version: version.to_string(),
            setting,
            origin,
        });
    }
    Ok(ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_finite_override_wins_over_the_executor_default() {
        let ms = resolve_timeout_ms(
            "mig_x",
            "lock_timeout",
            Some(7_000),
            "lock_timeout_ms",
            3_000,
            "pg.lock_timeout",
        )
        .expect("a finite override resolves");
        assert_eq!(ms, 7_000);
    }

    #[test]
    fn a_zero_override_is_refused_and_names_the_migration_flag() {
        let err = resolve_timeout_ms(
            "mig_x",
            "lock_timeout",
            Some(0),
            "lock_timeout_ms",
            3_000,
            "pg.lock_timeout",
        )
        .expect_err("a zero override is refused");
        assert_eq!(err.origin, TimeoutOrigin::MigrationFlag("lock_timeout_ms"));
        let text = err.to_string();
        assert!(text.contains("lock_timeout = 0"), "{text}");
        assert!(text.contains("flags.lock_timeout_ms"), "{text}");
    }

    #[test]
    fn a_zero_executor_default_is_refused_and_names_the_config_field() {
        let err = resolve_timeout_ms(
            "mig_x",
            "statement_timeout",
            None,
            "timeout_ms",
            0,
            "pg.statement_timeout",
        )
        .expect_err("a zero executor default is refused");
        assert_eq!(
            err.origin,
            TimeoutOrigin::ExecutorConfig("pg.statement_timeout")
        );
        assert!(err.to_string().contains("pg.statement_timeout"), "{err}");
    }

    #[test]
    fn the_smallest_expressible_budget_is_still_finite() {
        assert_eq!(
            resolve_timeout_ms(
                "mig_x",
                "lock_timeout",
                Some(1),
                "lock_timeout_ms",
                3_000,
                "pg.lock_timeout",
            )
            .expect("1ms is a real budget"),
            1
        );
    }
}
