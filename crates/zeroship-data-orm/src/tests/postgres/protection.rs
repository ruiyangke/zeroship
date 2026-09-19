//! Mask storage, authorization and projection contracts against PostgreSQL.
//!
//! Fixtures use migration-emitted DDL and the runtime write pipeline so the test
//! can detect disagreements about visible and protected storage. Predicate checks
//! verify that filtering cannot reveal a protected value without unmask authorization.
//! PostgreSQL and its required extensions come from an owned testcontainer.

#[allow(unused_imports)]
use crate::tests::fixtures::schema::fixture_table_sql;
use crate::tests::fixtures::Host;
use crate::tests::fixtures::{self, schema};
#[allow(unused_imports)]
use zeroship_migrate::schema::query::FkEmission;

use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use crate::sql::mapping::{raw_column_name, read_surface_columns, validate_field_name};
use crate::sql::{registration::SqlRegistration, SchemaName};
use crate::value::{value, Value};
use compio_postgres::{NoTls, Pool};
use zeroship_data_orm::error::DbError;
use zeroship_data_orm::protection::mask_policy::install_mask_policy;
use zeroship_data_orm::protection::unmask::{
    audit_query_hint_granted, authorize_query_hint, dispatch_bulk_unmask, dispatch_unmask,
    dispatch_unmask_for_query, parse_args, parse_bulk_args, BulkUnmaskArgs, BulkUnmaskItem,
    UnmaskFieldArgs,
};

/// Use the backend already installed by the fixture; this helper does not test cold startup.
async fn unmask_backend(host: &Host) -> zeroship_data_orm::backend::BackendHandle {
    host.backend()
        .await
        .expect("the backend the V8 dispatcher would have opened")
}

/// Bind a route from the fixture’s backend and current transaction scope.
async fn unmask_route(host: &Host, app: &str) -> zeroship_data_orm::tx_route::TxRoute {
    zeroship_data_orm::exec::ambient_route_for_tests(&crate::tests::fixtures::harness_binding(app), unmask_backend(host).await)
}

/// Connect, or fail the test.
///
/// Deliberately NOT a skip, for the reason the module doc gives.
async fn require_pg() -> (crate::tests::fixtures::postgres::Postgres, String) {
    let postgres = crate::tests::fixtures::postgres::Postgres::start();
    let url = postgres.url();
    match compio_postgres::connect(&url, NoTls).await {
        Ok((client, connection)) => {
            compio::runtime::spawn(async move {
                let _ = connection.run().await;
            })
            .detach();
            drop(client);
            (postgres, url)
        }
        Err(e) => {
            panic!("the mask-flip suite could not connect to its PostgreSQL testcontainer: {e}")
        }
    }
}

async fn release_pg(host: &Host, pool: Rc<Pool>) {
    drop(pool);
    host.reset();
    let _ = compio_postgres::drain_connections(std::time::Duration::from_secs(2)).await;
}

/// The schema both oracle fixtures use: one masked column and one unmasked
/// control that differs in exactly one variable (the `mask` block).
fn flip_schema() -> Value {
    value!({
        "ssn": {
            "type": "string",
            "mask": { "kind": "full", "classification": "pci" }
        },
        // The control. Same type, same nullability, no mask. Every assertion
        // about `ssn` below has a twin about `nickname`, so a fixture that
        // simply returned no rows for everything cannot pass.
        "nickname": { "type": "string" },
    })
}

/// A masked column whose declared name is snake_case, so a camelCase hint has
/// to travel through `resolve_schema_column`'s alias tolerance to be found.
///
/// `flip_schema` cannot express this: `ssn` is spelled identically in every
/// case convention, so no alias resolution happens and the canonical name and
/// the caller's name are the same string whatever the code does.
fn alias_schema() -> Value {
    value!({
        "contact_email": {
            "type": "string",
            "mask": { "kind": "email", "classification": "pii" }
        },
        "nickname": { "type": "string" },
    })
}

/// [`flip_schema`] with a SECOND masked column whose classification DIFFERS.
///
/// The batch and query-hint paths authorise a REQUEST, not a cell, and the only
/// shape in which "atomic" and "per-column" differ observably is a request that
/// is half-authorised. That needs two classifications a single policy can split
/// on, which `flip_schema` cannot express: it declares one masked column, so
/// every request over it is authorised entirely or refused entirely whatever
/// the fence does.
fn two_class_schema() -> Value {
    value!({
        "ssn": {
            "type": "string",
            "mask": { "kind": "full", "classification": "pci" }
        },
        "email": {
            "type": "string",
            "mask": { "kind": "email", "classification": "pii" }
        },
        "nickname": { "type": "string" },
    })
}

/// Create `<binding schema>.<collection>` from the DDL the platform actually
/// emits, and install the descriptor entry the deploy would have installed.
async fn fixture(
    host: &Host,
    pool: &Rc<Pool>,
    url: &str,
    app: &str,
    collection: &str,
    schema: &Value,
) {
    let alias = crate::tests::fixtures::harness_alias(app);
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{alias}\" CASCADE"), &[])
        .await
        .unwrap();
    pool.execute(&format!("CREATE SCHEMA \"{alias}\""), &[])
        .await
        .unwrap();
    let ddl = fixture_table_sql(
        crate::tests::fixtures::harness_binding(app).schema(),
        collection,
        schema,
        &FkEmission::Inline,
    )
    .expect("the platform's own CREATE TABLE emitter");
    pool.batch_execute(&ddl)
        .await
        .unwrap_or_else(|e| panic!("emitted DDL must apply: {e}\n{ddl}"));
    host.install_postgres_pool(Rc::clone(pool), url);
    crate::tests::fixtures::cache_schema(app, collection, schema.clone());
}

/// What one write through the pipeline left behind.
///
/// `id` is PLATFORM-ASSIGNED: the write pipeline refuses a document that
/// carries one, so no fixture in this file may choose a row's identity. Every
/// test below learns it from the write that created the row, which makes each
/// assertion on an id a statement that identity round-trips through the
/// platform's own minting rather than a comparison against a literal the
/// fixture and the assertion both made up.
struct Inserted {
    /// The id the platform minted for this row.
    ///
    /// Read off the PREPARED DOCUMENT, before the SQL runs. That is the value
    /// the pipeline itself treated as the row's identity - the stage that binds
    /// per-row derivations to the primary key (encryption's `row_pk` AAD) reads
    /// it from exactly here - and taking it from before the round trip is what
    /// lets `rows[0]["id"] == id` be a real assertion instead of a tautology.
    id: String,
    /// The `RETURNING` row(s) the database handed back.
    rows: Vec<Value>,
}

/// Insert one document through the REAL write pipeline and the REAL insert
/// builder, and return the minted id with the `RETURNING` row.
///
/// This asserted `RETURNING *` and said "this suite is written against it".
/// The write path now names its columns, so the shape this suite is written
/// against is the projection - and the projection is what the assertion pins,
/// because a builder that quietly went back to `*` would put the raw column
/// back in every row below.
async fn insert_through_the_pipeline(
    host: &Host,
    pool: &Rc<Pool>,
    app: &str,
    collection: &str,
    schema: &Value,
    doc: Value,
) -> Inserted {
    let mut docs = value!([doc]);
    host.prepare_insert_many_docs(&mut docs, app, collection, None)
        .await
        .expect("write pipeline");
    let id = docs[0]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("the write pipeline must mint an id: {}", docs[0]))
        .to_string();
    let schema = &crate::tests::fixtures::generated_schema(schema.clone());
    let bq = compile_insert(
        crate::tests::fixtures::harness_binding(app).schema(),
        collection,
        schema,
        &docs[0],
    )
    .expect("insert builder");
    assert!(
        !bq.sql.contains("RETURNING *") && bq.sql.contains(r#"RETURNING "id""#),
        "the write path's shape is a named projection; this suite is written against it: {}",
        bq.sql,
    );
    let param_refs = &bq.params;
    let rows = zeroship_data_orm::backend::postgres::params::query(
        &pool.acquire().await.unwrap(),
        &bq.sql,
        param_refs,
    )
    .await
    .unwrap_or_else(|e| panic!("insert must apply: {e}\n{}", bq.sql));
    Inserted {
        id,
        rows: rows.iter().map(row_to_value).collect(),
    }
}

/// Every column of a returned row as a JSON string value, keyed by column name.
fn row_to_value(row: &compio_postgres::Row) -> Value {
    let mut map = crate::value::Map::new();
    for (i, column) in row.columns().iter().enumerate() {
        let value: Option<String> = row.try_get(i).unwrap_or(None);
        map.insert(
            column.name().to_string(),
            value.map_or(Value::Null, Value::String),
        );
    }
    Value::Object(map)
}

fn compile_insert(
    namespace: &SchemaName,
    collection: &str,
    schema: &crate::schema::FieldMap,
    document: &Value,
) -> Result<crate::sql::compiler::CompiledQuery, crate::sql::mapping::QueryError> {
    crate::crud::insert::build_one(
        namespace,
        collection,
        schema,
        document.clone(),
        &SqlRegistration::postgres(),
    )
}

#[allow(clippy::too_many_arguments)]
fn compile_find(
    namespace: &SchemaName,
    collection: &str,
    filter: &Value,
    limit: Option<i64>,
    offset: Option<i64>,
    order_by: Option<&Value>,
    select: Option<&Value>,
    schema: &crate::schema::FieldMap,
) -> Result<crate::sql::compiler::CompiledQuery, crate::sql::mapping::QueryError> {
    crate::crud::read::find(
        namespace,
        collection,
        schema,
        filter.clone(),
        limit,
        offset,
        order_by,
        select,
        &[],
        false,
        &SqlRegistration::postgres(),
    )
}

fn compile_distinct(
    namespace: &SchemaName,
    collection: &str,
    field: &str,
    filter: &Value,
    schema: &crate::schema::FieldMap,
) -> Result<crate::sql::compiler::CompiledQuery, crate::sql::mapping::QueryError> {
    crate::crud::read::distinct(
        namespace,
        collection,
        schema,
        field,
        filter.clone(),
        false,
        &SqlRegistration::postgres(),
    )
}

fn compile_aggregate(
    namespace: &SchemaName,
    collection: &str,
    pipeline: &Value,
    schema: &crate::schema::FieldMap,
) -> Result<crate::sql::compiler::CompiledQuery, crate::sql::mapping::QueryError> {
    crate::crud::aggregate::build(
        namespace,
        collection,
        pipeline,
        false,
        schema,
        &SqlRegistration::postgres(),
    )
    .map(|(query, _)| query)
}

fn compile_filter(
    filter: &Value,
    _parameters: &mut Vec<Value>,
    _schema: &crate::schema::FieldMap,
) -> Result<crate::sql::Predicate, crate::sql::mapping::QueryError> {
    crate::sql::filter::decode(filter)
}

async fn run_find(pool: &Rc<Pool>, app: &str, filter: &Value, schema: &Value) -> Vec<Value> {
    let schema = &crate::tests::fixtures::generated_schema(schema.clone());
    let bq = compile_find(
        crate::tests::fixtures::harness_binding(app).schema(),
        "people",
        filter,
        Some(50),
        None,
        None,
        None,
        schema,
    )
    .expect("find builder");
    let param_refs = &bq.params;
    let rows = zeroship_data_orm::backend::postgres::params::query(
        &pool.acquire().await.unwrap(),
        &bq.sql,
        param_refs,
    )
    .await
    .unwrap();
    rows.iter().map(row_to_value).collect()
}

/// Masked fields expose only their visible representation, so ordered predicates are invalid.
#[test]
fn a_range_filter_on_a_masked_column_is_refused() {
    let schema = crate::tests::fixtures::generated_schema(flip_schema());
    let error = compile_find(
        &crate::sql::SchemaName::new("flip_oracle").expect("fixture schema name"),
        "people",
        &value!({ "ssn": { "$gt": "500-00-0000" } }),
        Some(50),
        None,
        None,
        None,
        &schema,
    )
    .expect_err("masked range predicates must be refused");
    assert!(
        matches!(error, crate::sql::mapping::QueryError::InvalidFilter(ref message)
            if message == "comparison operator is not supported for this field type"),
        "unexpected error: {error}"
    );
}

