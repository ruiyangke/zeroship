//! Column-level grants: the reason the write builders stopped emitting
//! `RETURNING *`.
//!
//! # The property
//!
//! `*` is expanded by the server before privileges are checked, so it expands
//! to every column of the table - including columns the role may not read. The
//! whole statement is then refused, not just the columns:
//!
//! ```text
//! INSERT ... RETURNING *   -> ERROR 42501: permission denied for table people
//! INSERT ... RETURNING "id", "ssn"  -> 1 row
//! ```
//!
//! So a per-app role narrowed with column grants cannot execute ONE write verb
//! while the star is there. Naming the columns is what makes column grants
//! expressible at all - it is not a second containment for the same leak.
//!
//! # Why the fixture withholds exactly the raw column
//!
//! The role below holds `INSERT` on `__zs_raw__ssn` and NOT `SELECT`. That is
//! the shape the platform wants and the shape `*` cannot express: the data
//! plane must WRITE the authoritative value (that is where a masked field's
//! real value lives) and must never READ it back on an ordinary verb - the
//! explicit unmask API is the one reader, under its own authorization check and
//! audit row.
//!
//! With `RETURNING *` that grant is unusable: every write verb fails. With the
//! projection every write verb succeeds and the raw column is unreadable by
//! construction rather than by a Rust-side row predicate running afterwards.
//!
//! # Fixture
//!
//! PostgreSQL comes from an owned testcontainer. Its roles and schemas are
//! isolated from other tests and released with the server.
//! Run: `cargo xtask test data --filter 'test(column_grants::)'`

use crate::tests::fixtures::Host;
#[allow(unused_imports)]
use crate::tests::fixtures::schema::{fixture_table_sql, fixture_table_sql_for};
#[allow(unused_imports)]
use zeroship_migrate::schema::query::FkEmission;

use std::collections::BTreeSet;

use compio_postgres::{Client, NoTls};
use crate::sql::compile::{
    SqlDialect, WriteAssignments, build_delete_many, build_delete_one, build_insert,
    build_insert_many, build_restore_many_with_assignments, build_restore_one_with_assignments,
    build_returning_expr, build_soft_delete_many_with_assignments,
    build_soft_delete_one_with_assignments, build_update_many_with_assignments,
    build_update_one_with_assignments, build_upsert, quote_ident, raw_column_name,
};
use crate::sql::render::postgres::{render_delete, render_update};
use crate::value::{Value, value};
use crate::sql::{
    Assignment as PlanAssignment, ColumnAssignment as PlanColumnAssignment,
    CompareOp as PlanCompareOp, Delete as PlanDelete, Ident as PlanIdent,
    IdentRole as PlanIdentRole, Literal as PlanLiteral, Operand as PlanOperand,
    Predicate as PlanPredicate, ProjectedField as PlanProjectedField, Projection as PlanProjection,
    Returning as PlanReturning, RowLimit as PlanRowLimit, Update as PlanUpdate,
};

/// The collection every arm writes.
const COLLECTION: &str = "people";

/// The descriptor entry, and the DDL input. ONE value: the risk this suite is
/// about is the projection naming a column the table does not have, and two
/// literals cannot be checked to agree.
///
/// `ssn` is masked, so the emitter gives the table a second physical column -
/// `__zs_raw__ssn` - that is not a declared field and therefore not on the
/// projection. That column is the whole point of the fixture.
fn people_schema() -> Value {
    crate::tests::fixtures::schema::generated_fields(value!({
        "ssn": {
            "type": "string",
            "mask": { "kind": "full", "classification": "pci" }
        },
        // The unmasked control: one column, on the read surface, so an arm that
        // passed by returning nothing at all would fail here.
        "nickname": { "type": "string" },
    }))
}

async fn connect(url: &str) -> Client {
    let (client, connection) = compio_postgres::connect(url, NoTls)
        .await
        .unwrap_or_else(|e| {
            panic!("the column-grant suite could not connect to its PostgreSQL testcontainer: {e}")
        });
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    client
}

/// A suffix unique per run. Roles live in the cluster, not the database.
fn unique_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    format!("{nanos:x}")
}

