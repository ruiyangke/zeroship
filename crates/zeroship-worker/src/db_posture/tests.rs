use super::*;
use crate::test_database::Database;

fn narrow_posture() -> DatabasePosture {
    DatabasePosture {
        current_user: "zeroship_worker".to_string(),
        superuser: false,
        create_role: false,
        create_db: false,
        replication: false,
        bypass_rls: false,
        workflow_owner_member: true,
        creates_in_system_schema: false,
        system_table_writes: false,
        system_column_writes: false,
        system_sequence_writes: false,
        unexpected_system_reads: false,
        required_system_reads: true,
        inheriting_memberships: 0,
        inheriting_membership_example: None,
    }
}

#[test]
fn rejects_replication_and_rls_bypass_independently() {
    let mut posture = narrow_posture();
    posture.replication = true;
    assert!(validate(&posture).unwrap_err().contains("NOREPLICATION"));
    posture.replication = false;
    posture.bypass_rls = true;
    assert!(validate(&posture).unwrap_err().contains("NOBYPASSRLS"));
    posture.bypass_rls = false;
    assert!(validate(&posture).is_ok());
}

#[compio::test]
async fn worker_boot_accepts_the_migrated_role_without_replication() {
    Database::migrated(async |database| {
        validate_database_url(database.url_as(WORKER_DATABASE_ROLE).as_str())
            .await
            .expect("the migrated worker role must satisfy the production boot gate");
    })
    .await;
}

#[test]
fn rejects_any_effective_write_to_the_platform_schema() {
    let mut posture = narrow_posture();
    posture.system_table_writes = true;

    let error = validate(&posture).expect_err("system write privilege must be refused");
    assert!(
        error.contains("write privilege"),
        "unexpected error: {error}"
    );
}

#[test]
fn accepts_only_the_named_narrow_worker_posture() {
    validate(&narrow_posture()).expect("narrow worker posture should be accepted");
}

/// A database provisioned before `runtime_dependents_sql` carried
/// `WITH INHERIT FALSE` leaves the worker inheriting every app role it has
/// ever been granted. Boot must refuse rather than serve requests from a
/// login whose ambient authority is the union of every tenant.
#[test]
fn refuses_a_login_that_ambiently_inherits_an_app_role() {
    // An arbitrary count, deliberately NOT the dev database's measured one:
    // this case pins that whatever number the query returns reaches the
    // operator's error, not that any particular database has that many.
    const OFFENDING: i64 = 7;
    let mut posture = narrow_posture();
    posture.inheriting_memberships = OFFENDING;
    posture.inheriting_membership_example =
        Some("app_0191e7a2-b3c4-4d5e-8f90-123456789abc_role".to_string());

    let error = validate(&posture).expect_err("an inheriting app-role grant must be refused");
    assert!(
        error.contains("WITH INHERIT FALSE"),
        "the error must name the fix: {error}"
    );
    assert!(
        error.contains(&OFFENDING.to_string()) && error.contains("app_0191e7a2"),
        "the error must carry the count and an offending role: {error}"
    );
}

/// THE CONTROL, differing in one variable. `narrow_posture` already sets
/// the count to zero, so the case above could pass because of any other
/// arm; this pins that a SINGLE inheriting membership is what flips it, and
/// that the arm is a `> 0` count rather than a threshold.
#[test]
fn one_inheriting_membership_is_enough_to_refuse_boot() {
    let mut posture = narrow_posture();
    posture.inheriting_memberships = 1;
    assert!(
        validate(&posture).is_err(),
        "one inheriting membership is one tenant too many"
    );

    posture.inheriting_memberships = 0;
    validate(&posture).expect("zero inheriting memberships is the accepted posture");
}

mod memberships;