/// The authorized unmask path still reaches the plaintext and writes its audit row.
#[test]
fn the_real_value_is_still_stored_and_still_reachable_by_the_audited_path() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg().await;
            let pool = Rc::new(Pool::connect(&url, 4).await.unwrap());
            let app = "flip_reachable";
            let alias = crate::tests::fixtures::harness_alias(app);
            let schema = flip_schema();
            fixture(host, &pool, &url, app, "people", &schema).await;

            // The platform mints the id; the row is addressed by that value from here
            // on. `row_pk` below is the same value, which matters beyond addressing:
            // it is the identity the write pipeline binds per-row derivations to, so a
            // stand-in would not merely miss the row, it would fail to decrypt one.
            let person = insert_through_the_pipeline(
                host,
                &pool,
                app,
                "people",
                &schema,
                value!({ "ssn": "123-45-6789", "nickname": "ada" }),
            )
            .await;

            let raw_col = raw_column_name("ssn");
            let stored = pool
                .query_text_params(
                    &format!(
                        "SELECT \"ssn\" AS mask, \"{raw_col}\" AS raw \
                 FROM \"{alias}\".\"people\" WHERE id = $1"
                    ),
                    &[person.id.as_str()],
                )
                .await
                .unwrap();
            assert_eq!(
                stored.len(),
                1,
                "the minted id must address the row the write created",
            );
            assert_eq!(
                stored[0].get::<_, String>("mask"),
                "***",
                "the field's own column holds the mask",
            );
            assert_eq!(
                stored[0].get::<_, String>("raw"),
                "123-45-6789",
                "the real value is stored, in a column no query surface can name",
            );

            // ---- and the audited path really does recover it, on Postgres ----
            //
            // Without this arm nothing binds `protection::unmask`'s PG SQL to the raw
            // column: pointing it back at the field's own column reddens no other test
            // in this file, because every other assertion here is about what a query
            // CANNOT reach. The unmask path is the one reader that must reach it.
            pool.batch_execute(&zeroship_migrate_server::provisioning::audit_unmask_table_sql(
                &alias,
            ))
            .await
            .expect("the audit table the deploy provisions");
            // The unmask fetch narrows to the binding role, so that role and the
            // capability role carrying its grants have to exist - the deploy's
            // `zeroship migrate` creates them, and this stands in for it.
            crate::tests::fixtures::roles::ensure_binding_ladder(&pool, &crate::tests::fixtures::harness_binding(app))
                .await
                .expect("the binding ladder, as the deploy would provision it");
            fixtures::grant_runtime_select_columns(&pool, &crate::tests::fixtures::harness_binding(app), "people", &["id", &raw_col]).await;

            let result = zeroship_data_orm::protection::unmask::dispatch_unmask(
                &unmask_route(host, app).await,
                &crate::tests::fixtures::harness_binding(app),
                zeroship_data_orm::protection::unmask::UnmaskFieldArgs {
                    collection: "people".to_string(),
                    row_pk: person.id.clone(),
                    column: "ssn".to_string(),
                    actor: Some(value!({ "kind": "auto", "id": null })),
                    reason: Some("mask_flip integration test".to_string()),
                    rejected_claim: None,
                },
            )
            .await
            .expect("the audited unmask path must still return plaintext");
            assert_eq!(
                result.plaintext, "123-45-6789",
                "unmask must read the RAW column; reading the field's own column would \
         return the mask and the feature would be closed rather than secured",
            );

            // And the audit row the guarantee rests on was written.
            let audit = pool
                .query_text_params(
                    &format!(
                        "SELECT outcome, \"column\" FROM \"{alias}\".\"__zeroship_audit_unmask\""
                    ),
                    &[],
                )
                .await
                .unwrap();
            assert_eq!(audit.len(), 1, "exactly one audit row");
            assert_eq!(audit[0].get::<_, String>("outcome"), "granted");
            assert_eq!(
                audit[0].get::<_, String>("column"),
                "ssn",
                "the audit row names the LOGICAL field, not the physical column",
            );

            release_pg(host, pool).await;
        })
    })
}

// ---------------------------------------------------------------------------
// 1b. The other half of the audited path: the REFUSAL
// ---------------------------------------------------------------------------
//
// The arm above is the only exercise `dispatch_unmask` had on Postgres, and the
// only actor it passes is `{"kind":"auto"}`. The no-policy fallback inside
// `check_unmask_authorization` is literally `Ok(kind == "auto")`, so that arm
// tests the check against its own allow-literal. The refusal branch - the
// denied audit row and the `unmask_not_permitted` error - needs its own arm
// against real Postgres.

/// Stand up everything the audited unmask path needs on a real server, and put
/// one row behind it whose real SSN is genuinely recoverable through that path.
///
/// The grants are deliberately SUFFICIENT FOR A LEAK: the runtime role can read
/// the raw column, and the row exists under the id the platform minted. A build
/// whose authorization check stops refusing therefore hands the plaintext back,
/// rather than failing later on a missing row or a permission error. A denial
/// test whose fixture could not leak in the first place proves nothing about
/// the check - it would pass against an implementation that had no check and no
/// data either.
async fn audited_unmask_fixture(
    host: &Host,
    pool: &Rc<Pool>,
    url: &str,
    app: &str,
    schema: &Value,
    ssn: &str,
) -> Inserted {
    audited_unmask_fixture_with(
        host,
        pool,
        url,
        app,
        schema,
        value!({ "ssn": ssn, "nickname": "ada" }),
        &[("ssn", ssn)],
    )
    .await
}

/// [`audited_unmask_fixture`] for a row carrying MORE THAN ONE masked column,
/// which the batch and query-hint arms need: their whole subject is a request
/// spanning two classifications, one the policy permits and one it does not.
///
/// `masked` names every masked field and the plaintext its row must hold. Each
/// one is granted to the runtime role and read back as the admin principal, so
/// the leak-capability requirement in [`audited_unmask_fixture`]'s doc holds
/// per COLUMN: "neither value came back" is then an assertion about two values
/// that were both genuinely there to come back, and a batch that withheld the
/// permitted one is withholding something it could have returned.
async fn audited_unmask_fixture_with(
    host: &Host,
    pool: &Rc<Pool>,
    url: &str,
    app: &str,
    schema: &Value,
    doc: Value,
    masked: &[(&str, &str)],
) -> Inserted {
    let alias = crate::tests::fixtures::harness_alias(app);
    fixture(host, pool, url, app, "people", schema).await;
    let person = insert_through_the_pipeline(host, pool, app, "people", schema, doc).await;
    pool.batch_execute(&zeroship_migrate_server::provisioning::audit_unmask_table_sql(&alias))
        .await
        .expect("the audit table the deploy provisions");
    // The audit INSERT and value SELECT both run under the binding role. The
    // deploy provisions that role after creating the schema tables; this
    // fixture follows the same order.
    crate::tests::fixtures::roles::ensure_binding_ladder(pool, &crate::tests::fixtures::harness_binding(app))
        .await
        .expect("the binding ladder, as the deploy would provision it");
    let raw_columns: Vec<String> = masked
        .iter()
        .map(|(column, _)| raw_column_name(column))
        .collect();
    let mut readable: Vec<&str> = vec!["id"];
    readable.extend(raw_columns.iter().map(String::as_str));
    fixtures::grant_runtime_select_columns(pool, &crate::tests::fixtures::harness_binding(app), "people", &readable).await;

    // Control zero, read as the admin principal: the plaintext really is on
    // disk under the minted id. Every refusal asserted below is therefore a
    // refusal, not an empty table.
    for ((column, plaintext), raw_col) in masked.iter().zip(&raw_columns) {
        let stored = pool
            .query_text_params(
                &format!("SELECT \"{raw_col}\" AS raw FROM \"{alias}\".\"people\" WHERE id = $1"),
                &[person.id.as_str()],
            )
            .await
            .expect("read the raw column directly");
        assert_eq!(stored.len(), 1, "the fixture row must exist");
        assert_eq!(
            stored[0].get::<_, String>("raw"),
            *plaintext,
            "the fixture must have a real value for '{column}' for the denied \
             path to be denying access TO something",
        );
    }
    person
}

/// The audit rows the app's schema HOLDS, oldest first.
///
/// Read back out of PostgreSQL as the admin principal - never from anything
/// `dispatch_unmask` returned. On the denied path it returns an `Err` that
/// carries no audit information at all, so a test that inspected the return
/// value could not tell a written row from an unwritten one.
async fn audit_rows(pool: &Rc<Pool>, app: &str) -> Vec<Value> {
    let alias = crate::tests::fixtures::harness_alias(app);
    let rows = pool
        .query_text_params(
            &format!(
                "SELECT actor_id, actor_role, claimed_actor, collection, row_pk, \"column\", \
                 classification, reason, outcome \
                 FROM \"{alias}\".\"__zeroship_audit_unmask\" ORDER BY id"
            ),
            &[],
        )
        .await
        .expect("read the audit table");
    rows.iter().map(row_to_value).collect()
}

fn unmask_args(row_pk: &str, actor: Option<Value>) -> UnmaskFieldArgs {
    UnmaskFieldArgs {
        collection: "people".to_string(),
        row_pk: row_pk.to_string(),
        column: "ssn".to_string(),
        actor,
        reason: Some("mask_flip integration test".to_string()),
        rejected_claim: None,
    }
}

/// The `code` of a refusal, whatever variant carried it. Written this way so a
/// denial that arrives as the WRONG typed error is reported by name instead of
/// matching a wildcard arm.
fn refusal_code(err: &DbError) -> String {
    match err {
        DbError::Coded { code, .. } => code.clone(),
        DbError::ValidationFailed { code, .. } => (*code).to_string(),
        other => panic!("expected a coded refusal, got {other:?}"),
    }
}

/// **The denied branch, on Postgres.** An actor the app's policy does not
/// permit is refused, the refusal is audited, and no plaintext comes back.
///
/// The closing arm is the control, differing in exactly ONE variable: the same
/// actor, the same row and the same column, after a policy grants that actor
/// role the column's classification. It succeeds. So the refusal above is a
/// property of the AUTHORIZATION DECISION and not of a fixture that could not
/// have produced the plaintext anyway - and it puts the policy path itself
/// (`MaskPolicy::allows`, via `install_mask_policy`) on Postgres for the
/// first time, rather than only the no-policy fallback.
///
/// It belongs in this file rather than beside the SQLite policy tests because
/// what is unverified is the POSTGRES dispatch: the policy cache is shared code
/// that the SQLite suite already covers, while the denied path's audit INSERT,
/// its `SET LOCAL ROLE` funnel and its per-app grants are all PG-specific and
/// exist nowhere in that suite.
#[test]
fn an_actor_the_policy_does_not_permit_is_refused_and_the_refusal_is_audited() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg().await;
            let pool = Rc::new(Pool::connect(&url, 4).await.unwrap());
            let app = "flip_denied";
            let schema = flip_schema();
            let ssn = "123-45-6789";
            let person = audited_unmask_fixture(host, &pool, &url, app, &schema, ssn).await;

            // `support` is not `auto`, and no policy is installed - so the no-policy
            // fallback denies it. This is the case `mask_flip` never had.
            let err = dispatch_unmask(
                &unmask_route(host, app).await,
                &crate::tests::fixtures::harness_binding(app),
                unmask_args(
                    &person.id,
                    Some(value!({ "kind": "support", "id": "usr_support_1" })),
                ),
            )
            .await
            .expect_err(
                "an actor no policy permits must be REFUSED; a build that returns \
         plaintext here has no authorization check on its one privileged \
         read path",
            );
            assert_eq!(
                refusal_code(&err),
                "unmask_not_permitted",
                "the refusal must be the authorization refusal, not an incidental \
         failure further down the path: {err:?}",
            );
            // The plaintext did not come back. The `Err` has no field that could carry
            // it, so this asserts the weaker reachable thing: the value appears nowhere
            // in the error the caller receives, message and hint included.
            assert!(
                !format!("{err:?}").contains(ssn),
                "the refusal must not carry the value it refused: {err:?}",
            );

            // The audit row the guarantee rests on, read back from the database.
            let audit = audit_rows(&pool, app).await;
            assert_eq!(audit.len(), 1, "the denied path writes exactly one row");
            assert_eq!(audit[0]["outcome"], value!("denied"));
            assert_eq!(audit[0]["actor_role"], value!("support"));
            assert_eq!(audit[0]["actor_id"], value!("usr_support_1"));
            assert_eq!(
                audit[0]["classification"],
                value!("pci"),
                "the audit row records the classification that was refused",
            );
            assert_eq!(
                audit[0]["column"],
                value!("ssn"),
                "the audit row names the LOGICAL field, not the physical column",
            );
            assert_eq!(audit[0]["collection"], value!("people"));
            assert_eq!(
                audit[0]["row_pk"],
                value!(person.id),
                "and the row it names is the one the platform minted",
            );

            // A new deployment declares the grant; the same actor can now unmask.
            let redeployed = crate::tests::fixtures::harness_binding_at_deploy(app, "granted_policy");
            crate::tests::fixtures::cache_schema_for_deploy(
                &redeployed,
                "people",
                schema.clone(),
            );
            install_mask_policy(&redeployed, value!({ "support": ["pci"] }))
                .expect("install the new deployment's policy");
            let result = dispatch_unmask(
                &unmask_route(host, app).await,
                &redeployed,
                unmask_args(
                    &person.id,
                    Some(value!({ "kind": "support", "id": "usr_support_1" })),
                ),
            )
            .await
            .expect("the same actor must pass once the policy grants it the class");
            assert_eq!(
                result.plaintext, ssn,
                "the policy path must reach the same value the fallback refused",
            );
            let audit = audit_rows(&pool, app).await;
            assert_eq!(audit.len(), 2, "the granted path appends its own row");
            assert_eq!(audit[1]["outcome"], value!("granted"));

            // And the grant is scoped to the classification the policy named: the same
            // role is still refused a class the policy does not list. Without this the
            // control could pass against an `allows` that ignores its arguments.
            crate::tests::fixtures::cache_schema_for_deploy(
                &redeployed,
                "vitals",
                value!({ "hr": { "type": "string", "mask": { "kind": "full", "classification": "phi" } } }),
            );
            let err = dispatch_unmask(
                &unmask_route(host, app).await,
                &redeployed,
                UnmaskFieldArgs {
                    collection: "vitals".to_string(),
                    row_pk: person.id.clone(),
                    column: "hr".to_string(),
                    actor: Some(value!({ "kind": "support", "id": "usr_support_1" })),
                    reason: None,
                    rejected_claim: None,
                },
            )
            .await
            .expect_err("a class the policy does not list must still be refused");
            assert_eq!(refusal_code(&err), "unmask_not_permitted");

            release_pg(host, pool).await;
        })
    })
}

