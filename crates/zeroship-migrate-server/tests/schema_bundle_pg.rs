//! What a schema bundle must do, against a real PostgreSQL.
//!
//! The bundle path exists to install and UPGRADE a platform-owned schema inside
//! a creator database. Two of its properties are the ones a mistake would be
//! expensive in, and both are exercised here rather than argued for:
//!
//!   - an upgrade that fails part way leaves the stamp at the version that is
//!     actually installed, because the whole upgrade is one transaction;
//!   - a bundle BEHIND an installed schema is refused, because a newer platform
//!     provisioned it and a creator's schema must never be downgraded.

#![expect(
    clippy::future_not_send,
    reason = "the bundle applier and its fixtures stay on their compio runtime"
)]

use compio_postgres::{error::SqlState, Client};
use testcontainers::{
    core::{IntoContainerPort, WaitFor},
    runners::SyncRunner,
    Container, GenericImage, ImageExt,
};
use zeroship_core::database_role::per_app_role_name;
use zeroship_core::schema_bundle::{
    SchemaBundle, SchemaBundleAction, SchemaBundleVersion, SchemaStamp,
};
use zeroship_migrate_server::bundle::{apply_schema_bundle, BundleError};

/// The stamp table this fixture's bundle owns. It is NOT the workflow journal's:
/// the service must not need to know which bundle it is applying, and a fixture
/// borrowing the real one would hide a dependency on that.
const STAMP: &str = "__zeroship_probe_version";
const STATE: &str = "__zeroship_probe_state";

struct Fixture {
    dsn: String,
    admin: Client,
    _postgres: Container<GenericImage>,
}

impl Fixture {
    async fn start() -> Self {
        let postgres = GenericImage::new("postgres", "18")
            .with_exposed_port(5432.tcp())
            .with_wait_for(WaitFor::message_on_stdout(
                "PostgreSQL init process complete; ready for start up.",
            ))
            .with_wait_for(WaitFor::message_on_stderr(
                "database system is ready to accept connections",
            ))
            .with_env_var("POSTGRES_HOST_AUTH_METHOD", "trust")
            .start()
            .expect("schema bundle tests require Testcontainers PostgreSQL");
        let dsn = format!(
            "postgres://postgres@{}:{}/postgres",
            postgres.get_host().unwrap(),
            postgres.get_host_port_ipv4(5432).unwrap()
        );
        let admin = connect(&dsn).await;
        Self {
            dsn,
            admin,
            _postgres: postgres,
        }
    }

    async fn stamp(&self, schema: &str) -> Option<(i64, String)> {
        let rows = self
            .admin
            .query(
                &format!(
                    "SELECT version, fingerprint FROM \"{schema}\".\"{STAMP}\" WHERE id='probe'"
                ),
                &[],
            )
            .await
            .ok()?;
        let row = rows.first()?;
        Some((row.get(0), row.get(1)))
    }

    async fn column_exists(&self, schema: &str, column: &str) -> bool {
        self.admin
            .query_one(
                "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
                 WHERE table_schema=$1 AND table_name=$2 AND column_name=$3)",
                &[&schema, &STATE, &column],
            )
            .await
            .expect("probe for a column")
            .get(0)
    }
}

async fn connect(url: &str) -> Client {
    let (client, connection) = compio_postgres::connect(url, compio_postgres::NoTls)
        .await
        .expect("connect to the fixture database");
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    client
}

fn charter(schema: &str, destructive: &str) -> String {
    format!(
        r#"policy_version = 1
[[grant]]
key = "schema.create_table"
value = true
scope = {{ include = ["{schema}"] }}
[[grant]]
key = "schema.cross_schema"
value = true
scope = {{ include = ["{schema}"] }}
[[grant]]
key = "schema.rename"
value = true
scope = {{ include = ["{schema}"] }}
[[grant]]
key = "sql.raw"
value = true
scope = {{ include = ["{schema}"] }}
[[grant]]
key = "safety.destructive_ops"
value = "{destructive}"
scope = "all"
"#
    )
}

