//! The cross-run, cross-binary claim on a PostgreSQL EXTENSION.
//!
//! WHY THIS EXISTS. A PostgreSQL extension is installed per DATABASE, not per
//! schema. Every other cluster-visible name the live suites touch - schemas, roles,
//! project ids - carries this process's pid, so two runs against the one shared
//! server never meet. An extension has no such freedom: its name is not an
//! identifier the test picks, it is a lookup into the server's installed extension
//! library, so `citext_<pid>` is not an isolated extension, it is `could not open
//! extension control file`. Isolation in SPACE is unavailable, so the claimants
//! isolate in TIME.
//!
//! MEASURED, twice, from separate concurrent gate runs sharing 127.0.0.1:5434:
//!
//! ```text
//!   rollback-live.test.ts:403   extension "citext" is already installed in this database
//!   test 385                    extension "citext" does not exist
//! ```
//!
//! Both name a real test and read as a product defect. Neither is one - a sibling
//! run created or removed a database-global object between this run's two
//! statements. The second line is the sharper of the two: the referent went missing
//! because ANOTHER RUN removed it, which is exactly the confusion a live suite must
//! not manufacture.
//!
//! KEYED BY THE RESOURCE, NOT BY THE SUITE. [`claim_key`] hashes
//! `zero-migrate:pg-extension:<name>` and NOTHING ELSE - no suite name, no binary
//! name, no pid. That is the whole point and the easiest thing to get subtly wrong:
//! `dialect_matrix` and `rollback` both install `pgcrypto`, and two locks with
//! different keys protect nothing at all while looking exactly like protection.
//! `extension_claim_is_exclusive.rs` pins the property.
//!
//! The key is hashed by the SERVER (`hashtext`) the way `apply::executor` hashes a
//! project id for its own lock, so a reader who knows one knows the other. Both live
//! in one 64-bit advisory space, and a collision between this key and some project's
//! could only make one of them WAIT, never mis-serialize: advisory locks are
//! re-entrant per session, so the executor's own acquire inside a claimed body is
//! satisfied at once.
//!
//! RELEASE ON EVERY PATH. The claim is a SESSION-level advisory lock on
//! [`PgDevSession`]'s ONE pinned connection, so the server releases it when that
//! connection closes - which covers an early return, a panic AND a killed process,
//! the third of which no `Drop` impl reaches. [`release`] exists so the claim ends at
//! the CASE boundary rather than at the end of the test's scope, and so the next
//! claimant is not made to wait on a session that is already finished with it.
//!
//! A FAILED CLAIM IS LOUD. [`claim`] returns `Err`, never a skip. A caller that
//! turned a lost claim into a quiet pass would have a test that reports green
//! without asking its question, which this project treats as a defect in itself.

use zero_migrate::driver::{Bind, SqlSession};

use super::PgDevSession;

/// How long a run waits for a claim before it REPORTS rather than hangs.
///
/// Spelled as a `lock_timeout`, which PostgreSQL applies to a `pg_advisory_lock`
/// wait - VERIFIED on the 18.4 instance these suites run against: a second session
/// waiting on a held key aborts with `canceling statement due to lock timeout` at the
/// bound rather than waiting forever. The claimed span of any one case is a handful
/// of statements, so this bound is reached by a WEDGED holder, not by a queue.
pub const CLAIM_WAIT: &str = "180s";

/// The advisory-lock key for one extension, keyed by the RESOURCE alone.
///
/// Every claimant of `citext` - in whatever test binary - must hash to this same
/// string, or the claim serializes a suite against itself and nothing against its
/// siblings.
#[must_use]
pub fn claim_key(extension: &str) -> String {
    format!("zero-migrate:pg-extension:{extension}")
}

fn quoted(extension: &str) -> String {
    format!("\"{}\"", extension.replace('"', "\"\""))
}

/// Take the cross-run claim on `extension`, and start it from a known state.
///
/// The `DROP EXTENSION IF EXISTS` here is INSIDE the claim and is a fixture
/// precondition: a run killed between its CREATE and its DROP leaves the extension
/// installed, and the next claimant's `CREATE EXTENSION` would then answer `already
/// installed in this database` - an answer about the leftover, not about the
/// declaration under test. The identical statement OUTSIDE the claim is the race
/// itself.
///
/// # Errors
/// Returns the reason the claim was not obtained. Every caller must make that LOUD.
pub async fn claim(session: &PgDevSession, extension: &str) -> Result<(), String> {
    claim_for(session, extension, CLAIM_WAIT).await
}

/// [`claim`] with an explicit bound, so the timeout path itself can be tested.
///
/// # Errors
/// Returns the reason the claim was not obtained.
pub async fn claim_for(session: &PgDevSession, extension: &str, wait: &str) -> Result<(), String> {
    let key = claim_key(extension);
    if let Err(error) = session.batch(&format!("SET lock_timeout = '{wait}'")).await {
        return Err(format!(
            "could not bound the wait for the {extension} claim: {error}"
        ));
    }
    let taken = session
        .exec(
            "SELECT pg_advisory_lock(hashtext($1)::bigint)",
            &[Bind::Text(key.clone())],
        )
        .await;
    // RESET before anything else runs on this PINNED connection. The bound belongs to
    // the claim; left armed it would abort the executor's OWN `pg_advisory_lock` wait
    // under load, and the case would be blamed for a server error that is this
    // function's.
    let _ = session.batch("RESET lock_timeout").await;
    if let Err(error) = taken {
        return Err(format!(
            "waited {wait} for the {extension} claim ({key}) and did not get it, so \
             this case never asked its question: {error}"
        ));
    }
    if let Err(error) = session
        .batch(&format!("DROP EXTENSION IF EXISTS {}", quoted(extension)))
        .await
    {
        return Err(format!(
            "holds the {extension} claim but could not clear a leftover installation \
             before asking the case: {error}"
        ));
    }
    Ok(())
}

/// Drop what the case installed under the claim, then release the claim.
///
/// Not a `Drop` guard, and it does not need to be - see the module doc: the pinned
/// connection closing is the backstop that covers the killed process too.
pub async fn release(session: &PgDevSession, extension: &str) {
    let _ = session
        .batch(&format!("DROP EXTENSION IF EXISTS {}", quoted(extension)))
        .await;
    let _ = session
        .exec(
            "SELECT pg_advisory_unlock(hashtext($1)::bigint)",
            &[Bind::Text(claim_key(extension))],
        )
        .await;
}