/// **The unauthenticated branch.** `check_unmask_authorization` returns
/// `Ok(false)` before it ever looks at a policy when the actor is absent or is
/// not an object - a different arm of the function from the wrong-kind case
/// above, and the arm `sanitize_app_actor` deliberately routes app JS into when
/// it claims the reserved `auto` kind (DB-3).
///
/// Both shapes run against the same leak-capable fixture, and the closing
/// control shows that fixture handing the plaintext to an actor a policy does
/// permit. So "refused" here is about the ACTOR, not about the row.
#[test]
fn an_unmask_with_no_usable_actor_is_refused_and_audited() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg().await;
            let pool = Rc::new(Pool::connect(&url, 4).await.unwrap());
            let app = "flip_unauth";
            let schema = flip_schema();
            let ssn = "987-65-4321";
            let person = audited_unmask_fixture(host, &pool, &url, app, &schema, ssn).await;

            install_mask_policy(&crate::tests::fixtures::harness_binding(app), value!({ "support": ["pci"] }))
                .expect("install the app's declared mask policy");

            // `None` is exactly what `sanitize_app_actor` produces from an app-JS
            // actor claiming `kind: "auto"`, so this is also the shape DB-3's patch
            // hands the check.
            for (label, actor) in [
                ("absent", None),
                ("not an object", Some(value!("support"))),
                ("object with no kind", Some(value!({ "id": "usr_1" }))),
            ] {
                let err = match dispatch_unmask(
                    &unmask_route(host, app).await,
                    &crate::tests::fixtures::harness_binding(app),
                    unmask_args(&person.id, actor),
                )
                .await
                {
                    Ok(leaked) => panic!(
                        "an actor that is {label} must be refused; the call returned \
                 the plaintext instead: {leaked:?}"
                    ),
                    Err(e) => e,
                };
                assert_eq!(
                    refusal_code(&err),
                    "unmask_not_permitted",
                    "actor {label}: expected the authorization refusal, got {err:?}",
                );
                assert!(
                    !format!("{err:?}").contains(ssn),
                    "actor {label}: the refusal must not carry the value: {err:?}",
                );
            }

            // Three attempts, three denied rows. The first two have no actor at all,
            // so both actor columns land empty; the third carries an id and no kind.
            let audit = audit_rows(&pool, app).await;
            assert_eq!(audit.len(), 3, "every refusal is audited: {audit:?}");
            for row in &audit {
                assert_eq!(row["outcome"], value!("denied"), "{row:?}");
                assert_eq!(row["classification"], value!("pci"), "{row:?}");
            }
            assert_eq!(audit[0]["actor_role"], value!(""));
            assert_eq!(audit[0]["actor_id"], value!(""));
            assert_eq!(audit[2]["actor_id"], value!("usr_1"));
            assert_eq!(
                audit[2]["actor_role"],
                value!(""),
                "an actor with no kind is audited with an empty role, not a forged one",
            );

            // ---- THE CONTROL: the same fixture DOES hand out the plaintext ----
            let result = dispatch_unmask(
                &unmask_route(host, app).await,
                &crate::tests::fixtures::harness_binding(app),
                unmask_args(
                    &person.id,
                    Some(value!({ "kind": "support", "id": "usr_2" })),
                ),
            )
            .await
            .expect("a permitted actor must still get the value");
            assert_eq!(result.plaintext, ssn);
            let audit = audit_rows(&pool, app).await;
            assert_eq!(audit.len(), 4);
            assert_eq!(audit[3]["outcome"], value!("granted"));

            release_pg(host, pool).await;
        })
    })
}

// ---------------------------------------------------------------------------
// 1b-2. DB-3: the actor an app SENDS, through the parser that sanitises it
// ---------------------------------------------------------------------------
//
// Every arm above builds `UnmaskFieldArgs` / `BulkUnmaskArgs` in Rust, which
// enters the dispatch path BELOW `sanitize_app_actor`. That helper is the DB-3
// fix - app JS reached a privileged unmask and could pass `actor:{kind:"auto"}`
// to read its own PII/PHI/PCI, because `check_unmask_authorization`'s no-policy
// fallback is literally `Ok(kind == "auto")` and `MaskPolicy::allows` keeps that
// rule for any policy that does not list `auto`. Until now the helper's only
// coverage was in-module units that call it directly with no database behind
// them, so the whole live path from "the JSON a handler sends" to "the row on
// disk" was unbound: deleting the call reddened nothing in this file.
//
// The two arms below start from the JSON, hand it to the REAL parser, and
// dispatch what comes out against Postgres. Each one's control differs from its
// refusal in exactly ONE token - `actor.kind` - under one policy installed
// before either call, so neither the fixture, the row, the column, the reason,
// the actor id nor the policy can be what made the difference.

/// The args object `Collection.unmaskField` actually hands `parse_args`.
///
/// The V8 method takes the caller's `opts` (`{ actor?, reason? }`) and stamps
/// `collection` / `row_pk` / `column` onto that same map
/// (`src/v8_classes/collection.rs`, `unmask_field`), so `actor` arrives exactly
/// as app JS wrote it. Note the key is `row_pk`, not `rowPk`: the camelCase
/// spelling is the JS method's positional parameter, and by the time the object
/// reaches the parser it has been stamped in `snake_case`.
fn unmask_args_json(row_pk: &str, actor: &Value) -> Value {
    value!({
        "collection": "people",
        "row_pk": row_pk,
        "column": "ssn",
        "actor": actor,
        "reason": "mask_flip integration test",
    })
}

/// The args object `Collection.bulkUnmask` hands `parse_bulk_args`. Same
/// stamping, and here the per-item key really is `rowPk` - that is the shape
/// the SDK documents and the parser reads.
fn bulk_args_json(row_pk: &str, columns: &[&str], actor: &Value) -> Value {
    value!({
        "collection": "people",
        "items": [{ "rowPk": row_pk, "columns": columns }],
        "actor": actor,
        "reason": "mask_flip integration test",
    })
}

/// **DB-3, on Postgres, through the parser.** An app handler that sends
/// `actor: { kind: "auto" }` is refused, the refusal is audited, and the
/// plaintext does not come back.
///
/// The payload is the one an app really sends, and it is not rejected for its
/// SHAPE: `parse_args` accepts it and only then strips the reserved actor, so
/// what this binds is the authorization outcome rather than a validation error
/// that would happen to look the same.
///
/// **The control differs in one token.** The policy `{"support":["pci"]}` is
/// installed BEFORE either call and is held constant across both. It does not
/// list `auto`, and `MaskPolicy::allows` returns `role == "auto"` for a role it
/// does not list - so this policy PERMITS the forged actor the moment the
/// sanitiser stops running. That is what makes the refusal a statement about
/// `sanitize_app_actor` and not about a fixture that could not have leaked: the
/// identical payload naming `support` instead of `auto` returns the SSN.
#[test]
fn app_js_claiming_the_auto_system_actor_is_refused_by_the_parser() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg().await;
            let pool = Rc::new(Pool::connect(&url, 4).await.unwrap());
            let app = "flip_db3_single";
            host.clear_mask_policy_cache(app);
            let schema = flip_schema();
            let ssn = "123-45-6789";
            let person = audited_unmask_fixture(host, &pool, &url, app, &schema, ssn).await;

            install_mask_policy(&crate::tests::fixtures::harness_binding(app), value!({ "support": ["pci"] }))
                .expect("install the app's declared mask policy");

            // ---- the forged system actor, parsed from the JSON a handler sends ----
            let forged = parse_args(&unmask_args_json(
                &person.id,
                &value!({ "kind": "auto", "id": "usr_support_1" }),
            ))
            .expect(
                "the payload must PARSE: DB-3 is an authorization fence, not a shape \
         rejection, and a test that never got past the parser would assert \
         nothing about it",
            );
            let err = match dispatch_unmask(
                &unmask_route(host, app).await,
                &crate::tests::fixtures::harness_binding(app),
                forged,
            )
            .await
            {
                Ok(leaked) => panic!(
                    "DB-3 is back: app JS claiming the reserved `auto` system actor was \
             authorized, and the plaintext came back: {leaked:?}"
                ),
                Err(e) => e,
            };
            assert_eq!(
                refusal_code(&err),
                "unmask_not_permitted",
                "the refusal must be the authorization refusal - not a parse error, and \
         not an incidental failure further down the path: {err:?}",
            );
            assert!(
                !format!("{err:?}").contains(ssn),
                "the refusal must not carry the value it refused: {err:?}",
            );

            // The audit row, read back from PostgreSQL rather than from anything the
            // call returned - the `Err` carries no audit information at all.
            let audit = audit_rows(&pool, app).await;
            assert_eq!(
                audit.len(),
                1,
                "the denied path writes exactly one row: {audit:?}",
            );
            assert_eq!(audit[0]["outcome"], value!("denied"));
            assert_eq!(audit[0]["collection"], value!("people"));
            assert_eq!(audit[0]["column"], value!("ssn"));
            assert_eq!(
                audit[0]["classification"],
                value!("pci"),
                "the audit row records the classification that was refused",
            );
            assert_eq!(
                audit[0]["row_pk"],
                value!(person.id),
                "and the row it names is the one the platform minted",
            );
            // The sanitiser strips the WHOLE actor, not just its `kind`: the id the
            // caller supplied never reaches the audit row either. So a forged-`auto`
            // attempt is recorded exactly like a call with no actor at all.
            assert_eq!(
                audit[0]["actor_role"],
                value!(""),
                "the forged `auto` claim must not be recorded as the actor's role: {audit:?}",
            );
            assert_eq!(
                audit[0]["actor_id"],
                value!(""),
                "nor the id that travelled with it: {audit:?}",
            );

            // ---- THE CONTROL, differing in one token: `auto` -> `support` ----
            let permitted = parse_args(&unmask_args_json(
                &person.id,
                &value!({ "kind": "support", "id": "usr_support_1" }),
            ))
            .expect("the same payload shape must parse");
            let result = dispatch_unmask(
                &unmask_route(host, app).await,
                &crate::tests::fixtures::harness_binding(app),
                permitted,
            )
            .await
            .expect(
                "the same payload naming a non-reserved kind the policy permits must \
             reach the value; if this fails the refusal above proved nothing \
             about the actor",
            );
            assert_eq!(
                result.plaintext, ssn,
                "the control must recover the very value the forged call was refused",
            );
            let audit = audit_rows(&pool, app).await;
            assert_eq!(audit.len(), 2, "the granted path appends its own row");
            assert_eq!(audit[1]["outcome"], value!("granted"));
            assert_eq!(
                audit[1]["actor_role"],
                value!("support"),
                "and a NON-reserved kind does travel through to the audit row, so the \
         empty role above is the sanitiser and not an audit path that never \
         records one: {audit:?}",
            );

            release_pg(host, pool).await;
        })
    })
}

/// **DB-3 on the bulk path.** `parse_bulk_args` is the second sanitising entry
/// point, and it is reached by a different V8 method (`Collection.bulkUnmask`)
/// with a different args shape, so the single-cell arm above says nothing about
/// it: deleting one call leaves the other's tests green.
///
/// Same construction - one policy, installed first and held constant, and a
/// control differing only in `actor.kind`. The refusal code differs because the
/// batch owns its own atomic fence: every `(row, column)` pair is unauthorized
/// once the actor is stripped, so the whole call is refused with
/// `bulk_unmask_partial_unauthorized`.
#[test]
fn app_js_claiming_the_auto_system_actor_is_refused_by_the_bulk_parser() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg().await;
            let pool = Rc::new(Pool::connect(&url, 4).await.unwrap());
            let app = "flip_db3_bulk";
            host.clear_mask_policy_cache(app);
            let schema = flip_schema();
            let ssn = "987-65-4321";
            let person = audited_unmask_fixture(host, &pool, &url, app, &schema, ssn).await;

            install_mask_policy(&crate::tests::fixtures::harness_binding(app), value!({ "support": ["pci"] }))
                .expect("install the app's declared mask policy");

            // ---- the forged system actor, parsed from the JSON a handler sends ----
            let forged = parse_bulk_args(&bulk_args_json(
                &person.id,
                &["ssn"],
                &value!({ "kind": "auto", "id": "usr_support_1" }),
            ))
            .expect("the payload must PARSE; DB-3 is an authorization fence");
            let err = match dispatch_bulk_unmask(
                &unmask_route(host, app).await,
                &crate::tests::fixtures::harness_binding(app),
                forged,
            )
            .await
            {
                Ok(leaked) => panic!(
                    "DB-3 is back on the bulk path: app JS claiming the reserved `auto` \
             system actor was authorized, and the plaintext came back: \
             {:?}",
                    leaked.results
                ),
                Err(e) => e,
            };
            assert_eq!(
                refusal_code(&err),
                "bulk_unmask_partial_unauthorized",
                "the refusal must be the batch fence's own code, not a parse error and \
         not an incidental failure further down the path: {err:?}",
            );
            assert!(
                !format!("{err:?}").contains(ssn),
                "the refusal must not carry the value it refused: {err:?}",
            );

            let audit = audit_rows(&pool, app).await;
            assert_eq!(
                audit.len(),
                1,
                "the refused batch writes exactly one row for the whole call: {audit:?}",
            );
            assert_eq!(audit[0]["outcome"], value!("denied"));
            assert_eq!(audit[0]["collection"], value!("people"));
            assert_eq!(audit[0]["column"], value!("ssn"));
            assert_eq!(audit[0]["classification"], value!("pci"));
            assert_eq!(audit[0]["row_pk"], value!(person.id));
            assert_eq!(
                audit[0]["actor_role"],
                value!(""),
                "the forged `auto` claim must not be recorded as the actor's role: {audit:?}",
            );
            assert_eq!(audit[0]["actor_id"], value!(""));
            let reason = audit[0]["reason"]
                .as_str()
                .expect("the audit row carries a reason");
            assert!(
                reason.contains(&format!("unauthorized=[{}/ssn]", person.id)),
                "the reason names the (row, column) pair that caused the refusal: {reason:?}",
            );

            // ---- THE CONTROL, differing in one token: `auto` -> `support` ----
            let permitted = parse_bulk_args(&bulk_args_json(
                &person.id,
                &["ssn"],
                &value!({ "kind": "support", "id": "usr_support_1" }),
            ))
            .expect("the same payload shape must parse");
            let granted = dispatch_bulk_unmask(
                &unmask_route(host, app).await,
                &crate::tests::fixtures::harness_binding(app),
                permitted,
            )
            .await
            .expect(
                "the same batch naming a non-reserved kind the policy permits must \
             reach the value; if this fails the refusal above proved nothing",
            );
            assert_eq!(
                granted.results[&person.id]["ssn"], ssn,
                "the control must recover the very value the forged batch was refused: {:?}",
                granted.results,
            );
            let audit = audit_rows(&pool, app).await;
            assert_eq!(audit.len(), 2, "the granted batch appends its own row");
            assert_eq!(audit[1]["outcome"], value!("granted"));
            assert_eq!(
                audit[1]["actor_role"],
                value!("support"),
                "and a NON-reserved kind does travel through to the audit row: {audit:?}",
            );

            release_pg(host, pool).await;
        })
    })
}

