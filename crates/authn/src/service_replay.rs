//! The Postgres-backed `jti` single-use cache for service assertions.
//!
//! The trait, the profile, and the in-memory implementation live in
//! `zeroship_core::service_assertion`. This is the arm a replicated callee
//! runs, and it lives here rather than in `zeroship-core` because it needs
//! `compio-postgres` and `zeroship-core` is the leaf every binary links.
//!
//! # Why a table and not a process-local cache
//!
//! "Single use" that is only single use per replica is no defence: an attacker
//! replaying a captured assertion retries until they land on a different
//! replica. The claim therefore has to be settled by something all replicas
//! share, and this deployment already has exactly one such thing.
//!
//! # Why this statement is atomic
//!
//! `INSERT ... ON CONFLICT DO UPDATE ... WHERE` is settled inside one
//! statement by Postgres, which takes a row lock on the conflicting row before
//! evaluating the `WHERE`. Two concurrent claims of one key therefore serialise
//! and exactly one reports a row affected. The obvious alternative - `SELECT`
//! then `INSERT` - is a race a concurrent attacker wins outright: both
//! transactions read "absent", both insert, both succeed.
//!
//! The `WHERE` arm is what lets an EXPIRED row be reclaimed without a separate
//! delete, so a lagging sweeper cannot lock a key out forever. It compares
//! against the DATABASE's clock, not the caller's, which is also what makes the
//! comparison consistent across replicas whose clocks differ.
//!
//! [`PostgresReplayStore::purge_expired`] is a housekeeping sweep, not a
//! correctness requirement: rows past `expires_at` are already reclaimable.

use std::time::{SystemTime, UNIX_EPOCH};

use compio_postgres::GenericClient;
use zeroship_core::service_assertion::{ClaimFuture, ReplayClaim, ReplayStore, ReplayStoreError};

/// The one statement the store issues.
///
/// `RETURNING` is deliberately absent: the affected-row count already carries
/// the verdict, so there is nothing to return.
///
/// That is NOT a statement about privileges, and an earlier version of this
/// comment said it was. The statement reads `expires_at` in the `DO UPDATE`
/// arm's `WHERE`, and `PostgreSQL` requires `SELECT` on every column read in a
/// condition whether or not a row is returned. It needs select, insert and
/// update; the sweep below needs select and delete. All four are granted in
/// `db/migrations-ts/20260816000100_service_assertion_replay.ts`.
const CLAIM_SQL: &str = "INSERT INTO zeroship.service_assertion_replay (replay_key, expires_at) \
     VALUES ($1, to_timestamp($2::double precision)) \
     ON CONFLICT (replay_key) DO UPDATE SET expires_at = excluded.expires_at \
     WHERE service_assertion_replay.expires_at <= now()";

const PURGE_SQL: &str = "DELETE FROM zeroship.service_assertion_replay WHERE expires_at <= now()";

/// Render a driver error with the server's own words.
///
/// `compio_postgres::Error` displays a server-side failure as the bare string
/// `db error`; everything an operator would act on - the SQLSTATE and the
/// message - hangs off `as_db_error`. The verifier logs this string and then
/// fails closed, so an unrendered error means a total outage of inbound service
/// authentication whose log line names nothing. MEASURED: a claim issued by a
/// role without the table grant produced `replay store unavailable: db error`
/// before this, and `replay store unavailable: db error: permission denied for
/// table service_assertion_replay (42501)` after.
fn describe(error: &compio_postgres::Error) -> String {
    error.as_db_error().map_or_else(
        || error.to_string(),
        |db| format!("{error}: {} ({})", db.message(), db.code().code()),
    )
}

/// A [`ReplayStore`] backed by `zeroship.service_assertion_replay`.
///
/// The table is established by
/// `db/migrations-ts/20260816000100_service_assertion_replay.ts`. This type
/// never creates it: a service that could `CREATE TABLE` would hold a privilege
/// the role split exists to withhold.
#[derive(Debug)]
pub struct PostgresReplayStore<C> {
    client: C,
}

impl<C> PostgresReplayStore<C> {
    /// Wrap a client that can reach the platform schema.
    pub const fn new(client: C) -> Self {
        Self { client }
    }
}

impl<C: GenericClient + Sync> PostgresReplayStore<C> {
    /// Delete every claim whose retention window has passed.
    ///
    /// Housekeeping only. Correctness does not depend on it: an expired row is
    /// reclaimed by the next claim of the same key.
    ///
    /// # Errors
    ///
    /// Returns [`ReplayStoreError`] when the statement could not be executed.
    #[allow(clippy::future_not_send)]
    pub async fn purge_expired(&self) -> Result<u64, ReplayStoreError> {
        self.client
            .execute(PURGE_SQL, &[])
            .await
            .map_err(|error| ReplayStoreError(describe(&error)))
    }
}

impl<C: GenericClient + Sync> ReplayStore for PostgresReplayStore<C> {
    fn claim<'a>(&'a self, key: &'a str, expires_at: SystemTime) -> ClaimFuture<'a> {
        Box::pin(async move {
            // The absolute instant travels as epoch seconds and is turned into
            // a timestamptz by the SERVER, so the stored value does not depend
            // on how this client's driver renders a local timestamp.
            let epoch_seconds = expires_at
                .duration_since(UNIX_EPOCH)
                .map_err(|_| ReplayStoreError("retention instant precedes the epoch".to_owned()))?
                .as_secs_f64();
            let affected = self
                .client
                .execute(CLAIM_SQL, &[&key, &epoch_seconds])
                .await
                .map_err(|error| ReplayStoreError(describe(&error)))?;
            // One row means this caller inserted the key, or reclaimed a row
            // whose window had already closed. Zero means a live claim is
            // already held, by someone else or by an earlier presentation of
            // the same assertion.
            Ok(if affected == 1 {
                ReplayClaim::Accepted
            } else {
                ReplayClaim::AlreadyUsed
            })
        })
    }
}