/// The columns the projection names, unquoted, in the order it names them.
///
/// Derived from [`build_returning_expr`] rather than spelled out, so the GRANT
/// this fixture issues is BY CONSTRUCTION the set the statements ask for. A
/// hand-written list would let the grant and the projection drift, and the
/// suite would then be measuring the list rather than the builder.
fn projected_columns(schema: &Value) -> Vec<String> {
    build_returning_expr(schema)
        .expect("the schema is an object")
        .split(", ")
        .map(|term| {
            let name = term.rsplit(" AS ").next().unwrap_or(term);
            name.trim().trim_matches('"').to_string()
        })
        .collect()
}

/// Stand the app schema and table up, and mint a role holding column-scoped
/// SELECT, INSERT and UPDATE grants.
///
/// PostgreSQL exposes DELETE only as a table privilege, so a readwrite binding
/// must also grant table DELETE. That does not grant SELECT on any column and
/// therefore does not weaken the withheld raw-column control this suite owns.
///
/// Returns `(app_schema, role_name)`.
async fn fixture(admin: &Client, suffix: &str) -> (String, String) {
    let app = format!("colgrant_{suffix}");
    let role = format!("colgrant_role_{suffix}");
    let schema = people_schema();

    admin
        .batch_execute(&format!("CREATE SCHEMA {}", quote_ident(&app)))
        .await
        .expect("create the app schema");

    let ddl = fixture_table_sql(
        &crate::sql::SchemaName::new(&app).expect("fixture schema name"),
        COLLECTION,
        &schema,
        &FkEmission::Inline,
    )
    .expect("the platform's own CREATE TABLE emitter");
    admin
        .batch_execute(&ddl)
        .await
        .unwrap_or_else(|e| panic!("emitted DDL must apply: {e}\n{ddl}"));

    admin
        .batch_execute(&format!("CREATE ROLE {} NOLOGIN", quote_ident(&role)))
        .await
        .unwrap_or_else(|e| panic!("create the role: {e}"));

    let table = format!("{}.{}", quote_ident(&app), quote_ident(COLLECTION));
    let raw = raw_column_name("ssn");

    // Every physical column the write verbs touch. The raw column is here for
    // INSERT and UPDATE and deliberately absent from SELECT below.
    let mut writable: Vec<String> = projected_columns(&schema);
    writable.push(raw.clone());
    let writable_list = writable
        .iter()
        .map(|c| quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ");

    // The read surface. NOT the raw column.
    let readable_list = projected_columns(&schema)
        .iter()
        .map(|c| quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ");

    admin
        .batch_execute(&format!(
            "GRANT USAGE ON SCHEMA {} TO {role_q};\n\
             GRANT SELECT ({readable_list}) ON {table} TO {role_q};\n\
             GRANT INSERT ({writable_list}) ON {table} TO {role_q};\n\
             GRANT UPDATE ({writable_list}) ON {table} TO {role_q};\n\
             GRANT DELETE ON {table} TO {role_q};",
            quote_ident(&app),
            role_q = quote_ident(&role),
        ))
        .await
        .unwrap_or_else(|e| panic!("issue the column grants: {e}"));

    let table_privilege_rows = admin
        .query_text_params(
            "SELECT privilege_type FROM information_schema.table_privileges \
               WHERE grantee = $1 AND table_schema = $2 AND table_name = $3 \
               ORDER BY privilege_type",
            &[&role, &app, COLLECTION],
        )
        .await
        .expect("read the fixture's exact table privileges");
    let table_privileges = table_privilege_rows
        .iter()
        .map(|row| row.get::<_, String>(0))
        .collect::<Vec<_>>();
    assert_eq!(
        table_privileges,
        vec!["DELETE"],
        "DELETE is the sole table privilege; read authority must stay column-scoped"
    );

    (app, role)
}

async fn teardown(admin: &Client, app: &str, role: &str) {
    // Reverse order, and by exact name. Nothing here drops an object this
    // fixture did not create.
    let _ = admin
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS {} CASCADE",
            quote_ident(app)
        ))
        .await;
    let _ = admin
        .batch_execute(&format!("DROP OWNED BY {}", quote_ident(role)))
        .await;
    let _ = admin
        .batch_execute(&format!("DROP ROLE IF EXISTS {}", quote_ident(role)))
        .await;
}

/// The server this suite actually reached, read from the server rather than
/// inferred from a container tag or a DSN.
async fn server_version_num(client: &Client) -> String {
    let rows = client
        .query_text_params("SHOW server_version_num", &[])
        .await
        .expect("read server_version_num");
    rows[0].get::<_, String>(0)
}