/// A REJECTED impersonation must be distinguishable, in the audit trail, from a
/// caller who simply sent no actor.
///
/// `sanitize_app_actor` strips the whole actor - `id` included - when the claim
/// names a reserved system kind, so both cases reach the audit writer as
/// `actor: None` and both rows come out `actor_id=""`, `actor_role=""`.
///
/// `__zeroship_audit_unmask` is the ONE durable record that someone tried to
/// exploit DB-3, and DB-3 is the bug the "privilege follows the PROCESS"
/// invariant is written about. An operator reading this table has to be able to
/// tell "a handler sent no actor", which happens on every anonymous path and is
/// routine, from "a handler claimed `kind: auto` and named `usr_support_1` while
/// doing it", which is an intrusion attempt. The fence working is exactly why
/// the attempt leaves no other trace.
///
/// The two calls below differ in ONE token - the actor - against the same row,
/// the same column and the same policy, so any difference between the two rows
/// can only come from the actor. Stripping the claim is correct and must stay;
/// what must change is that the REJECTED claim is recorded rather than dropped.
#[test]
fn a_rejected_impersonation_is_distinguishable_from_an_absent_actor() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg().await;
            let pool = Rc::new(Pool::connect(&url, 4).await.unwrap());
            let app = "flip_db3_signal";
            host.clear_mask_policy_cache(app);
            let schema = flip_schema();
            let person =
                audited_unmask_fixture(host, &pool, &url, app, &schema, "123-45-6789").await;

            install_mask_policy(&crate::tests::fixtures::harness_binding(app), value!({ "support": ["pci"] }))
                .expect("install the app's declared mask policy");

            // ---- (1) a forged claim on the reserved system kind, naming a real user
            let forged = parse_args(&unmask_args_json(
                &person.id,
                &value!({ "kind": "auto", "id": "usr_support_1" }),
            ))
            .expect("the forged payload must parse; DB-3 is a fence, not a shape check");
            dispatch_unmask(
                &unmask_route(host, app).await,
                &crate::tests::fixtures::harness_binding(app),
                forged,
            )
            .await
            .expect_err("the forged claim must be refused");

            // ---- (2) no actor at all, same row, same column, same policy
            let anonymous = parse_args(&unmask_args_json(&person.id, &Value::Null))
                .expect("an actor-less payload must parse");
            dispatch_unmask(
                &unmask_route(host, app).await,
                &crate::tests::fixtures::harness_binding(app),
                anonymous,
            )
            .await
            .expect_err("an absent actor must be refused");

            let audit = audit_rows(&pool, app).await;
            assert_eq!(audit.len(), 2, "both refusals are audited: {audit:?}");

            // Neither row may present the forged claim as identity. This half already
            // holds and must keep holding - the fix is a new column, never a relaxation
            // of these two.
            for row in &audit {
                assert_eq!(row["actor_id"], value!(""), "{row:?}");
                assert_eq!(row["actor_role"], value!(""), "{row:?}");
            }

            assert_ne!(
                audit[0], audit[1],
                "the forged-impersonation row and the no-actor row are IDENTICAL, so the \
         audit trail cannot record that anyone attempted DB-3. Stripping the \
         claim is right; discarding it is what has to change. Rows: {audit:?}",
            );

            release_pg(host, pool).await;
        })
    })
}

// ---------------------------------------------------------------------------
// 1c. The OTHER two callers of the same check: the batch and the query hint
// ---------------------------------------------------------------------------
//
// `check_unmask_authorization` has three call sites. Section 1b bound the
// single-cell one. The other two - `dispatch_bulk_unmask`
// (`bulkUnmaskFields`) and `authorize_query_hint` (`find({ unmask: [...] })`)
// - are creator-facing read paths and need their own arms. Both are of the
// shape DB-3 records.
//
// Neither authorises a CELL. Each authorises a REQUEST, and one denied column
// refuses the whole call - so each arm below drives a HALF-AUTHORISED request,
// one column the policy permits and one it does not. That is the only shape in
// which an atomic fence and a per-column one behave differently; a request over
// a single classification cannot tell them apart.

/// A one-row batch over `people`, so the arms below differ only in the column
/// list and the actor.
fn bulk_args(row_pk: &str, columns: &[&str], actor: Option<Value>) -> BulkUnmaskArgs {
    BulkUnmaskArgs {
        collection: "people".to_string(),
        items: vec![BulkUnmaskItem {
            row_pk: row_pk.to_string(),
            columns: columns.iter().map(|c| (*c).to_string()).collect(),
        }],
        actor,
        reason: Some("mask_flip integration test".to_string()),
        // Args built in Rust never pass the sanitiser, so there is no refused
        // claim to carry. The DB-3 tests drive `parse_bulk_args` instead.
        rejected_claim: None,
    }
}

/// **The batch fence, on Postgres.** A `bulkUnmaskFields` call naming one
/// permitted column and one the policy forbids is refused ENTIRELY: the error
/// carries `bulk_unmask_partial_unauthorized`, one denied audit row lands, and
/// neither value comes back - not the forbidden one, and not the permitted one
/// either.
///
/// That second half is the property worth binding. The all-or-nothing decision
/// is `if !unauthorized.is_empty()` at
/// `crates/zeroship-data-orm/src/protection/unmask.rs`, which returns before
/// the decrypt loop runs at all, and the reason is in that
/// function's own doc: a partial grant leaks the authorisation verdict through
/// which columns came back populated, which is a read oracle over the policy
/// itself.
///
/// The fence is exercised on BOTH axes - a batch mixing two columns on one
/// row, and a batch mixing two rows where only one carries the forbidden
/// column - because a single-row batch cannot tell an all-or-nothing fence from
/// a per-row one. A third arm pins that the fence COLLECTS every denied pair
/// rather than stopping at the first, which every single-denial arm is blind
/// to. The two controls differ from their refusal in exactly one
/// variable each and prove the withheld half was withholdable: the same actor
/// under the same policy DOES get `email` when the batch does not also ask for
/// `ssn`, and the same two-row batch hands over both values once the policy
/// grants both classes.
#[test]
fn a_bulk_unmask_batch_with_one_forbidden_column_is_refused_whole() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg().await;
            let pool = Rc::new(Pool::connect(&url, 4).await.unwrap());
            let app = "flip_bulk_denied";
            host.clear_mask_policy_cache(app);
            let schema = two_class_schema();
            let (ssn, email) = ("123-45-6789", "ada@example.com");
            let person = audited_unmask_fixture_with(
                host,
                &pool,
                &url,
                app,
                &schema,
                value!({ "ssn": ssn, "email": email, "nickname": "ada" }),
                &[("ssn", ssn), ("email", email)],
            )
            .await;

            // The policy grants `support` exactly ONE of the two classifications:
            // `email` is pii and permitted, `ssn` is pci and is not.
            install_mask_policy(&crate::tests::fixtures::harness_binding(app), value!({ "support": ["pii"] }))
                .expect("install the app's declared mask policy");
            let actor = value!({ "kind": "support", "id": "usr_support_1" });

            // ---- the half-authorised batch ----
            let err = dispatch_bulk_unmask(
                &unmask_route(host, app).await,
                &crate::tests::fixtures::harness_binding(app),
                bulk_args(&person.id, &["email", "ssn"], Some(actor.clone())),
            )
            .await
            .expect_err(
                "a batch naming one forbidden column must be refused; a build that \
         hands back the permitted half has no atomic fence, and which columns \
         came back is itself a read oracle over the policy",
            );
            assert_eq!(
                refusal_code(&err),
                "bulk_unmask_partial_unauthorized",
                "the refusal must be the batch fence's own code, not an incidental \
         failure further down the path: {err:?}",
            );
            let rendered = format!("{err:?}");
            assert!(
                !rendered.contains(ssn),
                "the refusal must not carry the value it refused: {err:?}",
            );
            assert!(
                !rendered.contains(email),
                "nor the value it would have permitted on its own: {err:?}",
            );

            // The audit row the guarantee rests on, read back from the database.
            let audit = audit_rows(&pool, app).await;
            assert_eq!(
                audit.len(),
                1,
                "the refused batch writes exactly one row for the whole call: {audit:?}",
            );
            assert_eq!(audit[0]["outcome"], value!("denied"));
            assert_eq!(audit[0]["actor_role"], value!("support"));
            assert_eq!(audit[0]["actor_id"], value!("usr_support_1"));
            assert_eq!(audit[0]["collection"], value!("people"));
            assert_eq!(
                audit[0]["row_pk"],
                value!(person.id),
                "and the row it names is the one the platform minted",
            );
            assert_eq!(
                audit[0]["column"],
                value!("email,ssn"),
                "the row names every column the batch ASKED for, not only the refused one",
            );
            assert_eq!(
                audit[0]["classification"],
                value!("pci,pii"),
                "and the union of the classifications the batch spanned",
            );
            let reason = audit[0]["reason"]
                .as_str()
                .expect("the audit row carries a reason");
            assert!(
                reason.contains(&format!("unauthorized=[{}/ssn]", person.id)),
                "the reason names the (row, column) pair that caused the refusal: {reason:?}",
            );
            assert!(
                !reason.contains("/email"),
                "and does not report the permitted pair as unauthorized: {reason:?}",
            );

            // ---- CONTROL 1, differing in one variable: the batch drops the forbidden
            // column. Same actor, same row, same policy - and now the value arrives.
            // So the refusal above withheld a column this very call could return,
            // which is what makes the fence ATOMIC rather than merely right per column.
            let granted = dispatch_bulk_unmask(
                &unmask_route(host, app).await,
                &crate::tests::fixtures::harness_binding(app),
                bulk_args(&person.id, &["email"], Some(actor.clone())),
            )
            .await
            .expect("the permitted column alone must be returned");
            assert_eq!(
                granted.results[&person.id]["email"], email,
                "the permitted column really was reachable for this actor: {:?}",
                granted.results,
            );
            assert!(
                !granted.results[&person.id].contains_key("ssn"),
                "and the forbidden column is not smuggled in beside it: {:?}",
                granted.results,
            );
            let audit = audit_rows(&pool, app).await;
            assert_eq!(audit.len(), 2, "the granted batch appends its own row");
            assert_eq!(audit[1]["outcome"], value!("granted"));
            assert_eq!(audit[1]["column"], value!("email"));
            assert_eq!(audit[1]["classification"], value!("pii"));

            // ---- and the fence spans ROWS, not only columns ----
            //
            // A SECOND row asking only for the permitted column, batched with the first
            // row asking for the forbidden one. Everything above is a single-row batch,
            // which cannot tell an all-or-nothing fence from a per-ROW one: a build
            // that refused row-by-row would return this row's email and pass every
            // assertion so far. `check_unmask_authorization` never sees a row, so the
            // verdict cannot differ per row - but WHAT IS RETURNED can, and that is the
            // half the atomic fence owns.
            let second = insert_through_the_pipeline(
                host,
                &pool,
                app,
                "people",
                &schema,
                value!({ "ssn": "555-55-5555", "email": "grace@example.com", "nickname": "grace" }),
            )
            .await;
            let two_rows = BulkUnmaskArgs {
                collection: "people".to_string(),
                items: vec![
                    BulkUnmaskItem {
                        row_pk: second.id.clone(),
                        columns: vec!["email".to_string()],
                    },
                    BulkUnmaskItem {
                        row_pk: person.id.clone(),
                        columns: vec!["ssn".to_string()],
                    },
                ],
                actor: Some(actor),
                reason: Some("mask_flip integration test".to_string()),
                rejected_claim: None,
            };
            let err = dispatch_bulk_unmask(
                &unmask_route(host, app).await,
                &crate::tests::fixtures::harness_binding(app),
                two_rows.clone(),
            )
            .await
            .expect_err("one forbidden pair on ONE row must refuse every row");
            assert_eq!(refusal_code(&err), "bulk_unmask_partial_unauthorized");
            let rendered = format!("{err:?}");
            assert!(
                !rendered.contains("grace@example.com"),
                "the wholly-permitted row's value must not come back either: {err:?}",
            );
            assert!(!rendered.contains(ssn), "{err:?}");
            let audit = audit_rows(&pool, app).await;
            assert_eq!(audit.len(), 3, "the refused batch audits once: {audit:?}");
            assert_eq!(audit[2]["outcome"], value!("denied"));
            assert_eq!(
                audit[2]["row_pk"],
                value!(format!("{},{}", second.id, person.id)),
                "the row names every row the batch spanned, in caller order",
            );
            let reason = audit[2]["reason"]
                .as_str()
                .expect("the audit row carries a reason");
            assert!(
                reason.contains(&format!("unauthorized=[{}/ssn]", person.id)),
                "the reason names the offending pair: {reason:?}",
            );
            assert!(
                !reason.contains(&format!("{}/email", second.id)),
                "and does not report the permitted row's pair as unauthorized: {reason:?}",
            );

            // ---- the fence COLLECTS every denied pair; it does not stop at the first
            //
            // Every arm above carries exactly ONE unauthorized pair, and a fence that
            // broke out of the loop on the first denial would satisfy all of them: one
            // audit row, one refusal, same code. The loop at
            // `crates/zeroship-data-orm/src/protection/unmask.rs` has no `break`
            // and no early return - it pushes every denied pair and refuses once -
            // and TWO observable things follow that a short-circuit would get
            // wrong. The refusal counts the pairs (`unauthorized.len()`), so
            // it must say TWO; and the whole call still audits exactly ONCE, so two
            // denials must not become two rows.
            let both_denied = BulkUnmaskArgs {
                collection: "people".to_string(),
                items: vec![
                    BulkUnmaskItem {
                        row_pk: person.id.clone(),
                        columns: vec!["ssn".to_string()],
                    },
                    BulkUnmaskItem {
                        row_pk: second.id.clone(),
                        columns: vec!["email".to_string(), "ssn".to_string()],
                    },
                ],
                actor: Some(value!({ "kind": "support", "id": "usr_support_1" })),
                reason: Some("mask_flip integration test".to_string()),
                rejected_claim: None,
            };
            let err = dispatch_bulk_unmask(
                &unmask_route(host, app).await,
                &crate::tests::fixtures::harness_binding(app),
                both_denied,
            )
            .await
            .expect_err("two forbidden pairs must still refuse the whole batch");
            assert_eq!(refusal_code(&err), "bulk_unmask_partial_unauthorized");
            assert!(
                format!("{err:?}").contains("2 (row, column) pair(s) not authorized"),
                "the refusal counts EVERY denied pair; a fence that stopped at the \
         first would report 1: {err:?}",
            );
            let audit = audit_rows(&pool, app).await;
            assert_eq!(
                audit.len(),
                4,
                "two denied pairs still audit ONCE, not once per denial: {audit:?}",
            );
            assert_eq!(audit[3]["outcome"], value!("denied"));
            let reason = audit[3]["reason"]
                .as_str()
                .expect("the audit row carries a reason");
            assert!(
                reason.contains(&format!("{}/ssn", person.id)),
                "the reason names the first denied pair: {reason:?}",
            );
            assert!(
                reason.contains(&format!("{}/ssn", second.id)),
                "and the second, which a short-circuiting fence would never reach: \
         {reason:?}",
            );
            assert!(
                !reason.contains(&format!("{}/email", second.id)),
                "and still not the permitted pair: {reason:?}",
            );

            // A new deployment grants both classifications. The same batch now passes.
            let redeployed = crate::tests::fixtures::harness_binding_at_deploy(app, "wider_policy");
            crate::tests::fixtures::cache_schema_for_deploy(&redeployed, "people", schema.clone());
            install_mask_policy(&redeployed, value!({ "support": ["pii", "pci"] }))
                .expect("install the new deployment's policy");
            let granted =
                dispatch_bulk_unmask(&unmask_route(host, app).await, &redeployed, two_rows)
                    .await
                    .expect("the same batch must pass once the policy grants both classes");
            assert_eq!(granted.results[&person.id]["ssn"], ssn);
            assert_eq!(
                granted.results[&second.id]["email"], "grace@example.com",
                "the permitted row's value was there the whole time: {:?}",
                granted.results,
            );
            let audit = audit_rows(&pool, app).await;
            assert_eq!(
                audit.len(),
                5,
                "the granted batch appends one row: {audit:?}"
            );
            assert_eq!(audit[4]["outcome"], value!("granted"));
            assert_eq!(audit[4]["column"], value!("email,ssn"));

            release_pg(host, pool).await;
        })
    })
}