fn version_one(schema: &str) -> String {
    format!(
        "CREATE TABLE \"{schema}\".\"{STAMP}\" \
           (id text PRIMARY KEY, version bigint NOT NULL, fingerprint text NOT NULL);\n\
         CREATE TABLE \"{schema}\".\"{STATE}\" (id text PRIMARY KEY, note text NOT NULL);"
    )
}

fn version_two(schema: &str) -> String {
    format!("ALTER TABLE \"{schema}\".\"{STATE}\" ADD COLUMN extra text;")
}

fn bundle(schema: &str, version: u32, fingerprint: &str) -> SchemaBundle {
    let mut versions = vec![SchemaBundleVersion {
        version: 1,
        sql: version_one(schema),
    }];
    if version >= 2 {
        versions.push(SchemaBundleVersion {
            version: 2,
            sql: version_two(schema),
        });
    }
    SchemaBundle {
        bundle: "probe".into(),
        schema: schema.into(),
        dialect: "postgres".into(),
        version,
        fingerprint: fingerprint.into(),
        stamp: SchemaStamp {
            table: STAMP.into(),
            row_id: "probe".into(),
        },
        policy: charter(schema, "allow"),
        versions,
    }
}

fn schema_name(suffix: &str) -> String {
    format!("bundle_{suffix}_{}", uuid::Uuid::new_v4().simple())
}

/// What the SERVER said, because `compio_postgres::Error` prints as `db error`
/// and nothing else. A failure here is read by someone who was not present when
/// it was written, and the refusal reason is the whole of the diagnosis.
fn detail(error: &compio_postgres::Error) -> String {
    error
        .as_db_error()
        .map_or_else(|| error.to_string(), |db| db.message().to_owned())
}

const FP1: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const FP2: &str = "2222222222222222222222222222222222222222222222222222222222222222";

/// Install, then apply the SAME bundle again. The second call must change
/// nothing - not the schema, and above all not the ROWS, which is the property a
/// re-install would quietly destroy.
#[compio::test]
async fn a_bundle_applied_twice_leaves_the_schema_and_its_rows_alone() {
    let fixture = Fixture::start().await;
    let schema = schema_name("twice");

    let first = apply_schema_bundle(&fixture.dsn, &bundle(&schema, 1, FP1))
        .await
        .expect("install");
    assert_eq!(first.action, SchemaBundleAction::Installed);
    assert_eq!(first.version, 1);
    assert_eq!(fixture.stamp(&schema).await, Some((1, FP1.to_owned())));

    fixture
        .admin
        .execute(
            &format!("INSERT INTO \"{schema}\".\"{STATE}\" VALUES ('row', 'keep me')"),
            &[],
        )
        .await
        .expect("seed a row the second apply must not touch");

    let second = apply_schema_bundle(&fixture.dsn, &bundle(&schema, 1, FP1))
        .await
        .expect("re-apply");
    assert_eq!(second.action, SchemaBundleAction::Unchanged);
    let note: String = fixture
        .admin
        .query_one(
            &format!("SELECT note FROM \"{schema}\".\"{STATE}\" WHERE id='row'"),
            &[],
        )
        .await
        .expect("the seeded row survives")
        .get(0);
    assert_eq!(note, "keep me");
}

/// A bundle one version ahead upgrades an installed schema and re-stamps it.
#[compio::test]
async fn a_bundle_one_version_ahead_upgrades_and_restamps() {
    let fixture = Fixture::start().await;
    let schema = schema_name("upgrade");

    apply_schema_bundle(&fixture.dsn, &bundle(&schema, 1, FP1))
        .await
        .expect("install v1");
    fixture
        .admin
        .execute(
            &format!("INSERT INTO \"{schema}\".\"{STATE}\" VALUES ('row', 'survive')"),
            &[],
        )
        .await
        .expect("seed a row the upgrade must carry forward");
    assert!(!fixture.column_exists(&schema, "extra").await);

    let upgraded = apply_schema_bundle(&fixture.dsn, &bundle(&schema, 2, FP2))
        .await
        .expect("upgrade to v2");
    assert_eq!(upgraded.action, SchemaBundleAction::Upgraded);
    assert_eq!(upgraded.version, 2);
    assert_eq!(fixture.stamp(&schema).await, Some((2, FP2.to_owned())));
    assert!(fixture.column_exists(&schema, "extra").await);
    let note: String = fixture
        .admin
        .query_one(
            &format!("SELECT note FROM \"{schema}\".\"{STATE}\" WHERE id='row'"),
            &[],
        )
        .await
        .expect("the row survives the upgrade")
        .get(0);
    assert_eq!(note, "survive");
}

