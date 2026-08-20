//! The extension claim excludes a second run, is keyed by the resource alone, and says so when it loses.
//!
//! `support::extension_claim` is the reason `drop_extension_rollback_pg.rs` and
//! `fold_live/fold_role_extension_pg.rs` can install a DATABASE-GLOBAL object while
//! a sibling gate run does the same. A claim nobody has watched fail is a green
//! light with no bulb, so the three properties it rests on are asserted here rather
//! than assumed:
//!
//!   1. THE KEY NAMES THE RESOURCE. `dialect_matrix` installs `pgcrypto` and so does
//!      `drop_extension_rollback_pg.rs`; `fold_live` installs `unaccent`. Two locks
//!      with different keys protect nothing while looking exactly like protection,
//!      and the first version of this claim was keyed by the SUITE - which is how a
//!      neighbour's installation came to be reported as a defect in a conformance
//!      row. So: the key for an extension is a function of the extension NAME and
//!      nothing else, distinct per extension, and carries no suite, binary or pid.
//!   2. IT ACTUALLY EXCLUDES. While one session holds the claim, a second cannot
//!      take it, and once the first releases, the second can. The contender uses a
//!      raw `pg_try_advisory_lock` rather than `claim`, because a WAIT and a REFUSAL
//!      are indistinguishable from a test that only ever waits: the try answers
//!      `false` at once and that false is the observable. This is the half that goes
//!      red if the acquire inside `claim` is ever reduced to a no-op.
//!   3. LOSING IS LOUD. A claim that cannot be obtained inside its bound returns
//!      `Err` naming the extension and the key. It is not a skip, and it does not
//!      hang - which asserts the bound is a real `lock_timeout` rather than
//!      decoration, by wedging a key from another session and asking for it with a
//!      short bound.
//!
//! WHY THE LIVE HALVES LOCK A NAME THAT IS NOT AN EXTENSION. The key is a string the
//! server hashes; nothing about `pg_advisory_lock` requires the name to resolve to an
//! installed extension, and `DROP EXTENSION IF EXISTS` on an unknown name is a
//! notice. Locking the REAL `citext` here would have this file contend with
//! `drop_extension_rollback_pg.rs` in this same binary, where the wedge in (3) - a
//! `pg_try_advisory_lock` that MUST succeed for the test to be about anything - would
//! answer `false` whenever that case happened to hold it, and the guard would go red
//! for a scheduling accident. The probe carries this process's pid for the same
//! reason one binary down: two concurrent runs of this binary must not wedge each
//! other. The shared-key property those probes give up is exactly what (1) asserts
//! directly, on the real names.
//!
//! The live halves are GATED behind `ZERO_MIGRATE_TEST_PG_URL`; property (1) needs no
//! server and runs unconditionally.

use crate::support::extension_claim::{claim, claim_for, claim_key, release};
use crate::support::PgDevSession;
use zero_migrate::driver::{Bind, SqlSession};

/// A lock name of this run's own, distinct per case. See the module doc.
fn probe(tag: &str) -> String {
    format!("zm_claim_probe_{tag}_{}", std::process::id())
}

async fn try_take(session: &PgDevSession, name: &str) -> bool {
    session
        .query_one(
            "SELECT pg_try_advisory_lock(hashtext($1)::bigint) AS got",
            &[Bind::Text(claim_key(name))],
        )
        .await
        .expect("ask the server for the claim")
        .try_get::<_, bool>("got")
        .expect("decode the try-lock answer")
}

#[compio::test]
async fn the_extension_claim_key_names_the_extension_and_nothing_else() {
    assert_eq!(
        claim_key("citext"),
        "zero-migrate:pg-extension:citext",
        "the key is the contract BETWEEN BINARIES, not a private detail: changing \
         its shape unshares the claim, and an unshared claim is indistinguishable \
         from no claim until two suites meet on one server"
    );
    assert_ne!(
        claim_key("citext"),
        claim_key("pgcrypto"),
        "two extensions are two resources; one key for both would serialize cases \
         that never contend"
    );
    for key in [
        claim_key("citext"),
        claim_key("pgcrypto"),
        claim_key("unaccent"),
    ] {
        for claimant in [
            "dialect",
            "conformance",
            "rollback",
            "fold",
            "matrix",
            "test",
        ] {
            assert!(
                !key.contains(claimant),
                "the key must name the RESOURCE, not the claimant: {key} contains \
                 {claimant}, which would give each suite a private lock and protect \
                 nothing"
            );
        }
        assert!(
            !key.contains(&std::process::id().to_string()),
            "a per-process key would make every run its own sole claimant: {key}"
        );
    }
}

#[compio::test]
async fn a_held_extension_claim_excludes_a_second_run_and_is_released_for_the_next() {
    let url = skip_if_no_pg!();
    let name = probe("exclusion");
    let holder = PgDevSession::connect(&url);
    let contender = PgDevSession::connect(&url);

    claim(&holder, &name)
        .await
        .expect("the holder takes the extension claim");

    assert!(
        !try_take(&contender, &name).await,
        "a second run must NOT be able to take the {name} claim while a first holds \
         it. Without this exclusion both runs install the extension and one drops it \
         under the other, which is `already exists` in one process and `does not \
         exist` in the other - the two failures this claim was written for"
    );

    release(&holder, &name).await;

    claim(&contender, &name)
        .await
        .expect("a released claim is obtainable by the next run");
    release(&contender, &name).await;
}

#[compio::test]
async fn an_unobtainable_extension_claim_is_an_error_that_names_the_key() {
    let url = skip_if_no_pg!();
    let name = probe("timeout");
    let wedged = PgDevSession::connect(&url);
    let loser = PgDevSession::connect(&url);

    // Wedge the key from a session that will not give it back until this test is done
    // with it. Taken with the raw verb rather than `claim`, so nothing here depends on
    // the function under test.
    assert!(
        try_take(&wedged, &name).await,
        "the wedge itself has to succeed, or the rest of this test is vacuous"
    );

    let refusal = claim_for(&loser, &name, "300ms")
        .await
        .expect_err("a claim on a wedged key must not be obtained");
    assert!(
        refusal.contains(&name) && refusal.contains(&claim_key(&name)),
        "the failure has to name the extension and the key an operator would look \
         for, got: {refusal}"
    );
    assert!(
        refusal.contains("never asked its question"),
        "a lost claim means the case did not run, which the message must SAY rather \
         than let it read as a pass: {refusal}"
    );

    let _ = wedged
        .exec(
            "SELECT pg_advisory_unlock(hashtext($1)::bigint)",
            &[Bind::Text(claim_key(&name))],
        )
        .await;
    // The loser holds nothing and has no `lock_timeout` left armed - `claim_for`
    // resets it on every path. Assert the first half explicitly, so a future change
    // that left a failed acquire half-held is caught here rather than as a hang in an
    // unrelated suite.
    assert!(
        try_take(&loser, &name).await,
        "once the wedge lets go the key must be obtainable; if it is not, the failed \
         claim left a lock behind"
    );
    release(&loser, &name).await;
}