/// **The query-hint fence, on Postgres.** `find({ unmask: [...] })` authorises
/// its hint BEFORE any SQL is built, and a hint naming one forbidden column is
/// refused entirely rather than quietly degraded to the columns the actor may
/// see - which would conceal the authorisation failure from the caller.
///
/// A refusal happens before read compilation, so no unmasking SELECT is issued.
///
/// This drives `authorize_query_hint` directly, as its SQLite twin does - the
/// `find` entry point needs a live V8 scope. The gap that leaves is what the
/// closing control covers: it runs the REAL post-find promotion
/// (`dispatch_unmask_for_query`) over rows from the REAL find builder and shows
/// them carrying plaintext once the policy permits it, so the refusal above is
/// withholding something this fixture demonstrably produces.
#[test]
fn a_query_hint_naming_one_forbidden_column_is_refused_whole() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg().await;
            let pool = Rc::new(Pool::connect(&url, 4).await.unwrap());
            let app = "flip_hint_denied";
            host.clear_mask_policy_cache(app);
            let schema = two_class_schema();
            let (ssn, email) = ("987-65-4321", "grace@example.com");
            let person = audited_unmask_fixture_with(
                host,
                &pool,
                &url,
                app,
                &schema,
                value!({ "ssn": ssn, "email": email, "nickname": "grace" }),
                &[("ssn", ssn), ("email", email)],
            )
            .await;

            install_mask_policy(&crate::tests::fixtures::harness_binding(app), value!({ "support": ["pii"] }))
                .expect("install the app's declared mask policy");
            let actor = Some(value!({ "kind": "support", "id": "usr_support_2" }));
            let reason = Some("mask_flip integration test".to_string());
            let both = ["email".to_string(), "ssn".to_string()];

            let err = authorize_query_hint(
                &crate::exec::ambient_route_for_tests(&crate::tests::fixtures::harness_binding(app), unmask_backend(host).await),
                &crate::tests::fixtures::harness_binding(app),
                "people",
                &both,
                &actor,
                None,
                &reason,
            )
            .await
            .expect_err(
                "a hint naming one forbidden column must be refused; a build that \
         authorises it lets the find SELECT promote a class the policy withholds",
            );
            assert_eq!(
                refusal_code(&err),
                "unmask_not_permitted",
                "the refusal must be the authorization refusal, not an incidental \
         failure further down the path: {err:?}",
            );
            let rendered = format!("{err:?}");
            assert!(
                !rendered.contains(ssn),
                "the refusal must not carry the value it refused: {err:?}",
            );
            assert!(
                !rendered.contains(email),
                "nor the value it would have permitted on its own: {err:?}",
            );

            let audit = audit_rows(&pool, app).await;
            assert_eq!(
                audit.len(),
                1,
                "the refused hint writes exactly one row for the whole query: {audit:?}",
            );
            assert_eq!(audit[0]["outcome"], value!("denied"));
            assert_eq!(audit[0]["actor_role"], value!("support"));
            assert_eq!(audit[0]["actor_id"], value!("usr_support_2"));
            assert_eq!(audit[0]["collection"], value!("people"));
            assert_eq!(audit[0]["column"], value!("email,ssn"));
            assert_eq!(audit[0]["classification"], value!("pci,pii"));
            assert_eq!(
                audit[0]["row_pk"],
                value!("[query_hint]"),
                "a hint is not a per-row dispatch, so the row_pk slot carries the \
         marker and an operator's `row_pk = '<id>'` query does not sweep it in",
            );
            let reason_text = audit[0]["reason"]
                .as_str()
                .expect("the audit row carries a reason");
            assert!(
                reason_text.contains("[query_hint] unauthorized=[ssn]"),
                "the reason names the column that caused the refusal: {reason_text:?}",
            );
            assert!(
                !reason_text.contains("email"),
                "and does not report the permitted column as unauthorized: {reason_text:?}",
            );

            // The refused hint leaves the ordinary read surface where it was: the find
            // a caller falls back to still shows the MASK. Without this the arm above
            // would pass against a build that refused the hint and leaked through the
            // default projection anyway.
            let rows = run_find(&pool, app, &value!({}), &schema).await;
            assert_eq!(rows.len(), 1, "the fixture row is still there: {rows:?}");
            assert_eq!(rows[0]["id"], value!(person.id));
            assert_eq!(rows[0]["ssn"], value!("***"));
            assert_eq!(rows[0]["email"], value!("g***@example.com"));
            assert_eq!(rows[0]["nickname"], value!("grace"));

            // ---- CONTROL 1, differing in one variable: the hint drops the forbidden
            // column. Same actor, same policy - and the fence passes. It writes no
            // audit row of its own: the granted row is deferred to
            // `audit_query_hint_granted` so a failing SELECT leaves no ghost, which is
            // why the count staying at 1 is the assertion here.
            authorize_query_hint(
                &crate::exec::ambient_route_for_tests(&crate::tests::fixtures::harness_binding(app), unmask_backend(host).await),
                &crate::tests::fixtures::harness_binding(app),
                "people",
                &["email".to_string()],
                &actor,
                None,
                &reason,
            )
            .await
            .expect("the permitted column alone must pass the fence");
            assert_eq!(
                audit_rows(&pool, app).await.len(),
                1,
                "the granted fence defers its audit row until after the SELECT lands",
            );

            // A new deployment grants both classifications, allowing the same hint.
            let redeployed = crate::tests::fixtures::harness_binding_at_deploy(app, "wider_policy");
            crate::tests::fixtures::cache_schema_for_deploy(&redeployed, "people", schema.clone());
            install_mask_policy(&redeployed, value!({ "support": ["pii", "pci"] }))
                .expect("install the new deployment's policy");
            authorize_query_hint(
                &crate::exec::ambient_route_for_tests(&crate::tests::fixtures::harness_binding(app), unmask_backend(host).await),
                &redeployed,
                "people",
                &both,
                &actor,
                None,
                &reason,
            )
            .await
            .expect("the same hint must pass once the policy grants both classes");
            let mut rows = run_find(&pool, app, &value!({}), &schema).await;
            dispatch_unmask_for_query(
                &unmask_route(host, app).await,
                &redeployed,
                "people",
                &both,
                &mut rows,
            )
            .await
            .expect("the promotion the find dispatcher runs after the SELECT");
            assert_eq!(rows[0]["id"], value!(person.id));
            assert_eq!(
                rows[0]["ssn"],
                value!(ssn),
                "the hint promotes plaintext into the listed columns: {rows:?}",
            );
            assert_eq!(rows[0]["email"], value!(email));
            audit_query_hint_granted(
                &crate::exec::ambient_route_for_tests(&crate::tests::fixtures::harness_binding(app), unmask_backend(host).await),
                &redeployed,
                "people",
                &both,
                &actor,
                None,
                &reason,
            )
            .await
            .expect("the granted audit row, written once the rows are in hand");
            let audit = audit_rows(&pool, app).await;
            assert_eq!(
                audit.len(),
                2,
                "the granted query appends exactly one row: {audit:?}",
            );
            assert_eq!(audit[1]["outcome"], value!("granted"));
            assert_eq!(audit[1]["column"], value!("email,ssn"));
            assert_eq!(audit[1]["row_pk"], value!("[query_hint]"));

            // And the promotion was in-memory only: the fields' own columns still hold
            // the mask on disk, so the next default read leaks nothing.
            let after = run_find(&pool, app, &value!({}), &schema).await;
            assert_eq!(after[0]["ssn"], value!("***"));
            assert_eq!(after[0]["email"], value!("g***@example.com"));

            release_pg(host, pool).await;
        })
    })
}

/// A hint spelled in a different case convention from the descriptor must READ
/// the column it AUTHORISED.
///
/// `resolve_schema_column` is alias-tolerant: `contactEmail` resolves to the
/// declared `contact_email`, so the authorization half of the query hint
/// succeeds. Its two siblings then adopt the resolved name - `dispatch_unmask`
/// assigns `args.column = mask_meta.canonical_column`, and
/// `dispatch_bulk_unmask` carries it through to the fetch. The query-hint path
/// did neither: it kept only the classification, so the CALLER's spelling
/// travelled on to `raw_column_name`, which is a bare prefix, and the read
/// looked for `__zs_raw__contactEmail` - a column no migration ever created.
///
/// It fails CLOSED, so this is a consistency defect rather than a leak. It is
/// still worth binding: three siblings doing two different things at one
/// boundary is how the next divergence gets introduced, and the next one may
/// not fail closed.
///
/// The two calls below are the exact sequence `crud::dispatch_find` runs - the
/// fence, then the read, over the same `unmask_columns` slice.
#[test]
fn a_query_hint_reads_the_column_its_alias_resolved_to() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg().await;
            let pool = Rc::new(Pool::connect(&url, 4).await.unwrap());
            let app = "flip_hint_alias";
            host.clear_mask_policy_cache(app);
            let schema = alias_schema();
            let email = "ada@example.com";
            let person = audited_unmask_fixture_with(
                host,
                &pool,
                &url,
                app,
                &schema,
                value!({ "contact_email": email, "nickname": "ada" }),
                &[("contact_email", email)],
            )
            .await;

            install_mask_policy(&crate::tests::fixtures::harness_binding(app), value!({ "support": ["pii"] }))
                .expect("install the app's declared mask policy");
            let actor = Some(value!({ "kind": "support", "id": "usr_support_3" }));
            let reason = Some("mask_flip integration test".to_string());
            // The caller's spelling: camelCase, where the descriptor declares snake.
            let hinted = ["contactEmail".to_string()];

            authorize_query_hint(
                &crate::exec::ambient_route_for_tests(&crate::tests::fixtures::harness_binding(app), unmask_backend(host).await),
                &crate::tests::fixtures::harness_binding(app),
                "people",
                &hinted,
                &actor,
                None,
                &reason,
            )
            .await
            .expect(
                "the alias must AUTHORISE - `resolve_schema_column` accepts the camel \
         spelling, so a failure here means the fixture is wrong rather than the \
         defect being present",
            );

            let mut rows = vec![value!({
                "id": person.id.clone(),
                "contact_email": "a***@example.com",
                "nickname": "ada",
            })];
            dispatch_unmask_for_query(
                &unmask_route(host, app).await,
                &crate::tests::fixtures::harness_binding(app),
                "people",
                &hinted,
                &mut rows,
            )
            .await
            .expect(
                "the read must find the column the fence authorised. If this errors \
             on a missing column, the hint authorised `contact_email` and then \
             read `__zs_raw__contactEmail`: the caller's spelling survived \
             because the query-hint path discarded the canonical name its two \
             siblings adopt",
            );
            assert_eq!(
                rows[0]["contact_email"],
                value!(email),
                "the promoted value lands under the DECLARED name: {rows:?}",
            );

            release_pg(host, pool).await;
        })
    })
}