/// A bundle BEHIND an installed schema is refused, not applied. An older
/// platform must never downgrade a schema a newer one provisioned.
#[compio::test]
async fn a_bundle_behind_the_installed_schema_is_refused() {
    let fixture = Fixture::start().await;
    let schema = schema_name("behind");

    apply_schema_bundle(&fixture.dsn, &bundle(&schema, 2, FP2))
        .await
        .expect("install v2");
    assert!(fixture.column_exists(&schema, "extra").await);

    let error = apply_schema_bundle(&fixture.dsn, &bundle(&schema, 1, FP1))
        .await
        .expect_err("an older bundle must be refused");
    assert!(
        matches!(
            error,
            BundleError::Behind {
                installed: 2,
                offered: 1
            }
        ),
        "{error}"
    );
    assert_eq!(
        fixture.stamp(&schema).await,
        Some((2, FP2.to_owned())),
        "a refused downgrade must leave the stamp alone"
    );
    assert!(
        fixture.column_exists(&schema, "extra").await,
        "a refused downgrade must leave the schema alone"
    );
}

/// The stamp says this version, but the fingerprint does not match: the schema
/// is CORRUPTED, not out of date. Refuse rather than silently repair, so the
/// damage is reported rather than overwritten.
#[compio::test]
async fn a_fingerprint_mismatch_at_the_same_version_is_refused_as_corruption() {
    let fixture = Fixture::start().await;
    let schema = schema_name("corrupt");

    apply_schema_bundle(&fixture.dsn, &bundle(&schema, 1, FP1))
        .await
        .expect("install v1");

    let error = apply_schema_bundle(&fixture.dsn, &bundle(&schema, 1, FP2))
        .await
        .expect_err("a fingerprint mismatch must be refused");
    assert!(
        matches!(error, BundleError::Corrupt { version: 1, .. }),
        "{error}"
    );
    assert_eq!(
        fixture.stamp(&schema).await,
        Some((1, FP1.to_owned())),
        "a refused bundle must not re-stamp"
    );
}

/// An upgrade that fails PART WAY leaves the stamp at the old version AND undoes
/// the versions that had already succeeded, because the whole upgrade and its
/// stamp write are one transaction.
///
/// # What makes this bind the transaction rather than the stamp ordering
///
/// The upgrade spans TWO versions: v2 succeeds, v3 fails. An applier that
/// committed each version on its own would still leave the stamp at 1 - it never
/// reaches the stamp write - so a test that only checked the stamp would pass
/// against it. It is v2's column that separates the two: under one transaction it
/// is gone, and under per-version commits it survives. Measured: replacing the
/// single transaction with a commit per step leaves `extra` present here and
/// changes nothing else in this file.
///
/// The failure is deliberate and comes from the DATABASE, not the guard: v3 is
/// well-formed SQL naming the bound schema, so the guard admits it and PostgreSQL
/// rejects it at execution. That is the shape a real broken upgrade has.
#[compio::test]
async fn an_upgrade_that_fails_part_way_rolls_back_the_versions_before_it() {
    let fixture = Fixture::start().await;
    let schema = schema_name("partial");

    apply_schema_bundle(&fixture.dsn, &bundle(&schema, 1, FP1))
        .await
        .expect("install v1");

    let mut broken = bundle(&schema, 2, FP2);
    broken.version = 3;
    broken.versions.push(SchemaBundleVersion {
        version: 3,
        sql: format!("ALTER TABLE \"{schema}\".\"__zeroship_probe_absent\" ADD COLUMN late text;"),
    });
    let error = apply_schema_bundle(&fixture.dsn, &broken)
        .await
        .expect_err("a broken upgrade must fail");
    assert!(matches!(error, BundleError::Database(_)), "{error}");

    assert_eq!(
        fixture.stamp(&schema).await,
        Some((1, FP1.to_owned())),
        "the stamp must still name the version that is actually installed"
    );
    assert!(
        !fixture.column_exists(&schema, "extra").await,
        "version 2 succeeded and version 3 failed, so version 2 must have rolled back with it; \
         a column that survives here means each version committed on its own"
    );
}

