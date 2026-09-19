//! Transaction routing contracts for unmask reads and their audit writes.
//! PostgreSQL comes from the mandatory owned testcontainer.

use crate::tests::fixtures;
#[allow(unused_imports)]
use crate::tests::fixtures::schema::fixture_table_sql;
use crate::tests::fixtures::Host;
#[allow(unused_imports)]
use zeroship_migrate::schema::query::FkEmission;

use std::rc::Rc;

use crate::value::{value, Value};
use compio_postgres::{NoTls, Pool};
use zeroship_data_orm::error::DbError;
use zeroship_data_orm::protection::mask_policy::install_mask_policy;
use zeroship_data_orm::tx_route::{CapturedRoute, TxRoute};

/// Connect, or fail the test. Deliberately NOT a skip: a skipping run of a
/// masking suite is indistinguishable from a passing one.
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
            panic!(
                "the unmask-tx-lane suite could not connect to its PostgreSQL testcontainer: {e}"
            )
        }
    }
}

async fn release_pg(host: &Host, pool: Rc<Pool>) {
    drop(pool);
    host.reset();
    let _ = compio_postgres::drain_connections(std::time::Duration::from_secs(2)).await;
}

/// One masked column and one unmasked control, exactly like `mask_flip`'s.
///
/// `ssn` is masked but NOT encrypted, so the unmask fetch takes
/// `fetch_plaintext_parent` -> `BackendHandle::read_raw_column_text`. The
/// encrypted sibling (`read_raw_column_bytes`) reaches the pool through the
/// same funnel, so the lane question is the same for both.
fn masked_schema() -> Value {
    value!({
        "ssn": {
            "type": "string",
            "mask": { "kind": "full", "classification": "pci" }
        },
        "nickname": { "type": "string" },
    })
}

/// Build the binding's schema from the PLATFORM's own DDL emitter, provision
/// the audit table and the role ladder the unmask path runs under, and install
/// the descriptor entry a deploy would have installed.
async fn fixture(host: &Host, pool: &Rc<Pool>, url: &str, app: &str) {
    fixture_with_schema(host, pool, url, app, masked_schema()).await;
}

async fn fixture_with_schema(host: &Host, pool: &Rc<Pool>, url: &str, app: &str, schema: Value) {
    let binding = crate::tests::fixtures::harness_binding(app);
    let alias = crate::tests::fixtures::harness_alias(app);
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{alias}\" CASCADE"), &[])
        .await
        .unwrap();
    pool.execute(&format!("CREATE SCHEMA \"{alias}\""), &[])
        .await
        .unwrap();
    let ddl = fixture_table_sql(binding.schema(), "people", &schema, &FkEmission::Inline)
        .expect("the platform's own CREATE TABLE emitter");
    pool.batch_execute(&ddl)
        .await
        .unwrap_or_else(|e| panic!("emitted DDL must apply: {e}\n{ddl}"));

    // Provision both tables before the roles so the fixture exercises the same
    // schema-wide data grants as production provisioning.
    pool.batch_execute(&zeroship_migrate_server::provisioning::audit_unmask_table_sql(&alias))
        .await
        .expect("the audit table the deploy provisions");
    crate::tests::fixtures::roles::ensure_binding_ladder(pool, &binding)
        .await
        .expect("the binding ladder, as the deploy would provision it");
    fixtures::grant_all_runtime_table_columns(pool, &binding, "people").await;

    host.install_postgres_pool(Rc::clone(pool), url);
    crate::tests::fixtures::cache_schema(app, "people", schema);
    host.clear_mask_policy_cache(app);
}

/// The backend handle the V8 dispatcher would have bound for this dispatch.
async fn backend(host: &Host) -> zeroship_data_orm::backend::BackendHandle {
    host.backend()
        .await
        .expect("the backend the V8 dispatcher would have opened")
}

/// A route that claims the app's open transaction — what `CapturedRoute::capture`
/// produces for a dispatch issued inside `db.transaction(fn)`.
async fn tx_route(host: &Host, app: &str) -> TxRoute {
    CapturedRoute::tx_on_binding_for_tests(&crate::tests::fixtures::harness_binding(app), crate::sql::registration::SqlRegistration::postgres())
        .bind(backend(host).await)
        .unwrap()
}

/// A route outside any transaction. Dialect stated, as in [`tx_route`].
async fn pool_route(host: &Host, app: &str) -> TxRoute {
    CapturedRoute::pool_for_tests(app, crate::sql::registration::SqlRegistration::postgres())
        .bind(backend(host).await)
        .unwrap()
}