// ---------------------------------------------------------------------------
// 2. Outward: the leak the flip would have created
// ---------------------------------------------------------------------------

/// **Outward.** No row-returning write verb may hand back the raw column.
///
/// A write site in `zeroship-data-orm::sql` that emits `RETURNING *` returns every
/// physical column, never passing through the projection allowlist, which is
/// SELECT-side only. Without the read pipeline's row-surface stage, `insert`
/// returns the real value under a key the generated `Row<S>` type does not
/// declare - invisible to any review written against the generated types, and
/// doubly silent because the mask still comes back correctly beside it.
///
/// **Both boundaries are asserted here, in the order a row crosses them.** The
/// projection is first: the write's own SQL must not name the raw column, so
/// the value never leaves the database. The surface stage is second, and still
/// necessary - the write is not the only producer of a row, and the second half
/// of this test feeds it the kind of row that has no projection in front of it.
///
/// The assertion is on the KEY SET, not on the absence of one name, so a
/// differently-named raw column cannot pass it.
#[test]
fn no_write_verb_hands_back_a_column_the_descriptor_does_not_declare() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg().await;
            let pool = Rc::new(Pool::connect(&url, 4).await.unwrap());
            let app = "flip_returning";
            let alias = crate::tests::fixtures::harness_alias(app);
            let schema = flip_schema();
            fixture(host, &pool, &url, app, "people", &schema).await;

            let Inserted {
                id: minted_id,
                rows: returned,
            } = insert_through_the_pipeline(
                host,
                &pool,
                app,
                "people",
                &schema,
                value!({ "ssn": "123-45-6789", "nickname": "ada" }),
            )
            .await;
            assert_eq!(returned.len(), 1);
            // BOUNDARY 1, the database's. The row PostgreSQL sent back does not contain
            // the raw column at all - not "contains it and we removed it".
            assert!(
                returned[0].get(raw_column_name("ssn").as_str()).is_none(),
                "the write's projection must not name the raw column: {:?}",
                returned[0],
            );
            // Paired with the control that the row is a real row and not an empty one -
            // otherwise "no raw column" is satisfied by returning nothing. The id is
            // the one the pipeline minted BEFORE the statement ran, so this arm is also
            // the projection's round trip: `RETURNING "id"` hands back the identity the
            // write assigned.
            assert_eq!(returned[0]["id"], value!(minted_id));
            assert!(
                returned[0].get("ssn").is_some(),
                "the masked column must still come back: {:?}",
                returned[0],
            );
            // And the raw column IS in the table - so the assertion above is about the
            // projection, not about a write that failed to store the value.
            let stored = pool
                .query_text_params(
                    &format!(
                        r#"SELECT {} AS raw FROM "{alias}"."people" WHERE "id" = $1"#,
                        crate::sql::mapping::quote_ident(&raw_column_name("ssn")),
                    ),
                    &[minted_id.as_str()],
                )
                .await
                .expect("read the raw column directly");
            assert_eq!(stored.len(), 1, "the row must exist");

            // BOUNDARY 2, the runtime's.
            let allowed: BTreeSet<String> =
                read_surface_columns(&crate::tests::fixtures::generated_schema(schema.clone()));
            let finalized = host
                .finalize_rows_on_read(app, "people", returned.clone())
                .await
                .expect("read pipeline");
            let keys: BTreeSet<String> =
                finalized[0].as_object().unwrap().keys().cloned().collect();
            assert!(
                keys.is_subset(&allowed),
                "a write's returned row carried columns the descriptor does not declare: {:?}",
                keys.difference(&allowed).collect::<Vec<_>>(),
            );
            // And the VALUE is gone, not merely re-keyed.
            let serialized = serde_json::to_string(&finalized[0]).unwrap();
            assert!(
                !serialized.contains("123-45-6789"),
                "the real value must not cross the JS boundary from a write: {serialized}",
            );
            // Paired with what must still come back, so this is not a green from
            // returning an empty row.
            assert_eq!(finalized[0]["ssn"]["masked"], value!("***"));
            assert_eq!(finalized[0]["nickname"], value!("ada"));
            assert_eq!(finalized[0]["id"], value!(minted_id));

            // ---- and the arm that binds the SURFACE stage specifically ----
            //
            // The arm above does not exercise the surface stage AT ALL: the row it
            // finalizes came out of a named projection, so there is nothing off-surface
            // in it to remove. That is the point of the projection, and it is also why
            // this arm has to build its own row.
            //
            // A row with off-surface columns still reaches the pipeline from the
            // producer no projection sits in front of: the WAL consumer decodes
            // pgoutput with no schema in reach. This is that row's shape - the raw
            // column carrying the real value, plus columns the descriptor does not
            // declare at all (an auxiliary shadow-table key, a column added to the
            // table out of band). Nothing else on the read path removes an unknown key:
            // the only other key removal in the pipeline is the mask pass's, and it
            // removes exactly one name it derives itself.
            let mut smuggled = returned[0].clone();
            smuggled[raw_column_name("ssn")] = value!("123-45-6789");
            smuggled["__zs_shadow_key"] = value!("aux-42");
            smuggled["totally_undeclared"] = value!("leak-me");
            let finalized = host
                .finalize_rows_on_read(app, "people", vec![smuggled])
                .await
                .expect("read pipeline");
            let keys: BTreeSet<String> =
                finalized[0].as_object().unwrap().keys().cloned().collect();
            assert!(
                keys.is_subset(&allowed),
                "an undeclared physical column reached the JS boundary: {:?}",
                keys.difference(&allowed).collect::<Vec<_>>(),
            );
            assert!(
                !serde_json::to_string(&finalized[0])
                    .unwrap()
                    .contains("leak-me"),
                "and neither did its value: {finalized:?}",
            );

            release_pg(host, pool).await;
        })
    })
}

// ---------------------------------------------------------------------------
// 3. Inward: the raw column is unnameable
// ---------------------------------------------------------------------------

/// **Inward.** The raw column is refused on every inbound surface.
///
/// Not because a fence was added to each of them - because the column is named
/// something `validate_field_name` already refused before the flip, and every
/// inbound surface already calls it. That is a stronger property than a fence:
/// a fence can be forgotten on a surface nobody has written yet.
///
/// The name is derived from `raw_column_name`, never spelled as a literal, so a
/// rename keeps the test pointed at the real column.
#[test]
fn the_raw_column_is_refused_on_every_inbound_surface() {
    Host::test(|_| {
        let raw = raw_column_name("ssn");
        let schema = crate::tests::fixtures::native_fields(flip_schema());

        let refusals: Vec<(&str, bool)> = vec![
            (
                "filter key",
                compile_filter(&value!({ (raw.clone()): "x" }), &mut Vec::new(), &schema).is_err(),
            ),
            (
                // The aggregate matcher and `$group.by` below both validate
                // against the same declared shape.
                "aggregate $match",
                compile_aggregate(
                    &crate::sql::SchemaName::new("app1").expect("fixture schema name"),
                    "people",
                    &value!([{ "$match": { (raw.clone()): "x" } }]),
                    &schema,
                )
                .is_err(),
            ),
            (
                "select",
                compile_find(
                    &crate::sql::SchemaName::new("app1").expect("fixture schema name"),
                    "people",
                    &value!({}),
                    Some(1),
                    None,
                    None,
                    Some(&value!([raw.clone()])),
                    &schema,
                )
                .is_err(),
            ),
            (
                "orderBy",
                compile_find(
                    &crate::sql::SchemaName::new("app1").expect("fixture schema name"),
                    "people",
                    &value!({}),
                    Some(1),
                    None,
                    Some(&value!({ (raw.clone()): 1 })),
                    None,
                    &schema,
                )
                .is_err(),
            ),
            (
                "$group.by",
                compile_aggregate(
                    &crate::sql::SchemaName::new("app1").expect("fixture schema name"),
                    "people",
                    &value!([{ "$group": { "by": [raw.clone()] } }]),
                    &schema,
                )
                .is_err(),
            ),
            (
                "distinct",
                compile_distinct(
                    &crate::sql::SchemaName::new("app1").expect("fixture schema name"),
                    "people",
                    &raw,
                    &value!({}),
                    &schema,
                )
                .is_err(),
            ),
            (
                // The exact function the write pipeline's document-key and
                // update-patch-key fences call, on every key including `$set`
                // nesting.
                "write document key",
                validate_field_name(&raw).is_err(),
            ),
        ];
        for (surface, refused) in &refusals {
            assert!(refused, "{surface} accepted the raw column {raw:?}");
        }
        assert_eq!(refusals.len(), 7, "seven inbound surfaces ruled on");

        // The control: the LOGICAL name is ACCEPTED on those same surfaces. Without
        // it, a validator that refused everything would pass all seven above.
        assert!(compile_filter(&value!({ "ssn": "x" }), &mut Vec::new(), &schema).is_ok());
        assert!(compile_distinct(
            &crate::sql::SchemaName::new("app1").expect("fixture schema name"),
            "people",
            "ssn",
            &value!({}),
            &schema
        )
        .is_ok());
        assert!(validate_field_name("ssn").is_ok());
        let runtime_schema = crate::tests::fixtures::generated_schema(flip_schema());
        assert!(compile_find(
            &crate::sql::SchemaName::new("app1").expect("fixture schema name"),
            "people",
            &value!({}),
            Some(1),
            None,
            Some(&value!({ "ssn": 1 })),
            Some(&value!(["ssn"])),
            &runtime_schema,
        )
        .is_ok());
    })
}

// ---------------------------------------------------------------------------
// 4. Live-query subscriptions
// ---------------------------------------------------------------------------

/// **Live-query subscriptions.** A masked-column predicate must keep firing.
///
/// The WAL tuple carries the mask under the field's own name, so an unlowered
/// `find({ssn: "123-45-6789"})` would compare a plaintext operand against a
/// stored mask, never match, and the subscription would stop firing with no
/// error anywhere - the failure mode `read_set`'s own doc calls unacceptable.
///
/// Equality is lowered (the operand is masked the same way the stored value
/// is); a range is dropped to coarse-grained, because a range over a mask is not
/// a range over the value and no rewriting makes it one. Coarse-grained
/// over-delivers, which is the bias `read_set` already declares.
#[test]
fn a_masked_predicate_is_lowered_for_the_change_stream() {
    Host::test(|_| {
        use zeroship_data_orm::cdc::read_set::{normalise_filter, Predicate, PredicateOp};
        let schema = crate::tests::fixtures::native_fields(flip_schema());

        let Some(Predicate::All(conjuncts)) =
            normalise_filter(&value!({ "ssn": "123-45-6789" }), &schema)
        else {
            panic!("an equality predicate on a masked column must stay fine-grained");
        };
        assert_eq!(conjuncts.len(), 1);
        assert_eq!(conjuncts[0].column, "ssn");
        assert_eq!(conjuncts[0].op, PredicateOp::Eq);
        assert_eq!(
            conjuncts[0].value,
            value!("***"),
            "the operand must be masked the same way the stored value is, or the \
         predicate silently never matches",
        );

        assert!(
            normalise_filter(&value!({ "ssn": { "$gt": "500-00-0000" } }), &schema).is_none(),
            "a range over a mask must fall back to coarse-grained rather than \
         comparing masks as if they were values",
        );

        // The control: the unmasked column keeps BOTH shapes fine-grained and its
        // operand untouched. Without this arm an implementation that returned
        // `None` for everything would pass the range assertion.
        let Some(Predicate::All(conjuncts)) =
            normalise_filter(&value!({ "nickname": "ada" }), &schema)
        else {
            panic!("an unmasked equality predicate must stay fine-grained");
        };
        assert_eq!(conjuncts[0].value, value!("ada"));
        assert!(normalise_filter(&value!({ "nickname": { "$gt": "m" } }), &schema).is_some());
    })
}

// ---------------------------------------------------------------------------
// 5. The DDL half - the piece that touches real column data
// ---------------------------------------------------------------------------

