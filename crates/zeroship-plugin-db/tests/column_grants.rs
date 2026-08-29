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
//! Needs a live PostgreSQL, named by the typed test config
//! (`zeroship_core::config::test_database_url`) - `PG_TEST_URL` or the
//! generated overlay `deploy/ops/zeroship.test.toml`. It FAILS rather than
//! skips without one, for the same reason `mask_flip` does: a skipping run of a
//! privilege suite is indistinguishable from a passing one.
//!
//! ROLES ARE CLUSTER-GLOBAL, not per-database, so the role name carries a
//! random suffix and is dropped on the way out. A leftover role from an earlier
//! run would otherwise fail the next one with `42710 role already exists` -
//! which has previously been mistaken for a code regression.
//!
//! ```text
//! cargo test -p zeroship-plugin-db --features test-helpers \
//!   --test column_grants -- --test-threads=1
//! ```

use std::collections::BTreeSet;

use compio_postgres::{Client, NoTls};
use serde_json::{Value, json};
use zeroship_plugin_db::query::{
    FkEmission, SYSTEM_FIELD_NAMES, SqlDialect, SystemFieldAutoBump, build_create_table_with_fks,
    build_delete_many, build_delete_one, build_insert, build_insert_many,
    build_restore_many_with_system_fields, build_restore_one_with_system_fields,
    build_returning_expr, build_soft_delete_many_with_system_fields,
    build_soft_delete_one_with_system_fields, build_update_many_with_system_fields,
    build_update_one_with_system_fields, build_upsert, quote_ident, raw_column_name,
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
    json!({
        "ssn": {
            "type": "string",
            "mask": { "kind": "full", "classification": "pci" }
        },
        // The unmasked control: one column, on the read surface, so an arm that
        // passed by returning nothing at all would fail here.
        "nickname": { "type": "string" },
    })
}

fn test_url() -> String {
    zeroship_core::config::test_database_url()
}