/// Insert one document through the real `run_insert` on `route`, returning the
/// id the platform minted.
async fn insert_on(route: TxRoute, app: &str, doc: Value) -> String {
    let result = zeroship_data_orm::crud::run_insert(
        crate::tests::fixtures::harness_binding(app),
        "people".to_string(),
        route,
        doc,
        None,
    )
    .await
    .expect("the write pipeline + insert builder must apply");
    result.rows[0]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("the write pipeline must mint an id: {:?}", result.rows))
        .to_string()
}

/// Run the real `plan_find` + `run_find` pair on `route`.
async fn find_on(
    route: TxRoute,
    app: &str,
    filter: Value,
    opts: Value,
) -> Result<Vec<Value>, DbError> {
    let binding = crate::tests::fixtures::harness_binding(app);
    let plan = zeroship_data_orm::crud::plan_find(&binding, "people", &filter, &opts).unwrap();
    zeroship_data_orm::crud::run_find(binding, "people".to_string(), route, filter, plan)
        .await
        .map(|r| r.rows)
}

fn code_of(err: &DbError) -> String {
    match err {
        DbError::ValidationFailed { code, .. } => (*code).to_string(),
        DbError::Coded { code, .. } => code.clone(),
        other => format!("{other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 1. The unmask fetch must run on the lane the SELECT ran on
// ---------------------------------------------------------------------------

/// **CONSEQUENCE 1.** A `find({ unmask })` inside `db.transaction(fn)` must
/// reach a row that same transaction inserted.
///
/// The row exists only on the transaction connection, so both the record read
/// and its protected-column read must remain on that lane.
///
/// **The control differs in exactly one token: `opts.unmask`.** Same route,
/// same filter, same row, same transaction. The arm without the hint must
/// return the row, so a failure below cannot be "the fixture wrote nothing" or
/// "the transaction lane is broken" - it can only be the lane the unmask fetch
/// took.
#[test]
fn a_find_unmask_inside_a_transaction_reaches_the_row_that_transaction_inserted() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg().await;
            let pool = Rc::new(Pool::connect(&url, 4).await.unwrap());
            let app = "unmask_lane_uncommitted";
            fixture(host, &pool, &url, app).await;
            // A policy the request's actor satisfies, so the fence GRANTS and the
            // failure below cannot be an authorization refusal wearing another code.
            install_mask_policy(&crate::tests::fixtures::harness_binding(app), value!({ "support": ["pci"] }))
                .expect("install the app's declared mask policy");

            host.begin_transaction(app, &url).await;

            let id = insert_on(
                tx_route(host, app).await,
                app,
                value!({ "ssn": "123-45-6789", "nickname": "aaa" }),
            )
            .await;

            // ---- CONTROL: the same find, same route, same row, no unmask hint.
            let rows = find_on(
                tx_route(host, app).await,
                app,
                value!({ "id": &id }),
                value!({}),
            )
            .await
            .expect("a plain find inside the transaction must see the row it inserted");
            assert_eq!(
                rows.len(),
                1,
                "the transaction lane must see its own uncommitted row; without this the \
         arm below rules on nothing: {rows:?}",
            );
            assert_eq!(rows[0]["id"], value!(id));

            // ---- and the row really is UNCOMMITTED: a pooled read must NOT see it.
            //
            // This is what makes the subject arm a lane question rather than a
            // visibility accident. Same row, same instant, a route that differs only in
            // `in_tx`.
            let outside = find_on(
                pool_route(host, app).await,
                app,
                value!({ "id": &id }),
                value!({}),
            )
            .await
            .expect("a pooled find is authorised to run");
            assert!(
                outside.is_empty(),
                "the row must be invisible outside the transaction, or the subject arm \
         below cannot distinguish the two lanes: {outside:?}",
            );

            // ---- SUBJECT: the same find with the unmask hint.
            let unmasked = find_on(
                tx_route(host, app).await,
                app,
                value!({ "id": &id }),
                value!({
                    "unmask": ["ssn"],
                    "actor": { "kind": "support", "id": "usr_support_1" },
                    "unmaskReason": "unmask tx lane regression",
                }),
            )
            .await;

            host.rollback_transaction(app).await;

            let unmasked = unmasked.unwrap_or_else(|e| {
                panic!(
                    "find({{ unmask }}) inside a transaction must reach the row the \
             transaction inserted. Got {}: {e:?}. The SELECT ran on the \
             transaction connection and found the row (the control above); the \
             unmask fetch took a pooled checkout, which cannot see it.",
                    code_of(&e),
                )
            });
            assert_eq!(unmasked.len(), 1, "the unmasked find returns the same row");
            assert_eq!(
                unmasked[0]["ssn"],
                value!("123-45-6789"),
                "the unmask hint must promote the plaintext: {unmasked:?}",
            );

            release_pg(host, pool).await;
        })
    })
}