/// **The type-and-constraint swap, verified against a real server.**
///
/// The differ emits nothing for the flip: the column-additions branch is
/// name-only and the `RewriteColumnType` arm keys strictly off the `encrypted`
/// toggle, and the flip moves neither the name nor that toggle. So the emitted
/// CREATE TABLE is hand-authored with nothing verifying it. This test is that
/// verification - it applies the emitted DDL to a real server, reads the
/// server's own catalog back, and then writes through the real pipeline.
///
/// The specific thing it catches: `.mask()` is legal on string, number and
/// bytes, and every mask kind returns a String. Leaving the declared type and
/// constraints on the field's own column makes `'***'` a hard error under
/// `DOUBLE PRECISION` and a CHECK violation under an enum - every write to the
/// collection fails.
#[test]
fn the_declared_type_and_constraints_travel_to_the_raw_column() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg().await;
            let pool = Rc::new(Pool::connect(&url, 4).await.unwrap());
            let app = "flip_ddl";
            let alias = crate::tests::fixtures::harness_alias(app);
            let schema = value!({
                "score": {
                    "type": "number",
                    "required": true,
                    "mask": { "kind": "full", "classification": "pii" }
                },
                "tier": {
                    "type": "string",
                    "enum": ["gold", "silver"],
                    "required": true,
                    "mask": { "kind": "full", "classification": "pii" }
                },
                "plain": { "type": "number" },
            });
            fixture(host, &pool, &url, app, "accounts", &schema).await;

            // The server's own catalog, not the emitted string.
            let cols = pool
                .query_text_params(
                    "SELECT column_name, data_type FROM information_schema.columns \
             WHERE table_schema = $1 AND table_name = 'accounts' ORDER BY column_name",
                    &[alias.as_str()],
                )
                .await
                .unwrap();
            let types: BTreeMap<String, String> = cols
                .iter()
                .map(|r| {
                    (
                        r.get::<_, String>("column_name"),
                        r.get::<_, String>("data_type"),
                    )
                })
                .collect();
            assert_eq!(
                types.get("score").map(String::as_str),
                Some("text"),
                "the masked column must be TEXT so it can hold '***': {types:?}",
            );
            assert_eq!(
                types.get(&raw_column_name("score")).map(String::as_str),
                Some("double precision"),
                "the declared numeric type belongs to the value: {types:?}",
            );
            // The control: an unmasked numeric column keeps its type under its own
            // name, so this is not a green from making everything text.
            assert_eq!(
                types.get("plain").map(String::as_str),
                Some("double precision"),
                "an unmasked column is untouched by the flip: {types:?}",
            );

            // And it accepts a write. This is what a mistake in the swap breaks.
            let account = insert_through_the_pipeline(
                host,
                &pool,
                app,
                "accounts",
                &schema,
                value!({ "score": 42.5, "tier": "gold", "plain": 7.0 }),
            )
            .await;

            let stored = pool
                .query_text_params(
                    &format!(
                        // `::text` on the raw score because it is a real `float8` on
                        // the server - which is the point of the test.
                        "SELECT \"score\" AS mask, \"{}\"::text AS raw, \"tier\" AS tier_mask, \
                 \"{}\" AS tier_raw FROM \"{alias}\".\"accounts\" WHERE id = $1",
                        raw_column_name("score"),
                        raw_column_name("tier"),
                    ),
                    &[account.id.as_str()],
                )
                .await
                .unwrap();
            assert_eq!(
                stored.len(),
                1,
                "the minted id must address the row the write created",
            );
            assert_eq!(stored[0].get::<_, String>("mask"), "***");
            assert_eq!(stored[0].get::<_, String>("raw"), "42.5");
            assert_eq!(stored[0].get::<_, String>("tier_mask"), "***");
            assert_eq!(stored[0].get::<_, String>("tier_raw"), "gold");

            release_pg(host, pool).await;
        })
    })
}

/// **Constraints follow the real value, and that is not an optimisation.**
///
/// `.unique()` on a masked field is semantically about the real value. Left on
/// the masked column it would enforce uniqueness over MASKS, where many rows
/// legitimately share `***-**-1234` - and for `kind: "full"` every mask is
/// `***`, so the table would cap at ONE ROW and the failure would present as a
/// duplicate-key error on perfectly valid data.
#[test]
fn a_unique_masked_field_admits_rows_that_share_a_mask() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg().await;
            let pool = Rc::new(Pool::connect(&url, 4).await.unwrap());
            let app = "flip_unique";
            let alias = crate::tests::fixtures::harness_alias(app);
            let schema = value!({
                "ssn": {
                    "type": "string",
                    "unique": true,
                    "mask": { "kind": "last4", "classification": "pci" }
                },
            });
            fixture(host, &pool, &url, app, "people", &schema).await;

            // `build_create_indexes` emits CONCURRENTLY, which cannot run inside the
            // implicit transaction `batch_execute` uses, so the fixture's DDL carries
            // the table alone. Apply the index the platform would build.
            for spec in schema::fixture_indexes(
                crate::tests::fixtures::harness_binding(app).schema(),
                "people",
                &schema,
            )
            .unwrap()
            {
                pool.execute(&spec.sql.replace("CONCURRENTLY ", ""), &[])
                    .await
                    .unwrap_or_else(|e| panic!("index must build: {e}\n{}", spec.sql));
            }

            // Two rows whose real values differ but whose masks are identical. Neither
            // names its own id - the platform mints one per row, which is what makes
            // them two rows rather than one overwritten one.
            let first = insert_through_the_pipeline(
                host,
                &pool,
                app,
                "people",
                &schema,
                value!({ "ssn": "111-11-1234" }),
            )
            .await;
            let second = insert_through_the_pipeline(
                host,
                &pool,
                app,
                "people",
                &schema,
                value!({ "ssn": "999-99-1234" }),
            )
            .await;

            let count = pool
                .query_text_params(
                    &format!("SELECT count(*)::text AS n FROM \"{alias}\".\"people\""),
                    &[],
                )
                .await
                .unwrap();
            assert_eq!(
                count[0].get::<_, String>("n"),
                "2",
                "two rows sharing the mask ***-**-1234 must both insert",
            );
            // ...and they are the two the writes created, addressed by the ids the
            // platform minted for them.
            let named = pool
                .query_text_params(
                    &format!(
                        "SELECT count(*)::text AS n FROM \"{alias}\".\"people\" \
                 WHERE \"id\" IN ($1, $2)"
                    ),
                    &[first.id.as_str(), second.id.as_str()],
                )
                .await
                .unwrap();
            assert_eq!(
                named[0].get::<_, String>("n"),
                "2",
                "both minted ids must address a stored row",
            );

            // The control: uniqueness over the REAL value is still enforced, so this is
            // not a green from dropping the constraint.
            // A third row whose REAL value collides with the first. Its id is minted
            // like every other, so the only thing that can be refused below is the
            // duplicate value on the raw column.
            let mut docs = value!([{ "ssn": "111-11-1234" }]);
            host.prepare_insert_many_docs(&mut docs, app, "people", None)
                .await
                .expect("write pipeline");
            let runtime_schema = crate::tests::fixtures::generated_schema(schema.clone());
            let bq = compile_insert(
                crate::tests::fixtures::harness_binding(app).schema(),
                "people",
                &runtime_schema,
                &docs[0],
            )
            .unwrap();
            let param_refs = &bq.params;
            let err = zeroship_data_orm::backend::postgres::params::query(
                &pool.acquire().await.unwrap(),
                &bq.sql,
                param_refs,
            )
            .await
            .expect_err("a duplicate REAL value must still be refused");
            assert!(
                format!("{err:?}").contains("23505") || format!("{err:?}").contains("unique"),
                "expected a unique violation on the raw column, got {err:?}",
            );

            release_pg(host, pool).await;
        })
    })
}

// ---------------------------------------------------------------------------
// 8. Protection removed from the descriptor alone
// ---------------------------------------------------------------------------

/// [`flip_schema`] with the `mask` key DELETED from `ssn` and nothing else
/// changed. The control column stays, so a fixture that stopped storing
/// anything at all cannot pass.
fn flip_schema_without_the_mask_key() -> Value {
    value!({
        "ssn": { "type": "string" },
        "nickname": { "type": "string" },
    })
}

fn encrypted_schema() -> Value {
    value!({
        "secret": {
            "type": "string",
            "encrypted": true
        },
        "nickname": { "type": "string" },
    })
}

/// [`encrypted_schema`] with the `encrypted` key DELETED from `secret`.
fn encrypted_schema_without_the_encrypted_key() -> Value {
    value!({
        "secret": { "type": "string" },
        "nickname": { "type": "string" },
    })
}

/// Every physical row named by `ids`, keyed by id, with `columns` projected.
async fn physical_rows(
    pool: &Rc<Pool>,
    app: &str,
    columns: &str,
    ids: [&str; 2],
) -> BTreeMap<String, Value> {
    let alias = crate::tests::fixtures::harness_alias(app);
    let rows = pool
        .query_text_params(
            &format!(
                "SELECT \"id\", {columns} FROM \"{alias}\".\"people\" \
                 WHERE \"id\" IN ($1, $2) ORDER BY \"id\""
            ),
            &ids,
        )
        .await
        .unwrap();
    rows.iter()
        .map(row_to_value)
        .map(|r| (r["id"].as_str().unwrap_or_default().to_string(), r))
        .collect()
}

/// **The descriptor is not the protection authority.**
///
/// A creator deletes the `mask` key from one field and redeploys. No migration
/// runs, so the physical table is untouched: `__zs_raw__ssn` is still there and
/// the column still carries its `zero-migrate:mask:` sentinel. The database therefore
/// still declares the column masked while the descriptor no longer does.
///
/// A write under that descriptor must not store the plaintext under the field's
/// own name. Doing so is a silent downgrade of a protection: no refusal, no
/// signal, and the next read hands app JS a bare string where every earlier row
/// yields a `MaskedValue`.
#[test]
fn deleting_the_mask_key_from_the_descriptor_must_not_write_plaintext() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg().await;
            let pool = Rc::new(Pool::connect(&url, 4).await.unwrap());
            let app = "flip_mask_key_deleted";
            let alias = crate::tests::fixtures::harness_alias(app);
            let masked = flip_schema();
            fixture(host, &pool, &url, app, "people", &masked).await;

            // Deploy 1: the descriptor declares the mask, and the table was built for
            // exactly that.
            let first = insert_through_the_pipeline(
                host,
                &pool,
                app,
                "people",
                &masked,
                value!({ "ssn": "123-45-6789", "nickname": "alice" }),
            )
            .await;

            // Control, differing in one variable: the FIRST write, under the
            // mask-declaring descriptor, put the mask in the field's own column and the
            // real value in the raw sibling. Without this the test would pass against an
            // implementation that refused every write, masked or not.
            let raw = raw_column_name("ssn");
            let before = physical_rows(
                &pool,
                app,
                &format!("\"ssn\", \"{raw}\""),
                [first.id.as_str(), first.id.as_str()],
            )
            .await;
            assert_eq!(
                before[&first.id]["ssn"].as_str(),
                Some("***"),
                "control: the mask-declaring deploy stores the mask under the field's \
         own name: {before:?}",
            );
            assert_eq!(
                before[&first.id][&raw].as_str(),
                Some("123-45-6789"),
                "control: and the real value in the raw sibling: {before:?}",
            );

            // Deploy 2: same table, same physical shape, one JSON key gone.
            let unmasked = flip_schema_without_the_mask_key();
            // The floor cache is deliberately NOT reset. Both writes run under one
            // binding, so the second reuses the floor the first resolved - which is
            // right, because the CATALOG did not change, only the descriptor did. A test
            // that reset it here would prove the fence works on a cold cache and say
            // nothing about the warm one production actually runs.
            crate::tests::fixtures::cache_schema(app, "people", unmasked.clone());
            let mut docs = value!([{ "ssn": "987-65-4321", "nickname": "bob" }]);
            let err = host
                .prepare_insert_many_docs(&mut docs, app, "people", None)
                .await
                .expect_err(
                    "a descriptor that dropped the mask must not be able to write the \
             plaintext this table still protects",
                );
            assert!(
                format!("{err:?}").contains("protection_removed_from_descriptor"),
                "the refusal must carry the typed code a creator branches on, got {err:?}",
            );
            assert!(
                format!("{err:?}").contains("ssn"),
                "and must name the column whose protection went missing, got {err:?}",
            );

            // The refusal is a refusal: nothing landed, and the row that was already
            // there is untouched.
            let count = pool
                .query_text_params(
                    &format!("SELECT count(*)::text AS n FROM \"{alias}\".\"people\""),
                    &[],
                )
                .await
                .unwrap();
            assert_eq!(
                count[0].get::<_, String>("n"),
                "1",
                "the refused write must not have stored a row",
            );

            // And the plaintext is nowhere in the field's own column.
            let leaked = pool
                .query_text_params(
                    &format!(
                        "SELECT count(*)::text AS n FROM \"{alias}\".\"people\" WHERE \"ssn\" = $1"
                    ),
                    &["987-65-4321"],
                )
                .await
                .unwrap();
            assert_eq!(leaked[0].get::<_, String>("n"), "0");

            release_pg(host, pool).await;
        })
    })
}

/// The same shape for ENCRYPTION, asserted separately.
///
/// The two passes read different descriptor keys (`encrypted` vs `mask`) and
/// place their output in different physical columns, so one answer says nothing
/// about the other.
#[test]
fn deleting_the_encrypted_key_from_the_descriptor_must_not_write_plaintext() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg().await;
            let pool = Rc::new(Pool::connect(&url, 4).await.unwrap());
            let app = "flip_enc_key_deleted";
            let alias = crate::tests::fixtures::harness_alias(app);
            let _keys = host.supply_project_key(&[app], &"01".repeat(32));
            let encrypted = encrypted_schema();
            fixture(host, &pool, &url, app, "people", &encrypted).await;

            let first = insert_through_the_pipeline(
                host,
                &pool,
                app,
                "people",
                &encrypted,
                value!({ "secret": "hunter2-the-real-one", "nickname": "alice" }),
            )
            .await;

            // Control: the encrypting deploy stored ciphertext, so this test is not
            // green because writes stopped working.
            let before = physical_rows(
                &pool,
                app,
                "encode(\"secret\", 'escape') AS secret_bytes",
                [first.id.as_str(), first.id.as_str()],
            )
            .await;
            assert_ne!(
                before[&first.id]["secret_bytes"].as_str(),
                Some("hunter2-the-real-one"),
                "control: the encrypting deploy must not store plaintext: {before:?}",
            );

            let plain = encrypted_schema_without_the_encrypted_key();
            crate::tests::fixtures::cache_schema(app, "people", plain.clone());
            let mut docs = value!([{ "secret": "hunter3-also-real", "nickname": "bob" }]);
            let err = host
                .prepare_insert_many_docs(&mut docs, app, "people", None)
                .await
                .expect_err(
                    "a descriptor that dropped the encryption block must not be able to \
             write the plaintext this column still protects",
                );
            assert!(
                format!("{err:?}").contains("protection_removed_from_descriptor"),
                "the refusal must carry the typed code a creator branches on, got {err:?}",
            );
            assert!(
                format!("{err:?}").contains("secret"),
                "and must name the column whose protection went missing, got {err:?}",
            );

            let leaked = pool
                .query_text_params(
                    &format!(
                        "SELECT count(*)::text AS n FROM \"{alias}\".\"people\" \
                 WHERE encode(\"secret\", 'escape') = $1"
                    ),
                    &["hunter3-also-real"],
                )
                .await
                .unwrap();
            assert_eq!(
                leaked[0].get::<_, String>("n"),
                "0",
                "the refused write must not have stored the plaintext",
            );

            release_pg(host, pool).await;
        })
    })
}