async fn connect(url: &str) -> Client {
    let (client, connection) = compio_postgres::connect(url, NoTls)
        .await
        .unwrap_or_else(|e| {
            panic!("the column-grant suite requires a reachable server at PG_TEST_URL: {e}")
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

/// Stand the app schema and table up, and mint a role holding COLUMN grants
/// only.
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

    let ddl = build_create_table_with_fks(&app, COLLECTION, &schema, &FkEmission::Inline)
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

type Statement = (&'static str, String, Vec<String>);

fn autobump() -> SystemFieldAutoBump<'static> {
    SystemFieldAutoBump {
        dispatch_write: true,
        actor_id: Some("usr_actor"),
        ..Default::default()
    }
}

/// The write verbs whose statements name nothing but ordinary columns.
///
/// These are the ones the projection makes reachable: with `RETURNING *` every
/// one of them is refused, with the projection every one succeeds, and the
/// difference is the only variable between this list and
/// [`the_same_verbs_are_refused_outright_when_the_returning_clause_stars`].
fn column_grant_ready_statements(app: &str, schema: &Value) -> Vec<Statement> {
    let ab = autobump();
    let d = SqlDialect::Postgres;
    let filter = json!({ "id": "psn_seed" });
    let update = json!({ "nickname": "updated" });
    let mk =
        |verb: &'static str, bq: zeroship_plugin_db::query::BuiltQuery| (verb, bq.sql, bq.params);
    vec![
        mk(
            "insert",
            build_insert(
                app,
                COLLECTION,
                schema,
                &json!({ "id": "psn_ins", "nickname": "a" }),
            )
            .unwrap(),
        ),
        mk(
            "insertMany",
            build_insert_many(
                app,
                COLLECTION,
                schema,
                &json!([{ "id": "psn_m1", "nickname": "b" }, { "id": "psn_m2", "nickname": "c" }]),
            )
            .unwrap(),
        ),
        mk(
            "upsert",
            build_upsert(
                app,
                COLLECTION,
                schema,
                &json!({ "id": "psn_up", "nickname": "d" }),
                &json!(["id"]),
            )
            .unwrap(),
        ),
        mk(
            "updateMany",
            build_update_many_with_system_fields(app, COLLECTION, schema, &filter, &update, d, &ab)
                .unwrap(),
        ),
        mk(
            "softDeleteMany",
            build_soft_delete_many_with_system_fields(app, COLLECTION, schema, &filter, d, &ab)
                .unwrap(),
        ),
        mk(
            "restoreMany",
            build_restore_many_with_system_fields(app, COLLECTION, schema, &filter, d, &ab)
                .unwrap(),
        ),
        mk(
            "deleteMany",
            build_delete_many(app, COLLECTION, schema, &json!({ "id": "psn_ins" })).unwrap(),
        ),
    ]
}

/// The four single-row verbs, which narrow through `ctid`.
///
/// Kept apart from the list above and asserted SEPARATELY, because they are
/// still refused for a column-granted role and NOT because of their projection.
/// See [`the_single_row_verbs_are_still_blocked_by_their_ctid_narrowing`].
fn ctid_narrowed_statements(app: &str, schema: &Value) -> Vec<Statement> {
    let ab = autobump();
    let d = SqlDialect::Postgres;
    let filter = json!({ "id": "psn_seed" });
    let update = json!({ "nickname": "updated" });
    let mk =
        |verb: &'static str, bq: zeroship_plugin_db::query::BuiltQuery| (verb, bq.sql, bq.params);
    vec![
        mk(
            "updateOne",
            build_update_one_with_system_fields(app, COLLECTION, schema, &filter, &update, d, &ab)
                .unwrap(),
        ),
        mk(
            "softDeleteOne",
            build_soft_delete_one_with_system_fields(app, COLLECTION, schema, &filter, d, &ab)
                .unwrap(),
        ),
        mk(
            "restoreOne",
            build_restore_one_with_system_fields(app, COLLECTION, schema, &filter, d, &ab).unwrap(),
        ),
        mk(
            "deleteOne",
            build_delete_one(app, COLLECTION, schema, &filter).unwrap(),
        ),
    ]
}

/// Seven of the twelve builders reach the server here.
///
/// The other five: four narrow through `ctid` and are ruled on by their own
/// test; `build_find_or_create` has no production caller (`crud/mod.rs` says so
/// where `dispatch_find_or_create` used to be) and its projection is
/// `build_upsert`'s plus one computed column, pinned by the unit tests.
const WRITE_VERBS_EXERCISED: usize = 7;

/// The four that are not.
const CTID_NARROWED_VERBS: usize = 4;

/// **THE TEST.** A role holding only column grants completes every write verb.
#[compio::test]
async fn a_role_with_only_column_grants_completes_every_write_verb() {
    let url = test_url();
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
    session
        .batch_execute(&format!("SET ROLE {}", quote_ident(&role)))
        .await
        .expect("assume the role");

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

    // Seed one row the update / delete / restore verbs act on, as the ROLE.
    let seed = build_insert(
        &app,
        COLLECTION,
        &schema,
        &json!({ "id": "psn_seed", "nickname": "seed", "ssn": "***", raw.clone(): "123-45-6789" }),
    )
    .expect("seed insert builds");
    let seed_params: Vec<&str> = seed.params.iter().map(String::as_str).collect();
    session
        .query_text_params(&seed.sql, &seed_params)
        .await
        .unwrap_or_else(|e| panic!("the role must be able to seed a row: {e}\n{}", seed.sql));

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
        let refs: Vec<&str> = params.iter().map(String::as_str).collect();
        session
            .query_text_params(sql, &refs)
            .await
            .unwrap_or_else(|e| {
                panic!("{verb} must be executable by a column-granted role: {e}\n{sql}")
            });
        ruled_on += 1;
    }
    assert_eq!(
        ruled_on, WRITE_VERBS_EXERCISED,
        "every verb must have reached the server",
    );

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
}

/// **THE CONTROL**, differing in ONE variable: `RETURNING *` instead of the
/// projection. Same role, same table, same row, same verbs.
///
/// Without this the test above is only "these statements happen to run". With
/// it, the pass is attributable to the projection.
#[compio::test]
async fn the_same_verbs_are_refused_outright_when_the_returning_clause_stars() {
    let url = test_url();
    let admin = connect(&url).await;
    let suffix = unique_suffix();
    let (app, role) = fixture(&admin, &suffix).await;
    let schema = people_schema();

    let session = connect(&url).await;
    session
        .batch_execute(&format!("SET ROLE {}", quote_ident(&role)))
        .await
        .expect("assume the role");

    let seed = build_insert(
        &app,
        COLLECTION,
        &schema,
        &json!({ "id": "psn_seed", "nickname": "s" }),
    )
    .expect("seed builds");
    let seed_params: Vec<&str> = seed.params.iter().map(String::as_str).collect();
    session
        .query_text_params(&seed.sql, &seed_params)
        .await
        .expect("seed insert");

    let returning = build_returning_expr(&schema).expect("projection");
    let mut ruled_on = 0usize;
    for (verb, sql, params) in column_grant_ready_statements(&app, &schema) {
        let starred = sql.replace(&format!("RETURNING {returning}"), "RETURNING *");
        assert!(
            starred.contains("RETURNING *"),
            "{verb}: the mutation must have applied, or this control proves nothing: {sql}",
        );
        let refs: Vec<&str> = params.iter().map(String::as_str).collect();
        let err = session
            .query_text_params(&starred, &refs)
            .await
            .expect_err(&format!(
                "{verb} with `RETURNING *` must be refused: {starred}"
            ));
        assert!(
            format!("{err:?}").contains("42501"),
            "{verb}: expected 42501 permission denied, got {err:?}",
        );
        ruled_on += 1;
    }
    assert_eq!(
        ruled_on, WRITE_VERBS_EXERCISED,
        "the control must rule on the same verb set as the test it controls",
    );

    teardown(&admin, &app, &role).await;
}

/// **THE SECOND BLOCKER, and it is not the projection.**
///
/// The four single-row verbs narrow with `WHERE ctid = (SELECT ctid FROM ...
/// LIMIT 1)`. `ctid` is a SYSTEM column, and a column-level `SELECT (a, b, ...)`
/// grant does not cover system columns - reading one needs table-level `SELECT`.
/// Measured on this server: `SELECT ctid FROM t` is refused `42501 permission
/// denied for table t` for the same role whose `SELECT id FROM t` succeeds.
///
/// So replacing `RETURNING *` is NECESSARY and NOT SUFFICIENT for a
/// column-granted role. This test exists so that fact is executable rather than
/// a paragraph someone has to remember: it will go red the day the narrowing
/// stops naming `ctid`, and the fix is then to move these four verbs into
/// [`column_grant_ready_statements`].
///
/// It is careful about attribution. A bare "these are refused" would also pass
/// if the projection were broken, so the arms below isolate the cause: the same
/// role, on the same table, is refused `SELECT ctid` and served `SELECT id`.
#[compio::test]
async fn the_single_row_verbs_are_still_blocked_by_their_ctid_narrowing() {
    let url = test_url();
    let admin = connect(&url).await;
    let suffix = unique_suffix();
    let (app, role) = fixture(&admin, &suffix).await;
    let schema = people_schema();

    let session = connect(&url).await;
    session
        .batch_execute(&format!("SET ROLE {}", quote_ident(&role)))
        .await
        .expect("assume the role");

    let table = format!("{}.{}", quote_ident(&app), quote_ident(COLLECTION));

    // The cause, isolated. Two statements differing in ONE variable: which
    // column the subquery selects.
    let ctid_err = session
        .query_text_params(&format!("SELECT ctid FROM {table}"), &[])
        .await
        .expect_err("a column-granted role must not be able to read ctid");
    assert!(
        format!("{ctid_err:?}").contains("42501"),
        "expected 42501 on the system column, got {ctid_err:?}",
    );
    session
        .query_text_params(&format!("SELECT \"id\" FROM {table}"), &[])
        .await
        .expect("the same role reads an ordinary granted column");

    // And therefore the four verbs that name it.
    let statements = ctid_narrowed_statements(&app, &schema);
    assert_eq!(statements.len(), CTID_NARROWED_VERBS);
    let mut ruled_on = 0usize;
    for (verb, sql, params) in &statements {
        assert!(
            sql.contains("ctid"),
            "{verb} is in this list because it narrows through ctid: {sql}",
        );
        assert!(
            !sql.contains("RETURNING *"),
            "{verb}: the refusal below must be about ctid, not about a star: {sql}",
        );
        let refs: Vec<&str> = params.iter().map(String::as_str).collect();
        let err = session
            .query_text_params(sql, &refs)
            .await
            .expect_err(&format!("{verb} is expected to be refused today: {sql}"));
        assert!(
            format!("{err:?}").contains("42501"),
            "{verb}: expected 42501 from the ctid read, got {err:?}",
        );
        ruled_on += 1;
    }
    assert_eq!(ruled_on, CTID_NARROWED_VERBS);

    teardown(&admin, &app, &role).await;
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
    let schema = people_schema();
    let readable: BTreeSet<String> = projected_columns(&schema).into_iter().collect();
    let raw = raw_column_name("ssn");
    assert!(
        !readable.contains(&raw),
        "the raw column must not be on the read surface: {readable:?}",
    );
    for field in SYSTEM_FIELD_NAMES {
        assert!(
            readable.contains(*field),
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
        SYSTEM_FIELD_NAMES.len() + 2,
        "the read surface is the seven system fields plus the two declared ones: {readable:?}",
    );
}
