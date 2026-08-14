//! The anchor regression, against a REAL PostgreSQL server.
//!
//! A creator deploys an `env.db` app and skips `zeroship migrate`. The
//! per-app role was never created, so `SET LOCAL ROLE "app_<id>_role"` in
//! the autocommit session setup (`plugin_db::exec`) refuses before any
//! creator SQL runs. Before this test's fix, `DbError::from_pg` had no arm
//! for that SQLSTATE, fell through to `DbError::Internal`, stamped the code
//! `internal` -- which is on no allow-list -- and the runtime's 5xx rail
//! correctly blanked it to `{"message":"internal error"}`. The sanitiser
//! did its job; the classifier lied to it.
//!
//! This test exists because the discriminator's unit tests in
//! `src/error.rs` CANNOT reach `from_pg`: `compio_postgres::Error` has no
//! public constructor, so nothing in-process can synthesise the server
//! error. Only a live server produces it. That also makes this the only
//! test that would fail if the arm were deleted from `from_pg` or reordered
//! after the catch-all.
//!
//! Requires: PostgreSQL at `PG_TEST_URL` (default port 5434).
//! Run: `cargo test -p zeroship-plugin-db --test missing_role \
//!       --features test-helpers -- --test-threads=1`
//!
//! WHAT THIS TEST DOES NOT CATCH:
//!   - The HTTP boundary. It asserts the classification and the message
//!     plugin-db produces, not that the runtime rail lets it through --
//!     that is `crates/runtime/src/core/dispatch.rs`'s
//!     `schema_not_provisioned_survives_the_5xx_rail_in_both_spellings`
//!     and its one-variable control.
//!   - A non-English server. `is_missing_role` matches PostgreSQL's
//!     English primary message; under a translated `lc_messages` this
//!     test's case would classify `internal` again, and so would
//!     production. The failure mode is a false NEGATIVE (today's
//!     behaviour), never a false positive.
//!   - Any role-missing path that does not go through `SET LOCAL ROLE`.
//!     42704 and 28000 are covered by reasoning and by the unit tests,
//!     not by a live server here.

use compio_postgres::NoTls;
use zeroship_plugin_db::error::DbError;

fn test_url() -> String {
    zeroship_core::test_env!("PG_TEST_URL")
        .unwrap_or_else(|| "postgres://postgres:test@localhost:5434/postgres".to_string())
}

/// Drive a real `SET LOCAL ROLE` against a role that does not exist and
/// hand the resulting server error to the classifier.
async fn classify_missing_role(role: &str) -> DbError {
    let url = test_url();
    let (client, connection) = compio_postgres::connect(&url, NoTls)
        .await
        .unwrap_or_else(|e| panic!("live-Postgres test requires a server at PG_TEST_URL: {e}"));
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();

    // Same shape as `exec::query_postgres_pool_with_autocommit_role`: the
    // SET LOCAL runs inside an explicit transaction, so the failure is the
    // one a creator's first `env.db` call actually hits.
    let err = client
        .simple_query(&format!("BEGIN; SET LOCAL ROLE \"{role}\""))
        .await
        .expect_err("SET LOCAL ROLE to a nonexistent role must fail");

    DbError::from_pg(&err)
}

#[compio::test]
async fn missing_per_app_role_is_creator_facing_not_internal() {
    // A name no cluster will have. Shaped like `per_app_role_name` output
    // so the case is the production one.
    let role = format!("app_{}_role", uuid::Uuid::new_v4().simple());
    let classified = classify_missing_role(&role).await;

    let op = classified.to_op_error();
    let code = match &op.kind {
        zeroship_runtime::state::OpErrorKind::CodedError { code, .. } => code.clone(),
        other => panic!("expected CodedError, got {other:?}"),
    };

    assert_eq!(
        code, "schema_not_provisioned",
        "a missing per-app role is a creator CONFIGURATION state with a documented \
         one-command fix, not an internal fault. Got {code:?} with message {:?}",
        op.message
    );

    // THE ACCEPTANCE BAR: the response names the command.
    assert!(
        op.message.contains("zeroship migrate"),
        "the creator must learn what to run from this message: {:?}",
        op.message
    );

    // And it names nothing else. The message is the platform CONSTANT --
    // not a composition -- so nothing from the server can appear in it.
    // Asserting equality rather than absence is the stronger form: an
    // absence list can only rule out the leaks someone thought of.
    assert_eq!(
        op.message,
        zeroship_plugin_db::error::MISSING_ROLE_MESSAGE,
        "the wire message must be the fixed platform constant"
    );

    // The three specific things that WOULD have ridden out on the old
    // `Internal` path, spelled out so a future edit that starts composing
    // the message fails here with a readable reason rather than only on
    // the equality above. The role name embeds the app id; `ERROR:` is
    // `compio_postgres::DbError`'s severity prefix; `caused by` is
    // `walk_pg_chain`'s source-chain joiner.
    assert!(
        !op.message.contains(&role),
        "the role name (which embeds the app id) must not ride out: {:?}",
        op.message
    );
    assert!(
        !op.message.contains("ERROR:"),
        "server severity prefix must not ride out: {:?}",
        op.message
    );
    assert!(
        !op.message.contains("caused by"),
        "the walked source chain must not ride out: {:?}",
        op.message
    );
    assert!(op.message.is_ascii(), "ASCII only: {:?}", op.message);
}

#[compio::test]
async fn a_real_internal_pg_failure_is_still_internal() {
    // ONE-VARIABLE CONTROL, run against the SAME live server through the
    // SAME classifier: a statement that fails for a reason the creator
    // cannot fix with `zeroship migrate`. If the new arm had widened into
    // "any configuration-shaped SQLSTATE is creator-facing", this would
    // come back `schema_not_provisioned` too.
    //
    // 22023 deliberately -- the SAME SQLSTATE the missing-role case
    // reports. The only thing separating the two is the server's primary
    // message, so this proves the discriminator narrows rather than
    // rubber-stamping the SQLSTATE.
    let url = test_url();
    let (client, connection) = compio_postgres::connect(&url, NoTls)
        .await
        .unwrap_or_else(|e| panic!("live-Postgres test requires a server at PG_TEST_URL: {e}"));
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();

    let err = client
        .simple_query("BEGIN; SET LOCAL statement_timeout = 'not-a-duration'")
        .await
        .expect_err("a bad GUC value must fail");

    // Confirm the control really is the same SQLSTATE, so a future server
    // version that changes it turns this into a visible failure rather
    // than a silently weaker control.
    assert_eq!(
        err.code().map(|c| c.code().to_string()).as_deref(),
        Some("22023"),
        "control must share the SQLSTATE of the case it controls for"
    );

    let op = DbError::from_pg(&err).to_op_error();
    match &op.kind {
        zeroship_runtime::state::OpErrorKind::CodedError { code, .. } => {
            assert_eq!(
                code, "internal",
                "a genuinely-internal 22023 must stay `internal` and be blanked at \
                 the rail; got {code:?}"
            );
        }
        other => panic!("expected CodedError, got {other:?}"),
    }
}