/// Begin one role-scoped test arm and prove `SET LOCAL ROLE` took effect.
async fn begin_as_role(session: &Client, role: &str) {
    session
        .batch_execute(&format!("BEGIN; SET LOCAL ROLE {}", quote_ident(role)))
        .await
        .expect("begin the role-scoped arm");
    let rows = session
        .query_text_params("SELECT current_user", &[])
        .await
        .expect("read the effective role");
    let current_user = rows[0].get::<_, String>(0);
    println!("column_grants role-scoped arm: current_user={current_user}");
    assert_eq!(
        current_user, role,
        "SET LOCAL ROLE must take effect inside the explicit transaction"
    );
}

type Statement = (&'static str, String, Vec<Value>);

fn assignments(schema: &Value, deleting: bool, restoring: bool) -> WriteAssignments {
    crate::assignments::AssignmentPlan::from_schema(schema)
        .unwrap()
        .write_assignments(schema, Some("usr_actor"), deleting, restoring)
}

/// The write verbs whose statements name nothing but ordinary columns.
///
/// These are the ones the projection makes reachable: with `RETURNING *` every
/// one of them is refused, with the projection every one succeeds, and the
/// difference is the only variable between this list and
/// [`the_same_verbs_are_refused_outright_when_the_returning_clause_stars`].
fn column_grant_ready_statements(app: &str, schema: &Value) -> Vec<Statement> {
    let d = SqlDialect::Postgres;
    let filter = value!({ "id": "psn_seed" });
    let update = value!({ "nickname": "updated" });
    let mk =
        |verb: &'static str, bq: crate::sql::compiler::CompiledQuery| (verb, bq.sql, bq.params);
    vec![
        mk(
            "insert",
            build_insert(
                &crate::sql::SchemaName::new(app).expect("fixture schema name"),
                COLLECTION,
                schema,
                &value!({ "id": "psn_ins", "nickname": "a" }),
            )
            .unwrap(),
        ),
        mk(
            "insertMany",
            build_insert_many(
                &crate::sql::SchemaName::new(app).expect("fixture schema name"),
                COLLECTION,
                schema,
                &value!([{ "id": "psn_m1", "nickname": "b" }, { "id": "psn_m2", "nickname": "c" }]),
            )
            .unwrap(),
        ),
        mk(
            "upsert",
            build_upsert(
                &crate::sql::SchemaName::new(app).expect("fixture schema name"),
                COLLECTION,
                schema,
                &value!({ "id": "psn_up", "nickname": "d" }),
                &value!(["id"]),
            )
            .unwrap(),
        ),
        mk(
            "updateMany",
            build_update_many_with_assignments(
                &crate::sql::SchemaName::new(app).expect("fixture schema name"),
                COLLECTION,
                schema,
                &filter,
                &update,
                d,
                &assignments(schema, false, false),
            )
            .unwrap(),
        ),
        mk(
            "softDeleteMany",
            build_soft_delete_many_with_assignments(
                &crate::sql::SchemaName::new(app).expect("fixture schema name"),
                COLLECTION,
                schema,
                &filter,
                d,
                &assignments(schema, true, false),
            )
            .unwrap(),
        ),
        mk(
            "restoreMany",
            build_restore_many_with_assignments(
                &crate::sql::SchemaName::new(app).expect("fixture schema name"),
                COLLECTION,
                schema,
                &filter,
                d,
                &assignments(schema, false, true),
            )
            .unwrap(),
        ),
        mk(
            "deleteMany",
            build_delete_many(
                &crate::sql::SchemaName::new(app).expect("fixture schema name"),
                COLLECTION,
                schema,
                &value!({ "id": "psn_ins" }),
                d,
            )
            .unwrap(),
        ),
    ]
}

/// The four single-row verbs, kept together so the live privilege test rules
/// on every production builder that shares the bounded target shape.
fn single_row_statements(app: &str, schema: &Value) -> Vec<Statement> {
    let d = SqlDialect::Postgres;
    let filter = value!({ "id": "psn_seed" });
    let update = value!({ "nickname": "updated" });
    let mk =
        |verb: &'static str, bq: crate::sql::compiler::CompiledQuery| (verb, bq.sql, bq.params);
    vec![
        mk(
            "updateOne",
            build_update_one_with_assignments(
                &crate::sql::SchemaName::new(app).expect("fixture schema name"),
                COLLECTION,
                schema,
                &filter,
                &update,
                d,
                &assignments(schema, false, false),
            )
            .unwrap(),
        ),
        mk(
            "softDeleteOne",
            build_soft_delete_one_with_assignments(
                &crate::sql::SchemaName::new(app).expect("fixture schema name"),
                COLLECTION,
                schema,
                &filter,
                d,
                &assignments(schema, true, false),
            )
            .unwrap(),
        ),
        mk(
            "restoreOne",
            build_restore_one_with_assignments(
                &crate::sql::SchemaName::new(app).expect("fixture schema name"),
                COLLECTION,
                schema,
                &filter,
                d,
                &assignments(schema, false, true),
            )
            .unwrap(),
        ),
        mk(
            "deleteOne",
            build_delete_one(
                &crate::sql::SchemaName::new(app).expect("fixture schema name"),
                COLLECTION,
                schema,
                &filter,
            )
            .unwrap(),
        ),
    ]
}

/// Render the replacement data-plan's two bounded PostgreSQL writes against
/// the same physical fixture as the shipped builders.
fn bounded_data_plan_statements(app: &str) -> Vec<(&'static str, crate::sql::compiler::CompiledQuery)> {
    let namespace = PlanIdent::parse_as(app, PlanIdentRole::Namespace).expect("namespace");
    let collection =
        PlanIdent::parse_as(COLLECTION, PlanIdentRole::Collection).expect("collection");
    let id = || PlanIdent::parse_as("id", PlanIdentRole::Column).expect("id column");
    let returning = || {
        PlanReturning::rows(
            PlanProjection::rows(vec![
                PlanProjectedField::column(
                    PlanIdent::parse_as("ssn", PlanIdentRole::Column).expect("ssn column"),
                )
                .expect("project ssn"),
                PlanProjectedField::column(
                    PlanIdent::parse_as("nickname", PlanIdentRole::Column)
                        .expect("nickname column"),
                )
                .expect("project nickname"),
            ])
            .expect("row projection"),
        )
        .expect("return rows")
    };
    let filter = || {
        PlanPredicate::compare(
            PlanOperand::column(id()),
            PlanCompareOp::Eq,
            PlanOperand::Lit(PlanLiteral::text("psn_seed").expect("id literal")),
        )
    };
    let limit = PlanRowLimit::new(1).expect("single-row bound");

    let update = PlanUpdate::builder(
        collection.clone(),
        PlanIdent::parse_as("id", PlanIdentRole::Column).unwrap(),
        limit,
        returning(),
    )
    .namespace(namespace.clone())
    .set(PlanColumnAssignment::new(
        PlanIdent::parse_as("nickname", PlanIdentRole::Column).expect("nickname column"),
        PlanAssignment::bind(PlanLiteral::text("updated-by-plan").expect("update literal")),
    ))
    .filter(filter())
    .build()
    .expect("bounded update plan");
    let delete = PlanDelete::builder(
        collection,
        PlanIdent::parse_as("id", PlanIdentRole::Column).unwrap(),
        limit,
        returning(),
    )
    .namespace(namespace)
    .filter(filter())
    .build()
    .expect("bounded delete plan");

    vec![
        (
            "data-plan update",
            render_update(&update).expect("render bounded update"),
        ),
        (
            "data-plan delete",
            render_delete(&delete).expect("render bounded delete"),
        ),
    ]
}

/// Seven of the twelve builders reach the server in the projection test.
///
/// Four single-row builders are ruled on by their own primary-key narrowing
/// test. `build_find_or_create` has no production caller (`crud/mod.rs` says so
/// where `dispatch_find_or_create` used to be), and its projection is
/// `build_upsert`'s plus one computed column, pinned by the unit tests.
const WRITE_VERBS_EXERCISED: usize = 7;

/// The four live builders that share the single-row target shape.
const SINGLE_ROW_VERBS: usize = 4;

/// **THE TEST.** A role with column-scoped read/write grants completes every
/// projected verb. Its sole table privilege is the unavoidable DELETE grant.
#[test]
fn column_scoped_reads_complete_every_projected_write_verb() {
    Host::test(|host| {
        host.run(async {
            let postgres = crate::tests::fixtures::postgres::Postgres::start();
            let url = postgres.url();
            let admin = connect(&url).await;
            println!(
                "column_grants oracle: server_version_num={}",
                server_version_num(&admin).await
            );
            let suffix = unique_suffix();
            let (app, role) = fixture(&admin, &suffix).await;
            let schema = people_schema();

            // The role must not be able to read the raw column even by naming it -
            // otherwise the grant is not what this suite claims it is.
            let session = connect(&url).await;
            begin_as_role(&session, &role).await;

            let table = format!("{}.{}", quote_ident(&app), quote_ident(COLLECTION));
            let raw = raw_column_name("ssn");
            let denied = session
                .query_text_params(&format!("SELECT {} FROM {table}", quote_ident(&raw)), &[])
                .await
                .expect_err("the role must not be able to read the raw column");
            assert!(
                format!("{denied:?}").contains("42501"),
                "expected a privilege refusal on the raw column, got {denied:?}",
            );
            session
                .batch_execute("ROLLBACK")
                .await
                .expect("close the refused raw-column arm");

            // Seed one row the update / delete / restore verbs act on, as the ROLE.
            begin_as_role(&session, &role).await;
            let seed = build_insert(
        &crate::sql::SchemaName::new(&app).expect("fixture schema name"),
        COLLECTION,
        &schema,
        &value!({ "id": "psn_seed", "nickname": "seed", "ssn": "***", (raw.clone()): "123-45-6789" }),
    )
    .expect("seed insert builds");
            let seed_params = &seed.params;
            zeroship_data_orm::backend::postgres::params::query(&session, &seed.sql, seed_params)
                .await
                .unwrap_or_else(|e| {
                    panic!("the role must be able to seed a row: {e}\n{}", seed.sql)
                });

            let statements = column_grant_ready_statements(&app, &schema);
            assert_eq!(
                statements.len(),
                WRITE_VERBS_EXERCISED,
                "this suite must rule on every write verb it claims to",
            );

            let mut ruled_on = 0usize;
            for (verb, sql, params) in &statements {
                assert!(
                    !sql.contains("RETURNING *"),
                    "{verb} still stars; the arm below would not be measuring the projection: {sql}",
                );
                let refs = &params;
                zeroship_data_orm::backend::postgres::params::query(&session, sql, refs)
                    .await
                    .unwrap_or_else(|e| {
                        panic!("{verb} must be executable with column-scoped reads: {e}\n{sql}")
                    });
                ruled_on += 1;
            }
            assert_eq!(
                ruled_on, WRITE_VERBS_EXERCISED,
                "every verb must have reached the server",
            );
            session
                .batch_execute("COMMIT")
                .await
                .expect("commit the projected write verbs");

            // The raw column really is in the table and really did receive the value -
            // so "the role cannot read it" is about the grant, not about a write that
            // never stored anything.
            let stored = admin
                .query_text_params(
                    &format!(
                        "SELECT {} AS raw FROM {table} WHERE \"id\" = $1",
                        quote_ident(&raw)
                    ),
                    &["psn_seed"],
                )
                .await
                .expect("the admin can read the raw column");
            assert_eq!(stored.len(), 1, "the seeded row must exist");
            assert_eq!(
                stored[0].get::<_, Option<String>>("raw").as_deref(),
                Some("123-45-6789"),
                "the role wrote the authoritative value it may not read back",
            );

            teardown(&admin, &app, &role).await;
        })
    })
}

/// **THE CONTROL**, differing in ONE variable: `RETURNING *` instead of the
/// projection. Same role, same table, same row, same verbs.
///
/// Without this the test above is only "these statements happen to run". With
/// it, the pass is attributable to the projection.
#[test]
fn the_same_verbs_are_refused_outright_when_the_returning_clause_stars() {
    Host::test(|host| {
        host.run(async {
            let postgres = crate::tests::fixtures::postgres::Postgres::start();
            let url = postgres.url();
            let admin = connect(&url).await;
            let suffix = unique_suffix();
            let (app, role) = fixture(&admin, &suffix).await;
            let schema = people_schema();

            let session = connect(&url).await;
            begin_as_role(&session, &role).await;
            let seed = build_insert(
                &crate::sql::SchemaName::new(&app).expect("fixture schema name"),
                COLLECTION,
                &schema,
                &value!({ "id": "psn_seed", "nickname": "s" }),
            )
            .expect("seed builds");
            let seed_params = &seed.params;
            zeroship_data_orm::backend::postgres::params::query(&session, &seed.sql, seed_params)
                .await
                .expect("seed insert");
            session
                .batch_execute("COMMIT")
                .await
                .expect("commit the control seed");

            let returning = build_returning_expr(&schema).expect("projection");
            let mut ruled_on = 0usize;
            for (verb, sql, params) in column_grant_ready_statements(&app, &schema) {
                let starred = if sql.contains(" RETURNING ") {
                    sql.replace(&format!("RETURNING {returning}"), "RETURNING *")
                } else {
                    format!("{sql} RETURNING *")
                };
                assert!(
                    starred.contains("RETURNING *"),
                    "{verb}: the mutation must have applied, or this control proves nothing: {sql}",
                );
                let parameters: Vec<_> = params
                    .iter()
                    .map(zeroship_data_orm::backend::postgres::params::Parameter)
                    .collect();
                let refs: Vec<&(dyn compio_postgres::types::ToSql + Sync)> =
                    parameters.iter().map(|p| p as _).collect();
                begin_as_role(&session, &role).await;
                let err = session.query(&starred, &refs).await.expect_err(&format!(
                    "{verb} with `RETURNING *` must be refused: {starred}"
                ));
                assert!(
                    err.code() == Some(&compio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE),
                    "{verb}: expected 42501 permission denied, got {err:?}",
                );
                session
                    .batch_execute("ROLLBACK")
                    .await
                    .expect("close the refused RETURNING star arm");
                ruled_on += 1;
            }
            assert_eq!(
                ruled_on, WRITE_VERBS_EXERCISED,
                "the control must rule on the same verb set as the test it controls",
            );

            teardown(&admin, &app, &role).await;
        })
    })
}

/// The four single-row verbs must execute without SELECT access to `ctid`.
///
/// The control first proves that this exact role still receives `42501` when it
/// names `ctid`. A fresh transaction then proves the role can read the ordinary
/// granted primary key and executes the actual production statements. This is
/// behavioural: changing a SQL string without making all four statements run
/// on PostgreSQL cannot make the test pass. Hard DELETE also needs the fixture's
/// table DELETE privilege because PostgreSQL has no column form of that verb;
/// that privilege grants no read access to `ctid` or to the withheld column.
#[test]
fn the_single_row_verbs_succeed_without_ctid_access() {
    Host::test(|host| {
        host.run(async {
            let postgres = crate::tests::fixtures::postgres::Postgres::start();
            let url = postgres.url();
            let admin = connect(&url).await;
            let suffix = unique_suffix();
            let (app, role) = fixture(&admin, &suffix).await;
            let schema = people_schema();

            let session = connect(&url).await;
            let table = format!("{}.{}", quote_ident(&app), quote_ident(COLLECTION));

            // The control is its own explicit transaction because PostgreSQL aborts a
            // transaction after the expected privilege error.
            begin_as_role(&session, &role).await;
            let ctid_err = session
                .query_text_params(&format!("SELECT ctid FROM {table}"), &[])
                .await
                .expect_err("the column-scoped role must not be able to read ctid");
            assert!(
                format!("{ctid_err:?}").contains("42501"),
                "expected 42501 on the system column, got {ctid_err:?}",
            );
            session
                .batch_execute("ROLLBACK")
                .await
                .expect("close the refused ctid arm");

            begin_as_role(&session, &role).await;
            session
                .query_text_params(&format!("SELECT \"id\" FROM {table}"), &[])
                .await
                .expect("the same role reads an ordinary granted column");

            let raw = raw_column_name("ssn");
            let seed = build_insert(
                &crate::sql::SchemaName::new(&app).expect("fixture schema name"),
                COLLECTION,
                &schema,
                &value!({ "id": "psn_seed", "nickname": "seed", "ssn": "***", raw: "123-45-6789" }),
            )
            .expect("seed insert builds");
            let seed_refs = &seed.params;
            zeroship_data_orm::backend::postgres::params::query(&session, &seed.sql, seed_refs)
                .await
                .unwrap_or_else(|e| {
                    panic!("the role must be able to seed a row: {e}\n{}", seed.sql)
                });

            let statements = single_row_statements(&app, &schema);
            assert_eq!(statements.len(), SINGLE_ROW_VERBS);
            let mut ruled_on = 0usize;
            for (verb, sql, params) in &statements {
                assert!(
                    !sql.contains("RETURNING *"),
                    "{verb}: the behavioural arm must retain its column projection: {sql}",
                );
                let refs = &params;
                let rows = zeroship_data_orm::backend::postgres::params::query(&session, sql, refs)
                    .await
                    .unwrap_or_else(|e| {
                        panic!("{verb} must execute under column grants: {e}\n{sql}")
                    });
                assert_eq!(rows.len(), 1, "{verb} must affect exactly the seeded row");
                ruled_on += 1;
            }
            assert_eq!(ruled_on, SINGLE_ROW_VERBS);
            session
                .batch_execute("COMMIT")
                .await
                .expect("commit the successful single-row verbs");

            teardown(&admin, &app, &role).await;
        })
    })
}

/// The replacement data-plan renderer must obey the same live grant boundary.
///
/// Both statements execute on PostgreSQL. A renderer-only assertion would let
/// a different privilege mistake pass while the SQL merely stopped spelling
/// `ctid`.
#[test]
fn bounded_data_plan_writes_succeed_with_column_scoped_reads() {
    Host::test(|host| {
        host.run(async {
            let postgres = crate::tests::fixtures::postgres::Postgres::start();
            let url = postgres.url();
            let admin = connect(&url).await;
            let suffix = unique_suffix();
            let (app, role) = fixture(&admin, &suffix).await;
            let schema = people_schema();
            let session = connect(&url).await;

            begin_as_role(&session, &role).await;
            let raw = raw_column_name("ssn");
            let seed = build_insert(
                &crate::sql::SchemaName::new(&app).expect("fixture schema name"),
                COLLECTION,
                &schema,
                &value!({ "id": "psn_seed", "nickname": "seed", "ssn": "***", raw: "123-45-6789" }),
            )
            .expect("seed insert builds");
            let seed_refs = &seed.params;
            zeroship_data_orm::backend::postgres::params::query(&session, &seed.sql, seed_refs)
                .await
                .unwrap_or_else(|e| {
                    panic!("the role must be able to seed a row: {e}\n{}", seed.sql)
                });

            let statements = bounded_data_plan_statements(&app);
            assert_eq!(
                statements.len(),
                2,
                "update and delete must both be ruled on"
            );
            let mut ruled_on = 0usize;
            for (verb, rendered) in statements {
                let rows = crate::backend::postgres::params::query(
                    &session, rendered.sql(), rendered.params(),
                )
                    .await
                    .unwrap_or_else(|e| {
                        panic!(
                            "{verb} must execute under column grants: {e}\n{}",
                            rendered.sql()
                        )
                    });
                assert_eq!(rows.len(), 1, "{verb} must affect exactly the bounded row");
                ruled_on += 1;
            }
            assert_eq!(ruled_on, 2, "both bounded writes must reach PostgreSQL");
            session
                .batch_execute("COMMIT")
                .await
                .expect("commit the successful data-plan writes");

            teardown(&admin, &app, &role).await;
        })
    })
}

/// The grant this fixture issues is the projection's own column set.
///
/// Not a tautology: it is the assertion that makes the two tests above
/// meaningful. If the projection named a column outside the grant, the first
/// test would fail; if the grant were WIDER than the projection - say the whole
/// table - the second test would fail. This pins that the fixture withholds
/// exactly one column, and which one.
#[test]
fn the_fixture_withholds_exactly_the_raw_column() {
    Host::test(|_| {
        let schema = people_schema();
        let readable: BTreeSet<String> = projected_columns(&schema).into_iter().collect();
        let raw = raw_column_name("ssn");
        assert!(
            !readable.contains(&raw),
            "the raw column must not be on the read surface: {readable:?}",
        );
        for field in crate::tests::fixtures::schema::generated_fields(value!({}))
            .as_object()
            .unwrap()
            .keys()
        {
            assert!(
                readable.contains(field),
                "the projection must name the system field {field}: {readable:?}",
            );
        }
        assert!(
            readable.contains("ssn"),
            "the masked column is readable: {readable:?}"
        );
        assert!(
            readable.contains("nickname"),
            "the control column is readable: {readable:?}"
        );
        assert_eq!(
            readable.len(),
            people_schema().as_object().unwrap().len(),
            "the read surface is the seven system fields plus the two declared ones: {readable:?}",
        );
    })
}