// ---------------------------------------------------------------------------
// 6. The protection floor against a table PRODUCTION built
// ---------------------------------------------------------------------------

/// The confined ceiling `zeroship-migrate-server` ships, composed for one app.
///
/// Not a fixture charter: `ManagedPolicyConfig::default_confined` loads
/// `CONFINED_CEILING_TOML`, which is
/// `crates/zeroship-migrate-server/policies/confined.policy.toml` concatenated
/// with `policies/confined-system-shape.inject.toml` at compile time. A charter
/// written here would inject whatever columns its author had in mind; this one
/// injects the seven every creator table on the platform carries, because it is
/// the same bytes the deployed server uses.
fn confined_ceiling_for(app_id: &zeroship_core::AppId) -> zeroship_migrate_policy::EffectivePolicy {
    // 32 bytes is the seal-key floor `ManagedPolicyConfig::new` enforces. Nothing
    // below seals anything - the key is a construction precondition, not an input
    // to the composition this reads.
    zeroship_migrate_server::policy::ManagedPolicyConfig::default_confined([7u8; 32], 1)
        .expect("the shipped confined ceiling must load")
        .current_ceiling_for_app(app_id, None)
        .expect("the shipped confined ceiling must compose for an app")
        .policy
}

/// Create `<app>.<collection>` the way PRODUCTION creates a creator table.
///
/// [`fixture`] renders its DDL with the DATA PLANE's emitter,
/// `crate::sql::mapping::build_create_table_with_fks`, whose only callers are
/// tests. Every creator table that exists on the platform is instead rendered by
/// the MIGRATION ENGINE and applied by `zeroship-migrate-server`. A protection
/// test that builds its table with the data-plane emitter therefore agrees with
/// the reader by construction and cannot see a disagreement between the two -
/// which is how the protection floor shipped with an input that was empty on
/// every real table.
///
/// So this one renders through `zeroship_migrate::schema::query` under
/// `zeroship_migrate_postgres::DIALECT`, the exact pair `apply.rs` drives, with
/// the shipped confined ceiling supplying the injected system columns.
async fn fixture_via_the_migration_engine(
    host: &Host,
    pool: &Rc<Pool>,
    url: &str,
    app_id: &zeroship_core::AppId,
    collection: &str,
    schema: &Value,
) -> String {
    let app = app_id.as_str().to_owned();
    let alias = crate::tests::fixtures::harness_alias(&app);
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{alias}\" CASCADE"), &[])
        .await
        .unwrap();
    pool.execute(&format!("CREATE SCHEMA \"{alias}\""), &[])
        .await
        .unwrap();

    let statements =
        zeroship_migrate::schema::query::build_create_table_with_fks_for_dialect_scoped_statements(
            zeroship_migrate::shipping_vendors(),
            &alias,
            collection,
            &serde_json::to_value(schema).unwrap(),
            &zeroship_migrate::schema::query::FkEmission::Inline,
            &zeroship_migrate_postgres::DIALECT,
            false,
            &confined_ceiling_for(app_id),
        )
        .expect("the migration engine's own CREATE TABLE emitter");
    // The sentinel COMMENTs are the tail of this list, so an emitter that stopped
    // producing them would leave the count short. Assert the payload is not a
    // bare CREATE before executing it.
    assert!(
        statements.len() > 1,
        "the engine emitted only {} statement(s); the sentinel COMMENTs are part \
         of this payload and a fixture without them measures nothing: {statements:?}",
        statements.len(),
    );
    for statement in &statements {
        pool.batch_execute(statement)
            .await
            .unwrap_or_else(|e| panic!("engine-emitted DDL must apply: {e}\n{statement}"));
    }
    host.install_postgres_pool(Rc::clone(pool), url);
    crate::tests::fixtures::cache_schema(&app, collection, schema.clone());
    app
}

/// The `pg_description` comment on `<binding schema>.<collection>.<column>`, or
/// `None`.
///
/// The same catalog row `read_live_schema`'s `LEFT JOIN pg_description` reads, so
/// what this returns is what the protection floor's introspector sees.
async fn column_comment(
    pool: &Rc<Pool>,
    app: &str,
    collection: &str,
    column: &str,
) -> Option<String> {
    let alias = crate::tests::fixtures::harness_alias(app);
    let rows = pool
        .query_text_params(
            "SELECT pgd.description AS comment
               FROM pg_attribute a
               JOIN pg_class c ON c.oid = a.attrelid
               JOIN pg_namespace n ON n.oid = c.relnamespace
               LEFT JOIN pg_description pgd
                      ON pgd.objoid = c.oid AND pgd.objsubid = a.attnum
              WHERE n.nspname = $1 AND c.relname = $2 AND a.attname = $3",
            &[alias.as_str(), collection, column],
        )
        .await
        .unwrap();
    rows.first().and_then(|row| row.try_get("comment").ok())
}

/// **The floor's input must be the sentinel PRODUCTION writes.**
///
/// [`deleting_the_mask_key_from_the_descriptor_must_not_write_plaintext`] proves
/// the fence refuses a downgrade on a table the DATA PLANE's emitter built. That
/// emitter has no production caller. This asks the same question of a table the
/// MIGRATION ENGINE built, which is the only kind that exists on the platform.
///
/// It failed before the sentinel spellings were converged: the engine wrote
/// `zero-migrate:mask:...`, the runtime introspector recognised only
/// `__zsmask:...`, so `floor_from_live` saw no protected column, the fence had
/// nothing to compare, and the write that deleted the `mask` key stored the
/// plaintext under the field's own name - the exact downgrade the fence exists
/// to refuse, on the exact tables it was built for.
#[test]
fn a_migration_engine_built_table_refuses_a_mask_downgrade() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg().await;
            let pool = Rc::new(Pool::connect(&url, 4).await.unwrap());
            let app_id = zeroship_core::AppId::mint();
            let masked = flip_schema();
            let app =
                fixture_via_the_migration_engine(host, &pool, &url, &app_id, "people", &masked)
                    .await;
            let alias = crate::tests::fixtures::harness_alias(&app);

            // CONTROL 1, and the one that binds the two crates' codecs together: the
            // comment the ENGINE wrote must be byte-identical to what the RUNTIME's codec
            // builds for the same declaration. Everything below is downstream of that
            // equality; without it the fence reads a spelling nobody writes.
            let stored = column_comment(&pool, &app, "people", "ssn")
                .await
                .expect("the engine must attach a mask sentinel to the masked column");
            assert_eq!(
                stored,
                crate::sql::mask_codec::build_mask_sentinel(
                    crate::sql::catalog::MaskKind::Full,
                    crate::sql::catalog::Classification::Pci,
                ),
                "the migration engine writes the protection record and the data plane \
         reads it; a spelling only one of them knows is a fence with no input",
            );

            // CONTROL 2: under the mask-declaring descriptor the write goes through and
            // the mask lands in the field's own column. Without it a fence that refused
            // every write would satisfy the assertion below.
            let first = insert_through_the_pipeline(
                host,
                &pool,
                &app,
                "people",
                &masked,
                value!({ "ssn": "123-45-6789", "nickname": "alice" }),
            )
            .await;
            let raw = raw_column_name("ssn");
            let before = physical_rows(
                &pool,
                &app,
                &format!("\"ssn\", \"{raw}\""),
                [first.id.as_str(), first.id.as_str()],
            )
            .await;
            assert_eq!(
                before[&first.id]["ssn"].as_str(),
                Some("***"),
                "control: the mask-declaring deploy stores the mask under the field's \
         own name: {before:?}",
            );
            assert_eq!(
                before[&first.id][&raw].as_str(),
                Some("123-45-6789"),
                "control: and the real value in the raw sibling: {before:?}",
            );

            // The one-key deletion, against the table the engine built.
            crate::tests::fixtures::cache_schema(
                &app,
                "people",
                flip_schema_without_the_mask_key(),
            );
            let mut docs = value!([{ "ssn": "987-65-4321", "nickname": "bob" }]);
            // Not `expect_err`: the failure this test exists for is the pipeline PREPARING
            // the write, and the prepared document is the downgrade itself. Reporting it
            // is the difference between "returned Ok(())" and naming the plaintext that
            // was about to be stored under the field's own name.
            let err = match host
                .prepare_insert_many_docs(&mut docs, &app, "people", None)
                .await
            {
                Ok(()) => panic!(
                    "a descriptor that dropped the mask must not be able to write the \
             plaintext this table still protects, but the pipeline prepared \
             it: {docs}",
                ),
                Err(err) => err,
            };
            assert!(
                format!("{err:?}").contains("protection_removed_from_descriptor"),
                "the refusal must carry the typed code a creator branches on, got {err:?}",
            );
            assert!(
                format!("{err:?}").contains("ssn"),
                "and must name the column whose protection went missing, got {err:?}",
            );

            // The refusal is a refusal: the plaintext is nowhere in the field's own
            // column. Asserted separately from the error because the failure this test
            // exists for is a SILENT one - the write succeeding is the defect, and the
            // row it leaves behind is the evidence.
            let leaked = pool
                .query_text_params(
                    &format!(
                        "SELECT count(*)::text AS n FROM \"{alias}\".\"people\" WHERE \"ssn\" = $1"
                    ),
                    &["987-65-4321"],
                )
                .await
                .unwrap();
            assert_eq!(leaked[0].get::<_, String>("n"), "0");

            release_pg(host, pool).await;
        })
    })
}

/// The same shape for ENCRYPTION, on a migration-engine-built table.
///
/// Asserted separately for the reason its data-plane peer is: the two passes read
/// different descriptor keys and their sentinels are different strings on
/// different columns, so one answer says nothing about the other. Here that is
/// sharper still - the encryption sentinel rides the ENCRYPTED column while the
/// mask sentinel rides the masked one, and the PG introspector dispatches on the
/// comment's prefix, so the two prefixes fail independently.
#[test]
fn a_migration_engine_built_table_refuses_an_encryption_downgrade() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg().await;
            let pool = Rc::new(Pool::connect(&url, 4).await.unwrap());
            let app_id = zeroship_core::AppId::mint();
            let encrypted = encrypted_schema();
            let _keys = host.supply_project_key(&[app_id.as_str()], &"01".repeat(32));
            let app =
                fixture_via_the_migration_engine(host, &pool, &url, &app_id, "people", &encrypted)
                    .await;
            let alias = crate::tests::fixtures::harness_alias(&app);

            // The live catalog must retain an encryption marker the runtime recognizes.
            let stored = column_comment(&pool, &app, "people", "secret")
                .await
                .expect("the engine must attach an encryption sentinel to the encrypted column");
            assert!(
                crate::sql::mask_codec::is_encryption_sentinel(&stored),
                "the migration engine must write an encryption marker the runtime recognizes",
            );

            // CONTROL 2: the encrypting deploy stores ciphertext, so this test is not
            // green because writes stopped working.
            let first = insert_through_the_pipeline(
                host,
                &pool,
                &app,
                "people",
                &encrypted,
                value!({ "secret": "hunter2-the-real-one", "nickname": "alice" }),
            )
            .await;
            let before = physical_rows(
                &pool,
                &app,
                "encode(\"secret\", 'escape') AS secret_bytes",
                [first.id.as_str(), first.id.as_str()],
            )
            .await;
            assert_ne!(
                before[&first.id]["secret_bytes"].as_str(),
                Some("hunter2-the-real-one"),
                "control: the encrypting deploy must not store plaintext: {before:?}",
            );

            crate::tests::fixtures::cache_schema(
                &app,
                "people",
                encrypted_schema_without_the_encrypted_key(),
            );
            let mut docs = value!([{ "secret": "hunter3-also-real", "nickname": "bob" }]);
            let err = match host
                .prepare_insert_many_docs(&mut docs, &app, "people", None)
                .await
            {
                Ok(()) => panic!(
                    "a descriptor that dropped the encryption block must not be able \
             to write the plaintext this column still protects, but the \
             pipeline prepared it: {docs}",
                ),
                Err(err) => err,
            };
            assert!(
                format!("{err:?}").contains("protection_removed_from_descriptor"),
                "the refusal must carry the typed code a creator branches on, got {err:?}",
            );
            assert!(
                format!("{err:?}").contains("secret"),
                "and must name the column whose protection went missing, got {err:?}",
            );

            let leaked = pool
                .query_text_params(
                    &format!(
                        "SELECT count(*)::text AS n FROM \"{alias}\".\"people\" \
                 WHERE encode(\"secret\", 'escape') = $1"
                    ),
                    &["hunter3-also-real"],
                )
                .await
                .unwrap();
            assert_eq!(
                leaked[0].get::<_, String>("n"),
                "0",
                "the refused write must not have stored the plaintext",
            );

            release_pg(host, pool).await;
        })
    })
}