// ---------------------------------------------------------------------------
// 2. A denied-unmask audit row and the transaction it was attempted inside
// ---------------------------------------------------------------------------

/// **CONSEQUENCE 2.** The audit row for a DENIED unmask attempted inside a
/// transaction outlives that transaction's ROLLBACK.
///
/// This is a DESIGN DECISION recorded as a test, not a defect report: an
/// independently committed record that an attempt happened SHOULD survive the rollback of
/// the work it was attempted beside, because the attempt really happened. What
/// the test pins is that the behaviour is deliberate and observable, so a later
/// change that folds the audit write into the caller's transaction reddens here
/// rather than silently discarding evidence.
///
/// **The control differs in one variable: whether the write was the audit
/// row.** An ordinary insert made inside the same transaction, rolled back by
/// the same `ROLLBACK`, must be gone. Without it "the audit row is present"
/// would also be satisfied by a transaction that never rolled back at all.
#[test]
fn a_denied_unmask_audit_row_survives_the_rollback_of_its_transaction() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg().await;
            let pool = Rc::new(Pool::connect(&url, 4).await.unwrap());
            let app = "unmask_lane_denied_audit";
            fixture(host, &pool, &url, app).await;
            // The policy grants `support` and nothing else, so `intern` below is
            // refused by the policy path rather than by the no-policy fallback.
            install_mask_policy(&crate::tests::fixtures::harness_binding(app), value!({ "support": ["pci"] }))
                .expect("install the app's declared mask policy");

            // A committed row for the denied attempt to name. Committed so the arm
            // cannot be confused with consequence 1.
            let committed = insert_on(
                pool_route(host, app).await,
                app,
                value!({ "ssn": "111-11-1111", "nickname": "committed" }),
            )
            .await;
            assert_eq!(
                audit_rows(&pool, app).await.len(),
                0,
                "no unmask has been attempted yet",
            );

            host.begin_transaction(app, &url).await;

            // The control write: an ordinary insert that shares the transaction the
            // denied attempt is made inside.
            let rolled_back = insert_on(
                tx_route(host, app).await,
                app,
                value!({ "ssn": "222-22-2222", "nickname": "rolled back" }),
            )
            .await;

            let err = find_on(
                tx_route(host, app).await,
                app,
                value!({ "id": &committed }),
                value!({
                    "unmask": ["ssn"],
                    "actor": { "kind": "intern", "id": "usr_intern_1" },
                    "unmaskReason": "unmask tx lane regression",
                }),
            )
            .await
            .expect_err("an actor the policy does not permit must be refused");
            assert_eq!(
                code_of(&err),
                "unmask_not_permitted",
                "the refusal must be the authorization one: {err:?}",
            );

            // ROLLBACK the transaction both writes were made inside.
            host.rollback_transaction(app).await;

            // ---- CONTROL: the ordinary write inside that transaction is gone.
            let surviving = pool
                .query_text_params(
                    &format!(
                        "SELECT id FROM \"{}\".\"people\" ORDER BY id",
                        crate::tests::fixtures::harness_alias(app)
                    ),
                    &[],
                )
                .await
                .unwrap();
            let surviving: Vec<String> =
                surviving.iter().map(|r| r.get::<_, String>("id")).collect();
            assert_eq!(
                surviving,
                vec![committed.clone()],
                "the ROLLBACK must have destroyed the in-transaction insert {rolled_back}; \
         without that the audit assertion below proves nothing",
            );

            // ---- SUBJECT: the denied audit row is still there.
            let audit = audit_rows(&pool, app).await;
            assert_eq!(
                audit.len(),
                1,
                "the denied attempt must leave exactly one durable audit row: {audit:?}",
            );
            assert_eq!(audit[0]["outcome"], value!("denied"));
            assert_eq!(audit[0]["actor_role"], value!("intern"));
            assert_eq!(
                audit[0]["column"],
                value!("ssn"),
                "the audit row names the logical field",
            );

            release_pg(host, pool).await;
        })
    })
}

// ---------------------------------------------------------------------------
// 3. The ENCRYPTED fetch, through the single-cell dispatcher
// ---------------------------------------------------------------------------

