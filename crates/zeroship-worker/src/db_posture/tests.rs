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
        reaches_platform_schema: false,
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
    // Isolated: this reads zeroship_worker's own cluster-wide membership count,
    // which the shared server's other worker tests must never be mutating
    // concurrently (see `Database::isolated`'s doc).
    Database::isolated(async |database| {
        validate_database_url(database.url_as(WORKER_DATABASE_ROLE).as_str())
            .await
            .expect("the migrated worker role must satisfy the production boot gate");
    })
    .await;
}

/// Every boot refusal carries the server's own reason, whichever of the three
/// exchanges the server refused. The driver's error displays only its kind, so
/// a refusal built from it alone reads "db error" whatever the server said, and
/// a connection budget other services had exhausted read as an unexplained
/// authority failure.
#[compio::test]
async fn every_refused_exchange_carries_the_servers_reason() {
    // THE CONTROL: with nothing refused, both queries run and the verdict is
    // the posture's own, so each arm below is refused by its setup alone.
    // `{role}` is substituted with this case's own minted login: a fixed
    // `fixture_login` would be a cluster-global name every worker test
    // process of the shared server could collide on.
    let arms = [
        (
            None,
            "worker database login must be zeroship_worker",
            "got {role}",
        ),
        (
            Some("ALTER ROLE \"{role}\" CONNECTION LIMIT 0"),
            "connect to inspect worker database role: ",
            "too many connections for role \"{role}\"",
        ),
        (
            Some("REVOKE SELECT ON pg_catalog.pg_roles FROM PUBLIC"),
            "inspect worker database role: ",
            "permission denied for view pg_roles",
        ),
        (
            Some("REVOKE SELECT ON pg_catalog.pg_auth_members FROM PUBLIC"),
            "inspect worker role memberships: ",
            "permission denied for table pg_auth_members",
        ),
    ];
    // Every arm runs before any is judged, so one verdict covers all of them.
    let mut mismatched = Vec::new();
    for (setup, step, reason) in arms {
        Database::run(async |database| {
            let role = database.mint_role("fixture_login");
            let substitute = |template: &str| template.replace("{role}", &role);
            database
                .admin
                .batch_execute(&format!("CREATE ROLE \"{role}\" LOGIN PASSWORD '{role}'"))
                .await
                .unwrap();
            if let Some(setup) = setup {
                database
                    .admin
                    .batch_execute(&substitute(setup))
                    .await
                    .unwrap();
            }
            let error = validate_database_url(database.url_as(&role).as_str())
                .await
                .expect_err("a freshly minted login is never the worker's posture");
            let step = substitute(step);
            let reason = substitute(reason);
            if !(error.starts_with(&step) && error.contains(&reason)) {
                mismatched.push(format!(
                    "expected {step:?} carrying {reason:?}, got: {error}"
                ));
            }
        })
        .await;
    }
    assert!(mismatched.is_empty(), "{mismatched:#?}");
}

/// Carrying the whole chain never carries the credential: a URL the driver
/// cannot read is refused by the component it names, not by what it holds.
/// Nothing is dialled, so no server is needed.
#[compio::test]
async fn a_refused_url_names_the_password_without_quoting_it() {
    for url in [
        "postgres://zeroship_worker:hunter2secret%zz@127.0.0.1:1/db",
        "postgres://zeroship_worker:hunter2secret%00@127.0.0.1:1/db",
    ] {
        let error = validate_database_url(url)
            .await
            .expect_err("a malformed password is refused before any dial");
        assert!(
            error.starts_with("connect to inspect worker database role: ")
                && error.contains("the password")
                && !error.contains("hunter2secret"),
            "{error}"
        );
    }
}

/// The worker connects to the CREATOR database. A login that can resolve the
/// platform schema at all is pointed at Control's database, which is the zone
/// split this gate exists to hold.
#[test]
fn refuses_a_login_that_can_reach_the_platform_schema() {
    let mut posture = narrow_posture();
    posture.reaches_platform_schema = true;

    let error = validate(&posture).expect_err("platform reach must be refused");
    assert!(
        error.contains("platform schema"),
        "unexpected error: {error}"
    );

    posture.reaches_platform_schema = false;
    validate(&posture).expect("a creator-database login is the accepted posture");
}

#[test]
fn accepts_only_the_named_narrow_worker_posture() {
    validate(&narrow_posture()).expect("narrow worker posture should be accepted");
}

/// A binding role granted to the worker without `WITH INHERIT FALSE`, by any
/// grantor, makes that binding's database privileges ambient on the shared
/// login. Boot must refuse rather than serve requests from a login whose
/// ambient authority reaches a tenant.
#[test]
fn refuses_a_login_that_ambiently_inherits_a_binding_role() {
    // An arbitrary count: this case pins that whatever number the query
    // returns reaches the operator's error, not that any particular database
    // has that many.
    const OFFENDING: i64 = 7;
    let mut posture = narrow_posture();
    posture.inheriting_memberships = OFFENDING;
    posture.inheriting_membership_example =
        Some("zs_bind_bnd_03coc2qj4x2ae61h80zwlnnq6".to_string());

    let error = validate(&posture).expect_err("an inheriting binding grant must be refused");
    assert!(
        error.contains("WITH INHERIT FALSE"),
        "the error must name the fix: {error}"
    );
    assert!(
        error.contains(&OFFENDING.to_string())
            && error.contains("zs_bind_bnd_03coc2qj4x2ae61h80zwlnnq6"),
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