/// A bundle whose declared policy forbids destructive operations is refused
/// BEFORE anything executes.
///
/// The refusal comes from the rendered-DDL guard, which applies
/// `safety.destructive_ops = "forbid"` as a data-security decision layered on the
/// parse. That is worth stating because the service briefly carried a second
/// comparison of its own, on the belief that the guard only flagged: the belief
/// was half right (it flags under `allow` and `warn`) and the extra gate could
/// only ever have fired under `warn`, where proceeding is what `warn` means.
#[compio::test]
async fn a_destructive_step_is_refused_when_the_declared_policy_forbids_it() {
    let fixture = Fixture::start().await;
    let schema = schema_name("destructive");

    apply_schema_bundle(&fixture.dsn, &bundle(&schema, 2, FP2))
        .await
        .expect("install v2");

    let mut dropping = bundle(&schema, 3, FP1);
    dropping.versions.push(SchemaBundleVersion {
        version: 3,
        sql: format!("ALTER TABLE \"{schema}\".\"{STATE}\" DROP COLUMN extra;"),
    });

    let mut forbidden = dropping.clone();
    forbidden.policy = charter(&schema, "forbid");
    let error = apply_schema_bundle(&fixture.dsn, &forbidden)
        .await
        .expect_err("a forbidden destructive step must be refused");
    match &error {
        BundleError::Guarded { version, detail } => {
            assert_eq!(*version, 3);
            assert!(
                detail.contains("DESTRUCTIVE_OPS_FORBID"),
                "the refusal must name the destructive rule, not some other denial: {detail}"
            );
        }
        other => panic!("expected a guard refusal naming the destructive rule: {other}"),
    }
    assert!(
        fixture.column_exists(&schema, "extra").await,
        "a refused destructive bundle must not have executed anything"
    );
    assert_eq!(fixture.stamp(&schema).await, Some((2, FP2.to_owned())));

    // The control: the SAME bundle, differing only in the destructive knob, is
    // applied. Without this the refusal above could be caused by anything.
    let allowed = apply_schema_bundle(&fixture.dsn, &dropping)
        .await
        .expect("an allowed destructive step applies");
    assert_eq!(allowed.action, SchemaBundleAction::Upgraded);
    assert!(!fixture.column_exists(&schema, "extra").await);
}