/// **CONSEQUENCE 1, on the other fetch helper and the other entry point.**
///
/// Test 1 covers `dispatch_unmask_for_query` over a masked-but-unencrypted
/// column, which reads the raw sibling as TEXT. That leaves half the operation
/// unbound: `MaskedValue.unmask()` reaches `dispatch_unmask`, and an ENCRYPTED
/// column reads the sibling as BYTES through a different arm. Both were on the
/// autocommit lane for the same reason, and a fix to one is not evidence about
/// the other.
///
/// **The control differs in one variable: `route.in_tx()`.** The same call,
/// over the same row, with a route captured outside the transaction must fail
/// `unmask_not_found` - because the row genuinely is not visible there. That is
/// what makes the passing arm a statement about the lane and not about the
/// fixture.
#[test]
fn an_encrypted_unmask_inside_a_transaction_reaches_the_row_that_transaction_inserted() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg().await;
            let pool = Rc::new(Pool::connect(&url, 4).await.unwrap());
            let app = "unmask_lane_encrypted";
            // A synthetic 32-byte root, supplied to THIS isolate. The write pipeline
            // encrypts with it and the unmask fetch decrypts with it.
            let _keys = host.supply_project_key(&[app], &"c".repeat(64));
            fixture_with_schema(host, &pool, &url, app, encrypted_schema()).await;
            install_mask_policy(&crate::tests::fixtures::harness_binding(app), value!({ "support": ["pci"] }))
                .expect("install the app's declared mask policy");

            host.begin_transaction(app, &url).await;

            let id = insert_on(
                tx_route(host, app).await,
                app,
                value!({ "ssn": "123-45-6789", "nickname": "aaa" }),
            )
            .await;

            let args = || zeroship_data_orm::protection::unmask::UnmaskFieldArgs {
                collection: "people".to_string(),
                row_pk: id.clone(),
                column: "ssn".to_string(),
                actor: Some(value!({ "kind": "support", "id": "usr_support_1" })),
                reason: Some("unmask tx lane regression".to_string()),
                rejected_claim: None,
            };

            // ---- CONTROL: outside the transaction the row is genuinely unreachable.
            let outside = zeroship_data_orm::protection::unmask::dispatch_unmask(
                &pool_route(host, app).await,
                &crate::tests::fixtures::harness_binding(app),
                args(),
            )
            .await
            .expect_err("a pooled unmask cannot see the uncommitted row");
            assert_eq!(
                code_of(&outside),
                "unmask_not_found",
                "the control must fail for the visibility reason, not another: {outside:?}",
            );

            // ---- SUBJECT: the same call on the transaction's own lane.
            let inside = zeroship_data_orm::protection::unmask::dispatch_unmask(
                &tx_route(host, app).await,
                &crate::tests::fixtures::harness_binding(app),
                args(),
            )
            .await;

            host.rollback_transaction(app).await;

            let inside = inside.unwrap_or_else(|e| {
                panic!(
                    "an unmask inside a transaction must reach the row that transaction \
             inserted. Got {}: {e:?}",
                    code_of(&e),
                )
            });
            assert_eq!(
                inside.plaintext, "123-45-6789",
                "the ciphertext must be read on the transaction's connection and decrypted",
            );

            release_pg(host, pool).await;
        })
    })
}

/// [`masked_schema`] with the masked column also ENCRYPTED, so its raw sibling
/// is BYTEA and the fetch takes `read_raw_column_bytes`.
fn encrypted_schema() -> Value {
    value!({
        "ssn": {
            "type": "string",
            "mask": { "kind": "last4", "classification": "pci" },
            "encrypted": true
        },
        "nickname": { "type": "string" },
    })
}

/// Every audit row in the binding's schema, oldest first.
async fn audit_rows(pool: &Rc<Pool>, app: &str) -> Vec<Value> {
    let alias = crate::tests::fixtures::harness_alias(app);
    let rows = pool
        .query_text_params(
            &format!(
                "SELECT outcome, actor_id, actor_role, claimed_actor, \"column\", row_pk, reason \
                 FROM \"{alias}\".\"__zeroship_audit_unmask\" ORDER BY id"
            ),
            &[],
        )
        .await
        .unwrap();
    rows.iter()
        .map(|row| {
            let mut map = crate::value::Map::new();
            for (i, column) in row.columns().iter().enumerate() {
                let value: Option<String> = row.try_get(i).unwrap_or(None);
                map.insert(
                    column.name().to_string(),
                    value.map_or(Value::Null, Value::String),
                );
            }
            Value::Object(map)
        })
        .collect()
}