/// The bundle path must leave the app's RUNTIME role able to USE what it
/// installed - with no creator migration anywhere in this fixture.
///
/// # Why this calls nothing but `apply_schema_bundle`
///
/// It is the whole database hook the deploy path has. An app that declares
/// workflows has its journal installed here, and the worker then opens that
/// journal as the per-app runtime role. Every other Rust fixture that covers
/// this territory runs a creator-migration apply afterwards, and that apply
/// provisions the role as a side effect - which is how a deploy path that
/// provisions no role at all stayed green everywhere but production.
///
/// # Why the INSERT is not decoration
///
/// The role's grant set is `GRANT ... ON ALL TABLES IN SCHEMA`, a SNAPSHOT of
/// the tables that exist when it runs. A fix that created the role while
/// creating the schema - before the bundle's own DDL - would satisfy the
/// `SET LOCAL ROLE` below and still leave the installed tables unreachable. The
/// INSERT and the read-back are what separate a role that exists from a role
/// that can work.
#[compio::test]
async fn a_bundle_leaves_the_runtime_role_able_to_use_what_it_installed() {
    let mut fixture = Fixture::start().await;
    let schema = schema_name("runtime_role");

    apply_schema_bundle(&fixture.dsn, &bundle(&schema, 1, FP1))
        .await
        .expect("install v1");

    let role = per_app_role_name(&schema).expect("derive the runtime role name");
    let transaction = fixture
        .admin
        .transaction()
        .await
        .expect("open a transaction to narrow");
    transaction
        .batch_execute(&format!("SET LOCAL ROLE \"{role}\""))
        .await
        .unwrap_or_else(|error| {
            panic!(
                "the bundle left no runtime role for the worker to open the schema with: {}",
                detail(&error)
            )
        });
    assert_eq!(
        transaction
            .query_one("SELECT current_user", &[])
            .await
            .expect("read the narrowed identity")
            .get::<_, &str>(0),
        role,
        "SET LOCAL ROLE did not narrow the session, so every privilege below is the admin's"
    );
    transaction
        .execute(
            &format!("INSERT INTO \"{schema}\".\"{STATE}\" VALUES ('row', 'written as the app')"),
            &[],
        )
        .await
        .unwrap_or_else(|error| {
            panic!(
                "the runtime role cannot INSERT into a table the bundle installed: {}",
                detail(&error)
            )
        });
    let note: String = transaction
        .query_one(
            &format!("SELECT note FROM \"{schema}\".\"{STATE}\" WHERE id='row'"),
            &[],
        )
        .await
        .unwrap_or_else(|error| {
            panic!(
                "the runtime role cannot SELECT a table the bundle installed: {}",
                detail(&error)
            )
        })
        .get(0);
    assert_eq!(note, "written as the app");
    transaction.commit().await.expect("commit the app's write");

    // The control, in its own transaction because the refusal aborts one. The
    // grant is the narrow DML set: a runtime role that could also create schema
    // objects would pass every assertion above for the wrong reason.
    let transaction = fixture
        .admin
        .transaction()
        .await
        .expect("open a transaction for the control");
    transaction
        .batch_execute(&format!("SET LOCAL ROLE \"{role}\""))
        .await
        .expect("narrow the session again");
    let denied = transaction
        .batch_execute(&format!(
            "CREATE TABLE \"{schema}\".\"runtime_role_ddl\" (id text PRIMARY KEY)"
        ))
        .await
        .expect_err("the runtime role must not be able to create schema objects");
    assert_eq!(
        denied.code(),
        Some(&SqlState::INSUFFICIENT_PRIVILEGE),
        "expected the DDL to be refused for privilege, got {denied}"
    );
    transaction.rollback().await.expect("discard the control");
}

/// The bundle is confined to the schema it names. A step reaching another schema
/// is refused by the rendered-DDL guard, which is the property the whole path
/// rests on: a platform caller may install ITS schema, not any schema.
#[compio::test]
async fn a_step_naming_another_schema_is_refused_by_the_guard() {
    let fixture = Fixture::start().await;
    let schema = schema_name("confined");
    let foreign = schema_name("foreign");
    fixture
        .admin
        .batch_execute(&format!(
            "CREATE SCHEMA \"{foreign}\"; \
             CREATE TABLE \"{foreign}\".secrets (id text PRIMARY KEY)"
        ))
        .await
        .expect("create a neighbouring schema");

    let mut reaching = bundle(&schema, 1, FP1);
    reaching.versions[0].sql = format!(
        "{}\nALTER TABLE \"{foreign}\".secrets ADD COLUMN stolen text;",
        version_one(&schema)
    );
    let error = apply_schema_bundle(&fixture.dsn, &reaching)
        .await
        .expect_err("a step reaching another schema must be refused");
    assert!(
        matches!(error, BundleError::Guarded { version: 1, .. }),
        "{error}"
    );

    let reached: bool = fixture
        .admin
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
             WHERE table_schema=$1 AND table_name='secrets' AND column_name='stolen')",
            &[&foreign],
        )
        .await
        .expect("probe the neighbour")
        .get(0);
    assert!(!reached, "the guard admitted a reach into another schema");
}
