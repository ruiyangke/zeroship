//! SQLite unmask contracts.
use super::fixtures::*;

use crate::tests::fixtures::Host;

use std::rc::Rc;

use zeroship_data_orm::backend::sqlite::SqliteBackend;

use zeroship_data_orm::binding::DbBinding;

use zeroship_data_sql::compile::raw_column_name;

use zeroship_data_orm::protection::unmask;

use zeroship_data_orm::protection::mask_policy;

use zeroship_data_orm::protection::unmask::{BulkUnmaskArgs, BulkUnmaskItem, dispatch_bulk_unmask};

use zeroship_data_orm::protection::unmask::{
    audit_query_hint_granted, authorize_query_hint, dispatch_unmask_for_query,
};

use zeroship_data_orm::protection::Catalog;

#[cfg(test)]
use crate::tests::fixtures::DatabaseFixture;

/// Open the fixture backend before calling the ORM directly. This helper warms
/// the backend; adapter tests cover opening it through a creator operation.
async fn unmask_backend(host: &Host) -> zeroship_data_orm::backend::BackendHandle {
    host.backend()
        .await
        .expect("the backend the V8 dispatcher would have opened")
}

/// Bind a route from the fixture’s backend and current transaction scope.
async fn unmask_route(host: &Host, app: &str) -> zeroship_data_orm::tx_route::TxRoute {
    zeroship_data_orm::exec::ambient_route_for_tests(app, unmask_backend(host).await)
}

/// Drop the fixture-installed backend while preserving its on-disk databases,
/// then reinstall the app descriptor and policy as a fresh startup would.
/// The database remains cold until its first operation.
fn configure_cold_sqlite_unmask_fixture(
    host: &Host,
    dir: &tempfile::TempDir,
    app_id: &str,
    collection: &str,
    schema: zeroship_data_sql::value::Value,
    policy: zeroship_data_sql::value::Value,
) {
    host.reset();
    let url = format!("sqlite:{}", dir.path().join("zs-control.sqlite").display());
    host.set_database_url(&url);
    crate::tests::fixtures::cache_schema(app_id, collection, schema);
    mask_policy::install_mask_policy(&DbBinding::cold_start(app_id), policy)
        .expect("reinstall the app declaration during startup");
}

/// The cold-open gate: prove the isolate left by
/// [`configure_cold_sqlite_unmask_fixture`] has NO backend, and that
/// `tx_scope::ensure_backend` is what opens and installs one.
///
/// Native unmask operations open their backend lazily. Policy installation
/// has already run at startup without touching the database. The V8 dispatch
/// wiring is covered by `v8_classes::cold_open`; this helper verifies that the
/// resolver opens a fresh backend and keeps using that instance.
///
/// # Why identity, and not a context read
///
/// `crate::context` is `pub(crate)` in every build, so an integration target
/// cannot ask "is the backend slot empty" directly. It can ask something
/// stronger: the handle that comes back must not be the one the fixture
/// installed, and a SECOND resolution must return that same fresh handle rather
/// than open a third. The caller keeps its fixture `Rc` alive across this call
/// for exactly that reason - a dropped `SqliteBackend` could be reallocated at
/// the same address and make the first assertion pass on a coincidence.
async fn assert_cold_open_installs_a_fresh_backend(host: &Host, fixture: &SqliteBackend) {
    let opened = host.backend().await.expect(
        "a cold isolate must be OPENED by ensure_backend: a plain context read answers \
             not_configured here, which is what every fresh isolate would get",
    );
    let opened = opened
        .get::<zeroship_data_orm::backend::SqliteBackend>()
        .expect("the SQLite arm");
    assert!(
        !std::ptr::eq(opened, fixture),
        "the cold fixture's backend is still installed, so nothing was opened"
    );

    let again = host
        .backend()
        .await
        .expect("the second resolution must see the backend the first one opened");
    assert!(
        std::ptr::eq(
            again
                .get::<zeroship_data_orm::backend::SqliteBackend>()
                .expect("the SQLite arm"),
            opened
        ),
        "the open must INSTALL into the isolate, not hand back a private handle: \
         a second resolution opened a different backend"
    );
}

/// Read every row from `__zeroship_audit_unmask` for a given app.
/// Returns `Vec<(outcome, actor_role, classification)>`.
async fn read_audit_rows(backend: &SqliteBackend, app_id: &str) -> Vec<(String, String, String)> {
    // A read: it belongs on `op_conn`, not on the exclusive `tx_conn`
    // reservation. Asking for the transaction lane here contends with whatever
    // the unmask dispatch itself is holding.
    let client = backend.autocommit_client();
    let q_app = zeroship_data_sql::compile::quote_ident(app_id);
    let sql = format!(
        r#"SELECT outcome, actor_role, classification
           FROM {q_app}."__zeroship_audit_unmask"
           ORDER BY id"#
    );
    let rows = client.query(&sql, &[]).await.expect("query audit rows");
    rows.into_iter()
        .map(|r| {
            (
                r[0].clone().unwrap_or_default(),
                r[1].clone().unwrap_or_default(),
                r[2].clone().unwrap_or_default(),
            )
        })
        .collect()
}

/// **cold-open gate, single unmask**: the fixture leaves the isolate with a URL
/// and no backend, and `tx_scope::ensure_backend` - the call
/// `v8_classes::dispatch::dispatch_unmask_field` makes before handing the engine
/// a handle - is what opens one.
///
/// The sibling gate below drives the same cold fixture through
/// `dispatch_unmask` and rules on the ATTACH; this one rules on the OPEN, which
/// nothing else does. See [`assert_cold_open_installs_a_fresh_backend`].
#[test]
fn cold_unmask_open_comes_from_ensure_backend_not_the_fixture() {
    Host::test(|host| {
        let schema = zeroship_data_sql::value!({
            "id":  { "type": "string" },
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" },
            },
        });
        let app_id = "app_unmask_cold_open";
        let collection = "users";

        host.run(async {
            // `fixture` stays bound for the whole block: the assertion is an
            // address comparison against it.
            let (fixture, dir) =
                unmask_setup_with_schema(host, app_id, collection, schema.clone()).await;
            configure_cold_sqlite_unmask_fixture(
                host,
                &dir,
                app_id,
                collection,
                schema,
                zeroship_data_sql::value!({}),
            );
            assert_cold_open_installs_a_fresh_backend(host, fixture.as_ref()).await;
        });
    })
}

/// **Gate #1**: an `auto` actor unmasking an encrypted +
/// masked column recovers plaintext, and a `granted` audit row is
/// emitted with the right classification.
///
/// The ATTACH is what this rules on. The OPEN is the harness's - see
/// `unmask_backend` - and is bound by the cold-open gate directly above.
#[test]
fn cold_unmask_with_auto_actor_attaches_before_read() {
    Host::test(|host| {
        let _keys = host.supply_project_key(&["app_unmask_auto"], &"a".repeat(64));
        let schema = zeroship_data_sql::value!({
            "id": { "type": "string" },
            "ssn": {
                "type": "string",
                "encrypted": true,
                "mask": { "kind": "last4", "classification": "spi" },
            },
        });
        let app_id = "app_unmask_auto";
        let collection = "users";

        host.run(async {
            let (backend, dir) =
                unmask_setup_with_schema(host, app_id, collection, schema.clone()).await;
            // Manually create the table — the encryption pass + dual-write
            // pipeline lives in CRUD, but the unmask SELECT only needs
            // `id TEXT PRIMARY KEY, "<raw ssn>" BLOB, ssn TEXT`. Mirrors the
            // e2e CRUD test. Post-storage-flip layout: the raw column (named
            // via `raw_column_name`, never spelled out here) holds the
            // ciphertext `protection::unmask` reads; the field's own column (`ssn`)
            // holds the mask, exactly as a default read pipeline would leave it.
            let raw_ssn = raw_column_name("ssn");
            backend
                .execute_fixture(
                    &format!(
                        "CREATE TABLE \"app_unmask_auto\".\"users\" (\
                         id  TEXT PRIMARY KEY, \
                         \"{raw_ssn}\" BLOB, \
                         ssn TEXT NOT NULL DEFAULT '***-**-XXXX'\
                     )"
                    ),
                    &[],
                )
                .await
                .expect("CREATE TABLE");

            // Encrypt + insert one row inline.
            use zeroship_data_orm::protection::encryption_pass::encrypt_row_on_write;
            use zeroship_data_sql::compile::{SqlDialect, build_insert_with_dialect};
            let row_pk = "usr_auto_01";
            let plaintext = "123-45-6789";
            let mut doc = zeroship_data_sql::value!({
                "id": row_pk,
                "ssn": plaintext,
            });
            encrypt_row_on_write(
                backend.key_store(),
                app_id,
                collection,
                &schema,
                row_pk,
                &mut doc,
            )
            .await
            .expect("encrypt_row_on_write");
            // Relocate the native ciphertext to the raw column, as the mask pass
            // does, and store the precomputed mask in the logical column.
            let ciphertext = doc
                .as_object_mut()
                .expect("doc object")
                .shift_remove("ssn")
                .expect("ciphertext produced by encrypt_row_on_write");
            {
                let obj = doc.as_object_mut().expect("doc object");
                obj.insert(raw_ssn.clone(), ciphertext);
                obj.insert("ssn".to_string(), zeroship_data_sql::value!("***-**-6789"));
            }
            let bq = build_insert_with_dialect(
                &zeroship_data_sql::SchemaName::new(app_id).expect("fixture schema name"),
                collection,
                &schema,
                &doc,
                SqlDialect::Sqlite,
            )
            .expect("build_insert_with_dialect");
            let client = backend
                .fixture_session(app_id)
                .await
                .expect("acquire client");
            let param_refs = &bq.params;
            let _ = client
                .query_typed(&bq.sql, param_refs)
                .await
                .expect("INSERT");

            // Remove the fixture-installed backend. The `unmask_backend()` below is
            // the harness making the open the V8 dispatch makes in production (the
            // cold-open gate above is where that open is ruled on); what THIS test
            // rules on is the next step - `dispatch_unmask` ATTACHing the existing
            // app file to that freshly opened connection before its direct SELECT.
            configure_cold_sqlite_unmask_fixture(
                host,
                &dir,
                app_id,
                collection,
                schema.clone(),
                zeroship_data_sql::value!({}),
            );
            let _cold_keys = host.supply_project_key(&["app_unmask_auto"], &"a".repeat(64));

            // Dispatch unmask with `kind: "auto"` actor — must succeed.
            let args = unmask::UnmaskFieldArgs {
                collection: collection.to_string(),
                row_pk: row_pk.to_string(),
                column: "ssn".to_string(),
                actor: Some(zeroship_data_sql::value!({ "kind": "auto", "id": null })),
                reason: Some("integration test".to_string()),
                rejected_claim: None,
            };
            let result = unmask::dispatch_unmask(
                &unmask_route(host, app_id).await,
                &DbBinding::cold_start(app_id),
                args,
            )
            .await
            .expect("dispatch_unmask must succeed for auto actor");
            assert_eq!(
                result.plaintext, plaintext,
                "plaintext must recover via decrypt path"
            );

            // Audit row must show `granted` + `spi`.
            let audit = read_audit_rows(backend.as_ref(), app_id).await;
            assert_eq!(audit.len(), 1, "exactly one audit row expected: {audit:?}");
            assert_eq!(audit[0].0, "granted", "outcome must be granted");
            assert_eq!(audit[0].1, "auto", "actor_role must be 'auto'");
            assert_eq!(
                audit[0].2, "spi",
                "classification must be 'spi' (from the schema mask block)"
            );

            // A direct read of the field's own column (what a default read
            // pipeline would see, no audit, no authorization check) must
            // still be the mask, never the plaintext - the audited path above
            // is the only way to recover it.
            let direct = client
                .query(
                    "SELECT ssn FROM \"app_unmask_auto\".\"users\" WHERE id = 'usr_auto_01'",
                    &[],
                )
                .await
                .expect("direct SELECT of the field's own column");
            assert_eq!(
                direct[0][0].as_deref(),
                Some("***-**-6789"),
                "field's own column must hold the mask"
            );
            assert_ne!(
                direct[0][0].as_deref(),
                Some(plaintext),
                "field's own column must never hold the plaintext"
            );
        });
    })
}

/// **Gate #2**: a `user`-kind actor is denied by the
/// default-policy stub; a `denied` audit row is emitted; the typed
/// error `unmask_not_permitted` reaches the caller.
#[test]
fn unmask_with_user_actor_returns_forbidden_audit_logged() {
    Host::test(|host| {
        let _keys = host.supply_project_key(&["app_unmask_user"], &"b".repeat(64));
        let schema = zeroship_data_sql::value!({
            "id": { "type": "string" },
            "ssn": {
                "type": "string",
                "encrypted": true,
                "mask": { "kind": "last4", "classification": "spi" },
            },
        });
        let app_id = "app_unmask_user";
        let collection = "users";

        host.run(async {
            let (backend, _dir) =
                unmask_setup_with_schema(host, app_id, collection, schema.clone()).await;
            backend
                .execute_fixture(
                    "CREATE TABLE \"app_unmask_user\".\"users\" (\
                     id  TEXT PRIMARY KEY, \
                     ssn BLOB, \
                     ssn_masked TEXT NOT NULL DEFAULT '***'\
                 )",
                    &[],
                )
                .await
                .expect("CREATE TABLE");

            // No need to insert a row — the authorization check happens
            // BEFORE the SELECT, so a denied path doesn't touch the data
            // table at all. Even if the row exists, the SELECT is gated
            // by `allowed = false`.
            let args = unmask::UnmaskFieldArgs {
                collection: collection.to_string(),
                row_pk: "usr_anywhere".to_string(),
                column: "ssn".to_string(),
                actor: Some(zeroship_data_sql::value!({ "kind": "user", "id": "usr_xyz" })),
                reason: None,
                rejected_claim: None,
            };
            let err = unmask::dispatch_unmask(
                &unmask_route(host, app_id).await,
                &DbBinding::cold_start(app_id),
                args,
            )
            .await
            .expect_err("dispatch_unmask must refuse user actor under PR 4 stub");
            match err {
                zeroship_data_orm::error::DbError::Coded { code, .. } => {
                    assert_eq!(code, "unmask_not_permitted");
                }
                other => panic!("expected Coded::unmask_not_permitted, got {other:?}"),
            }

            let audit = read_audit_rows(backend.as_ref(), app_id).await;
            assert_eq!(audit.len(), 1, "denied path must still emit one audit row");
            assert_eq!(audit[0].0, "denied", "outcome must be 'denied'");
            assert_eq!(audit[0].1, "user", "actor_role must be 'user'");
            assert_eq!(
                audit[0].2, "spi",
                "classification must be 'spi' even on denied path"
            );
        });
    })
}

/// **Gate #3**: unmask of a column that has no mask
/// declaration on the cached schema returns the typed
/// `unmask_column_not_masked` error. Pins the contract that the
/// dispatcher refuses to leak plaintext through a "forged" RPC for
/// arbitrary columns.
#[test]
fn unmask_column_not_masked_returns_typed_error() {
    Host::test(|host| {
        // Schema declares `name` as a bare string — no mask block.
        let schema = zeroship_data_sql::value!({
            "id":   { "type": "string" },
            "name": { "type": "string" },
        });
        let app_id = "app_unmask_unmasked";
        let collection = "users";

        host.run(async {
            let (_backend, _dir) = unmask_setup_with_schema(host, app_id, collection, schema).await;
            let args = unmask::UnmaskFieldArgs {
                collection: collection.to_string(),
                row_pk: "any_pk".to_string(),
                column: "name".to_string(),
                actor: Some(zeroship_data_sql::value!({ "kind": "auto" })),
                reason: None,
                rejected_claim: None,
            };
            let err = unmask::dispatch_unmask(
                &unmask_route(host, app_id).await,
                &DbBinding::cold_start(app_id),
                args,
            )
            .await
            .expect_err("unmask of non-masked column must refuse");
            match err {
                zeroship_data_orm::error::DbError::ValidationFailed { code, .. } => {
                    assert_eq!(code, "unmask_column_not_masked");
                }
                other => {
                    panic!("expected ValidationFailed::unmask_column_not_masked, got {other:?}")
                }
            }
        });
    })
}

/// **Gate #4**: classification flows through to the audit
/// row regardless of outcome. We register a column with `classification:
/// "phi"`, force the denied path (user actor), and assert the audit
/// row's classification text matches.
#[test]
fn unmask_writes_audit_row_with_correct_classification() {
    Host::test(|host| {
        let schema = zeroship_data_sql::value!({
            "id":      { "type": "string" },
            "diag":    {
                "type": "string",
                // Mask-only (no encryption) — exercises the plaintext-storage
                // fetch path indirectly (although the denied branch never
                // reaches it). The dispatcher's denied audit-row write still
                // pulls classification from the cached schema.
                "mask": { "kind": "full", "classification": "phi" },
            },
        });
        let app_id = "app_unmask_phi";
        let collection = "patients";

        host.run(async {
            let (backend, _dir) = unmask_setup_with_schema(host, app_id, collection, schema).await;
            let args = unmask::UnmaskFieldArgs {
                collection: collection.to_string(),
                row_pk: "pat_01".to_string(),
                column: "diag".to_string(),
                actor: Some(zeroship_data_sql::value!({ "kind": "user", "id": "doctor_x" })),
                reason: Some("chart review".to_string()),
                rejected_claim: None,
            };
            let _err = unmask::dispatch_unmask(
                &unmask_route(host, app_id).await,
                &DbBinding::cold_start(app_id),
                args,
            )
            .await
            .expect_err("user actor denied");

            let audit = read_audit_rows(backend.as_ref(), app_id).await;
            assert_eq!(audit.len(), 1);
            assert_eq!(audit[0].0, "denied");
            assert_eq!(
                audit[0].2, "phi",
                "classification 'phi' must round-trip onto the audit row"
            );
        });
    })
}

/// The unmask SELECT names the raw column the DESCRIPTOR declares.
///
/// The end-to-end half of the change `zeroship_data_sql::compile::declared_raw_column`
/// carries. The unit tests in `zeroship-data-orm`'s `protection::mask_pass` bind the
/// WRITE side - which column the plaintext is relocated INTO - in the engine's
/// default-feature build. Nothing there rules on the READ, because the read is a
/// SELECT against a real database and the fetch helpers are private.
///
/// Coherence is the property, not tidiness: if the write pass places the value
/// by the descriptor's name and this SELECT keeps formatting its own, every
/// unmask of a renamed column fails with "no such column" on a row that is
/// perfectly well stored. The fixture therefore declares a name
/// `raw_column_name` does NOT produce, and the table has ONLY that column - so a
/// dispatch that re-derives cannot accidentally find the value.
///
/// Mask-only (no `encrypted` block) so it lands on `fetch_plaintext_parent`; the
/// encrypted twin reads the same resolved name through `fetch_and_decrypt`.
#[test]
fn unmask_reads_the_raw_column_the_descriptor_declares() {
    Host::test(|host| {
        let declared_raw = "__zs_raw2__ssn";
        let schema = zeroship_data_sql::value!({
            "id": { "type": "string" },
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" },
                "storage": { "valueColumn": "ssn", "rawColumn": declared_raw },
            },
        });
        let app_id = "app_unmask_declared_raw";
        let collection = "people";

        host.run(async {
            let (backend, _dir) = unmask_setup_with_schema(host, app_id, collection, schema).await;
            assert_ne!(
                declared_raw,
                raw_column_name("ssn"),
                "the fixture must declare a name the derivation does not produce, or it \
             passes against a dispatch that ignores the descriptor",
            );
            backend
                .execute_fixture(
                    &format!(
                        "CREATE TABLE \"{app_id}\".\"{collection}\" (\
                         id TEXT PRIMARY KEY, \"{declared_raw}\" TEXT, ssn TEXT)"
                    ),
                    &[],
                )
                .await
                .expect("CREATE TABLE");
            backend
                .execute_fixture(
                    &format!(
                        "INSERT INTO \"{app_id}\".\"{collection}\" (id, \"{declared_raw}\", ssn) \
                     VALUES ('per_01', '123-45-6789', '***-**-6789')"
                    ),
                    &[],
                )
                .await
                .expect("INSERT");

            let args = unmask::UnmaskFieldArgs {
                collection: collection.to_string(),
                row_pk: "per_01".to_string(),
                column: "ssn".to_string(),
                actor: Some(zeroship_data_sql::value!({ "kind": "auto", "id": null })),
                reason: Some("integration test".to_string()),
                rejected_claim: None,
            };
            let result = unmask::dispatch_unmask(
                &unmask_route(host, app_id).await,
                &DbBinding::cold_start(app_id),
                args,
            )
            .await
            .expect("the unmask SELECT must name the declared raw column");
            assert_eq!(result.plaintext, "123-45-6789");
        });
    })
}

/// Helper — install backend + schema + clean any pre-existing cached
/// policy for the app. Returns the backend (kept alive via Rc) and the
/// TempDir guard. Drains the cache so the test starts from
/// "no-policy-declared".
async fn policy_setup(
    host: &Host,
    app_id: &str,
    collection: &str,
    schema: zeroship_data_sql::value::Value,
) -> (Rc<SqliteBackend>, tempfile::TempDir) {
    let (backend, dir) = unmask_setup_with_schema(host, app_id, collection, schema).await;
    host.clear_mask_policy_cache(app_id);
    (backend, dir)
}

/// **Gate #1**: a policy granting `user` access to `pii`
/// allows a user-role actor to unmask a pii-classified column.
#[test]
fn unmask_with_user_role_in_policy_returns_plaintext() {
    Host::test(|host| {
        let _keys = host.supply_project_key(&["app_unmask_policy_grant"], &"c".repeat(64));
        let schema = zeroship_data_sql::value!({
            "id": { "type": "string" },
            "email": {
                "type": "string",
                "encrypted": true,
                "mask": { "kind": "email", "classification": "pii" },
            },
        });
        let app_id = "app_unmask_policy_grant";
        let collection = "users";

        host.run(async {
            let (backend, _dir) = policy_setup(host, app_id, collection, schema.clone()).await;

            // Define the policy: `user` can unmask `pii`.
            let policy_v = zeroship_data_sql::value!({
                "user": ["public", "pii"],
            });
            mask_policy::install_mask_policy(&DbBinding::cold_start(app_id), policy_v)
                .expect("set_mask_policy must succeed");

            // Post-storage-flip layout: the raw column (named via
            // `raw_column_name`, never spelled out here) holds the ciphertext
            // `protection::unmask` reads; the field's own column (`email`) holds the
            // mask, exactly as a default read pipeline would leave it.
            let raw_email = raw_column_name("email");
            backend
                .execute_fixture(
                    &format!(
                        "CREATE TABLE \"app_unmask_policy_grant\".\"users\" (\
                         id    TEXT PRIMARY KEY, \
                         \"{raw_email}\" BLOB, \
                         email TEXT NOT NULL DEFAULT 'x***@***'\
                     )"
                    ),
                    &[],
                )
                .await
                .expect("CREATE TABLE");

            // Encrypt + insert one row.
            use zeroship_data_orm::protection::encryption_pass::encrypt_row_on_write;
            use zeroship_data_sql::compile::{SqlDialect, build_insert_with_dialect};
            let row_pk = "usr_grant_01";
            let plaintext = "alice@example.com";
            let mut doc = zeroship_data_sql::value!({
                "id": row_pk,
                "email": plaintext,
            });
            encrypt_row_on_write(
                backend.key_store(),
                app_id,
                collection,
                &schema,
                row_pk,
                &mut doc,
            )
            .await
            .expect("encrypt_row_on_write");
            // Relocate the native ciphertext to the raw column, as the mask pass
            // does, and store the precomputed mask in the logical column.
            let ciphertext = doc
                .as_object_mut()
                .expect("doc object")
                .shift_remove("email")
                .expect("ciphertext produced by encrypt_row_on_write");
            {
                let obj = doc.as_object_mut().expect("doc object");
                obj.insert(raw_email.clone(), ciphertext);
                obj.insert(
                    "email".to_string(),
                    zeroship_data_sql::value!("a****@example.com"),
                );
            }
            let bq = build_insert_with_dialect(
                &zeroship_data_sql::SchemaName::new(app_id).expect("fixture schema name"),
                collection,
                &schema,
                &doc,
                SqlDialect::Sqlite,
            )
            .expect("build_insert_with_dialect");
            let client = backend
                .fixture_session(app_id)
                .await
                .expect("acquire client");
            let param_refs = &bq.params;
            let _ = client
                .query_typed(&bq.sql, param_refs)
                .await
                .expect("INSERT");

            // Unmask with `user` actor — must succeed via the policy.
            let args = unmask::UnmaskFieldArgs {
                collection: collection.to_string(),
                row_pk: row_pk.to_string(),
                column: "email".to_string(),
                actor: Some(zeroship_data_sql::value!({ "kind": "user", "id": "usr_xyz" })),
                reason: Some("user requested own data".to_string()),
                rejected_claim: None,
            };
            let result = unmask::dispatch_unmask(
                &unmask_route(host, app_id).await,
                &DbBinding::cold_start(app_id),
                args,
            )
            .await
            .expect("policy grants user → pii; unmask must succeed");
            assert_eq!(result.plaintext, plaintext);

            let audit = read_audit_rows(backend.as_ref(), app_id).await;
            assert_eq!(audit.len(), 1);
            assert_eq!(audit[0].0, "granted", "outcome must be granted");
            assert_eq!(audit[0].1, "user");
            assert_eq!(audit[0].2, "pii");

            // A direct read of the field's own column (what a default read
            // pipeline would see, no audit, no authorization check) must
            // still be the mask, never the plaintext.
            let direct = client
            .query(
                "SELECT email FROM \"app_unmask_policy_grant\".\"users\" WHERE id = 'usr_grant_01'",
                &[],
            )
            .await
            .expect("direct SELECT of the field's own column");
            assert_eq!(
                direct[0][0].as_deref(),
                Some("a****@example.com"),
                "field's own column must hold the mask"
            );
            assert_ne!(
                direct[0][0].as_deref(),
                Some(plaintext),
                "field's own column must never hold the plaintext"
            );
        });
    })
}

/// **Gate #2**: a policy granting `user` only `public` denies
/// a user-role attempt to unmask a `pii`-classified column. The denied
/// path emits an audit row.
#[test]
fn unmask_with_user_role_not_in_policy_denied() {
    Host::test(|host| {
        let schema = zeroship_data_sql::value!({
            "id": { "type": "string" },
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "pii" },
            },
        });
        let app_id = "app_unmask_policy_deny";
        let collection = "users";

        host.run(async {
            let (backend, _dir) = policy_setup(host, app_id, collection, schema.clone()).await;

            // Policy: `user` can only unmask `public`.
            let policy_v = zeroship_data_sql::value!({
                "user": ["public"],
            });
            mask_policy::install_mask_policy(&DbBinding::cold_start(app_id), policy_v)
                .expect("set_mask_policy must succeed");

            let args = unmask::UnmaskFieldArgs {
                collection: collection.to_string(),
                row_pk: "usr_anywhere".to_string(),
                column: "ssn".to_string(),
                actor: Some(zeroship_data_sql::value!({ "kind": "user", "id": "usr_xyz" })),
                reason: None,
                rejected_claim: None,
            };
            let err = unmask::dispatch_unmask(
                &unmask_route(host, app_id).await,
                &DbBinding::cold_start(app_id),
                args,
            )
            .await
            .expect_err("policy does not allow user → pii; must refuse");
            match err {
                zeroship_data_orm::error::DbError::Coded { code, .. } => {
                    assert_eq!(code, "unmask_not_permitted");
                }
                other => panic!("expected Coded::unmask_not_permitted, got {other:?}"),
            }

            let audit = read_audit_rows(backend.as_ref(), app_id).await;
            assert_eq!(audit.len(), 1);
            assert_eq!(audit[0].0, "denied");
            assert_eq!(audit[0].1, "user");
            assert_eq!(audit[0].2, "pii");
        });
    })
}

/// **Gate #3**: regression guard for the no-policy case.
/// The default-deny stub still applies: `auto` allowed,
/// everyone else denied. Closes the "did we accidentally start
/// allowing everything when no policy is declared" hole.
#[test]
fn unmask_default_deny_when_no_policy() {
    Host::test(|host| {
        let schema = zeroship_data_sql::value!({
            "id": { "type": "string" },
            "name": {
                "type": "string",
                "mask": { "kind": "name", "classification": "public" },
            },
        });
        let app_id = "app_unmask_policy_default_deny";
        let collection = "users";

        host.run(async {
            let (backend, _dir) = policy_setup(host, app_id, collection, schema).await;
            // NO setMaskPolicy call — exercise the default-deny stub.

            let args = unmask::UnmaskFieldArgs {
                collection: collection.to_string(),
                row_pk: "usr_anywhere".to_string(),
                column: "name".to_string(),
                actor: Some(zeroship_data_sql::value!({ "kind": "user", "id": "usr_xyz" })),
                reason: None,
                rejected_claim: None,
            };
            let err = unmask::dispatch_unmask(
                &unmask_route(host, app_id).await,
                &DbBinding::cold_start(app_id),
                args,
            )
            .await
            .expect_err("no policy + non-auto actor → default-deny");
            match err {
                zeroship_data_orm::error::DbError::Coded { code, .. } => {
                    assert_eq!(code, "unmask_not_permitted");
                }
                other => panic!("expected Coded::unmask_not_permitted, got {other:?}"),
            }
            // Audit row written on the denied path.
            let audit = read_audit_rows(backend.as_ref(), app_id).await;
            assert_eq!(audit.len(), 1);
            assert_eq!(audit[0].0, "denied");
        });
    })
}

/// **Gate #4**: invalid classification at the Rust validator.
/// The SDK's `defineMaskPolicy()` rejects at declare-time; the Rust
/// validator catches anything that bypasses the SDK (forged RPC,
/// untrusted client, future SDK drift). Both layers refuse with
/// `invalid_mask_classification`.
#[test]
fn unmask_invalid_classification_rejected_at_dispatch_time() {
    Host::test(|host| {
        let schema = zeroship_data_sql::value!({ "id": { "type": "string" } });
        let app_id = "app_unmask_invalid_classification";
        let collection = "users";

        host.run(async {
            let (_backend, _dir) = policy_setup(host, app_id, collection, schema).await;

            let bad_policy = zeroship_data_sql::value!({
                "admin": ["public", "badclass"],
            });
            let err = mask_policy::install_mask_policy(&DbBinding::cold_start(app_id), bad_policy)
                .expect_err("rust validator must refuse unknown classification");
            match err {
                zeroship_data_orm::error::DbError::ValidationFailed { code, .. } => {
                    assert_eq!(code, "invalid_mask_classification");
                }
                other => {
                    panic!("expected ValidationFailed::invalid_mask_classification, got {other:?}")
                }
            }
        });
    })
}

/// The app's startup declaration stays fixed while the database is in use.
#[test]
fn policy_cannot_change_after_startup() {
    Host::test(|host| {
        let app_id = "app_unmask_policy_fixed";
        let collection = "items";
        host.run(async {
            let (backend, dir) = policy_setup(host, app_id, collection, zeroship_data_sql::value!({
            "id": { "type": "string" },
            "data": { "type": "string", "mask": { "kind": "full", "classification": "internal" } },
        })).await;
            let binding = DbBinding::cold_start(app_id);
            mask_policy::install_mask_policy(
                &binding,
                zeroship_data_sql::value!({ "support": ["public"] }),
            )
            .unwrap();
            let args = unmask::UnmaskFieldArgs {
                collection: collection.to_string(),
                row_pk: "any".to_string(),
                column: "data".to_string(),
                actor: Some(zeroship_data_sql::value!({ "kind": "support", "id": "sup_1" })),
                reason: None,
                rejected_claim: None,
            };
            for attempt in 0..2 {
                if attempt > 0 {
                    let error = mask_policy::install_mask_policy(
                        &binding,
                        zeroship_data_sql::value!({ "support": ["internal"] }),
                    )
                    .unwrap_err();
                    assert!(matches!(
                        error,
                        zeroship_data_orm::error::DbError::ValidationFailed {
                            code: "mask_policy_immutable",
                            ..
                        }
                    ));
                }
                let error = unmask::dispatch_unmask(
                    &unmask_route(host, app_id).await,
                    &binding,
                    args.clone(),
                )
                .await
                .unwrap_err();
                assert!(matches!(
                    error,
                    zeroship_data_orm::error::DbError::Coded {
                        code, ..
                    } if code == "unmask_not_permitted"
                ));
            }
            let audit = read_audit_rows(backend.as_ref(), app_id).await;
            assert_eq!(audit.len(), 2);
            assert!(audit.iter().all(|row| row.0 == "denied"));
            assert!(!dir.path().join("mask_policies.json").exists());
        });
    })
}

/// Existing sidecar contents cannot grant access or break authorization.
#[test]
fn unmask_ignores_policy_sidecar_files() {
    Host::test(|host| {
        host.run(async {
            for (app_id, contents) in [
                (
                    "app_sidecar_grant",
                    r#"{"app_sidecar_grant":{"support":["internal"]}}"#,
                ),
                ("app_sidecar_corrupt", "invalid JSON"),
            ] {
                let (backend, dir) = policy_setup(host, app_id, "items", zeroship_data_sql::value!({
                "id": { "type": "string" },
                "data": { "type": "string", "mask": { "kind": "full", "classification": "internal" } },
            })).await;
                let path = dir.path().join("mask_policies.json");
                std::fs::write(&path, contents).unwrap();
                let error = unmask::dispatch_unmask(
                    &unmask_route(host, app_id).await,
                    &DbBinding::cold_start(app_id),
                    unmask::UnmaskFieldArgs {
                        collection: "items".into(),
                        row_pk: "any".into(),
                        column: "data".into(),
                        actor: Some(
                            zeroship_data_sql::value!({ "kind": "support", "id": "sup_1" }),
                        ),
                        reason: None,
                        rejected_claim: None,
                    },
                )
                .await
                .unwrap_err();
                assert!(matches!(
                    error,
                    zeroship_data_orm::error::DbError::Coded {
                        code, ..
                    } if code == "unmask_not_permitted"
                ));
                assert_eq!(std::fs::read_to_string(path).unwrap(), contents);
                let audit = read_audit_rows(backend.as_ref(), app_id).await;
                assert_eq!(audit.len(), 1);
                assert_eq!(audit[0].0, "denied");
            }
        });
    })
}

/// **Malformed sentinel does not poison introspection** on
/// SQLite: a sibling carrying a garbled sentinel parses to "no mask"
/// on the parent (and a `tracing::warn!` fires; the test only checks
/// the introspection shape).
#[test]
fn malformed_mask_sentinel_skipped_on_sqlite() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("app_demo")
                .await
                .expect("ensure_app_schema");
            backend
            .execute_fixture(
                "CREATE TABLE \"app_demo\".\"users\" (\
                     \"id\" INTEGER PRIMARY KEY, \
                     \"ssn\" TEXT, \
                     \"ssn_masked\" TEXT NOT NULL /* zero-migrate:mask:kind=cosmic_radiation,classification=spi */\
                 )",
                &[],
            )
            .await
            .expect("CREATE garbled");
            let live = backend
                .introspect_schema("app_demo")
                .await
                .expect("introspect garbled");
            let parent = live
                .tables
                .get("users")
                .and_then(|t| t.get("ssn"))
                .expect("ssn col");
            assert!(
                parent.mask.is_none(),
                "malformed sentinel must leave parent unmasked: {:?}",
                parent.mask
            );
        });
    })
}

/// **cold-open gate, bulk unmask**: the same guard as the single-unmask
/// cold-open gate, after a fresh startup reinstalls the app policy in memory.
/// The database must open from the configured URL.
///
/// The production line is `v8_classes::dispatch::dispatch_bulk_unmask_field`
/// (and `v8_classes::masked_value`'s bulk site), which resolves through
/// `tx_scope::ensure_backend` exactly as the single-unmask dispatch does.
#[test]
fn cold_bulk_unmask_open_comes_from_ensure_backend_not_the_fixture() {
    Host::test(|host| {
        let schema = zeroship_data_sql::value!({
            "id":    { "type": "string" },
            "email": {
                "type": "string",
                "mask": { "kind": "email", "classification": "pii" },
            },
        });
        let app_id = "app_bulk_unmask_cold_open";
        let collection = "users";

        host.run(async {
            // `fixture` stays bound for the whole block: the assertion is an
            // address comparison against it.
            let (fixture, dir) =
                unmask_setup_with_schema(host, app_id, collection, schema.clone()).await;
            host.clear_mask_policy_cache(app_id);
            mask_policy::install_mask_policy(
                &DbBinding::cold_start(app_id),
                zeroship_data_sql::value!({ "user": ["pii"] }),
            )
            .expect("set_mask_policy");
            configure_cold_sqlite_unmask_fixture(
                host,
                &dir,
                app_id,
                collection,
                schema,
                zeroship_data_sql::value!({ "user": ["pii"] }),
            );
            assert_cold_open_installs_a_fresh_backend(host, fixture.as_ref()).await;
        });
    })
}

/// **bulk gate #1**: authorised actor unmasks many columns
/// across many rows in one call; the result map carries plaintext
/// for every pair, and exactly ONE audit row lands.
///
/// The ATTACH is what this rules on. The OPEN is the harness's - see
/// `unmask_backend` - and is bound by the cold-open gate directly above.
#[test]
fn cold_bulk_unmask_attaches_before_read() {
    Host::test(|host| {
        let schema = zeroship_data_sql::value!({
            "id":    { "type": "string" },
            "email": {
                "type": "string",
                "mask": { "kind": "email", "classification": "pii" }
            },
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            },
        });
        let app_id = "app_bulk_unmask_e2e";
        let collection = "users";

        host.run(async {
            let (backend, dir) =
                unmask_setup_with_schema(host, app_id, collection, schema.clone()).await;
            host.clear_mask_policy_cache(app_id);
            // Post-storage-flip layout: each field's own column holds the
            // mask; the raw sibling (named via `raw_column_name`, never
            // spelled out here) holds the real value `dispatch_bulk_unmask`
            // reads.
            let raw_email = raw_column_name("email");
            let raw_ssn = raw_column_name("ssn");
            backend
                .execute_fixture(
                    &format!(
                        "CREATE TABLE \"app_bulk_unmask_e2e\".\"users\" (\
                         id             TEXT PRIMARY KEY, \
                         \"{raw_email}\" TEXT, \
                         email          TEXT NOT NULL, \
                         \"{raw_ssn}\"   TEXT, \
                         ssn            TEXT NOT NULL\
                     )"
                    ),
                    &[],
                )
                .await
                .expect("CREATE TABLE");
            for (id, email, ssn) in [
                ("u1", "alice@example.com", "123-45-6789"),
                ("u2", "bob@example.com", "987-65-4321"),
            ] {
                let sql = format!(
                    "INSERT INTO \"app_bulk_unmask_e2e\".\"users\" \
                 (id, \"{raw_email}\", email, \"{raw_ssn}\", ssn) VALUES \
                 ('{id}', '{email}', 'masked', '{ssn}', 'masked')"
                );
                backend.execute_fixture(&sql, &[]).await.expect("INSERT");
            }

            // Policy: `user` can unmask pii AND spi.
            let policy_v = zeroship_data_sql::value!({ "user": ["pii", "spi"] });
            mask_policy::install_mask_policy(&DbBinding::cold_start(app_id), policy_v.clone())
                .expect("set_mask_policy");

            // A fresh startup reinstalls the app declaration without a sidecar.
            // Bulk dispatch then opens the backend and attaches the app database.
            assert!(!dir.path().join("mask_policies.json").exists());
            configure_cold_sqlite_unmask_fixture(host, &dir, app_id, collection, schema, policy_v);

            let args = BulkUnmaskArgs {
                collection: collection.to_string(),
                items: vec![
                    BulkUnmaskItem {
                        row_pk: "u1".into(),
                        columns: vec!["email".into(), "ssn".into()],
                    },
                    BulkUnmaskItem {
                        row_pk: "u2".into(),
                        columns: vec!["email".into()],
                    },
                ],
                actor: Some(zeroship_data_sql::value!({ "kind": "user", "id": "actor_x" })),
                reason: Some("ops dashboard".into()),
                rejected_claim: None,
            };
            let result = dispatch_bulk_unmask(
                &unmask_route(host, app_id).await,
                &DbBinding::cold_start(app_id),
                args,
            )
            .await
            .expect("bulk unmask");
            // Plaintext recovered for every pair.
            let u1 = result.results.get("u1").expect("u1 row");
            assert_eq!(
                u1.get("email")
                    .and_then(zeroship_data_sql::value::Value::as_str),
                Some("alice@example.com")
            );
            assert_eq!(
                u1.get("ssn")
                    .and_then(zeroship_data_sql::value::Value::as_str),
                Some("123-45-6789")
            );
            let u2 = result.results.get("u2").expect("u2 row");
            assert_eq!(
                u2.get("email")
                    .and_then(zeroship_data_sql::value::Value::as_str),
                Some("bob@example.com")
            );

            // Exactly ONE audit row covering the whole call.
            let audit = read_audit_rows(backend.as_ref(), app_id).await;
            assert_eq!(audit.len(), 1, "bulk → single audit row: {audit:?}");
            assert_eq!(audit[0].0, "granted");
            assert_eq!(audit[0].1, "user", "actor_role recorded");

            // A direct read of the fields' own columns (what a default read
            // pipeline would see) must still be the mask placeholder, never
            // the plaintext bulk_unmask returned above.
            let client = backend
                .fixture_session(app_id)
                .await
                .expect("acquire client");
            let direct = client
                .query(
                    "SELECT email, ssn FROM \"app_bulk_unmask_e2e\".\"users\" WHERE id = 'u1'",
                    &[],
                )
                .await
                .expect("direct SELECT of the fields' own columns");
            assert_eq!(direct[0][0].as_deref(), Some("masked"));
            assert_eq!(direct[0][1].as_deref(), Some("masked"));
            assert_ne!(direct[0][0].as_deref(), Some("alice@example.com"));
            assert_ne!(direct[0][1].as_deref(), Some("123-45-6789"));
        });
    })
}

/// **bulk gate #2**: ANY unauthorised pair refuses the WHOLE
/// call (Q-MASK-F atomic). One audit row with outcome `denied`; no
/// plaintext returned for the authorised pair either.
#[test]
fn bulk_unmask_authorization_atomic_one_unauthorized_fails_all() {
    Host::test(|host| {
        let schema = zeroship_data_sql::value!({
            "id":    { "type": "string" },
            "email": {
                "type": "string",
                "mask": { "kind": "email", "classification": "pii" }
            },
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            },
        });
        let app_id = "app_bulk_atomic_refuse";
        let collection = "users";

        host.run(async {
            let (backend, _dir) = unmask_setup_with_schema(host, app_id, collection, schema).await;
            host.clear_mask_policy_cache(app_id);
            backend
                .execute_fixture(
                    "CREATE TABLE \"app_bulk_atomic_refuse\".\"users\" (\
                     id              TEXT PRIMARY KEY, \
                     __zs_raw__email TEXT, \
                     email           TEXT NOT NULL, \
                     __zs_raw__ssn   TEXT, \
                     ssn             TEXT NOT NULL\
                 )",
                    &[],
                )
                .await
                .expect("CREATE TABLE");

            // Policy: `user` can ONLY unmask pii; spi is forbidden.
            let policy_v = zeroship_data_sql::value!({ "user": ["pii"] });
            mask_policy::install_mask_policy(&DbBinding::cold_start(app_id), policy_v)
                .expect("set_mask_policy");

            let args = BulkUnmaskArgs {
                collection: collection.to_string(),
                // Pair (u1, email) authorised; pair (u1, ssn) NOT
                // authorised. Atomic fence: entire call refuses.
                items: vec![BulkUnmaskItem {
                    row_pk: "u1".into(),
                    columns: vec!["email".into(), "ssn".into()],
                }],
                actor: Some(zeroship_data_sql::value!({ "kind": "user", "id": "actor_x" })),
                reason: None,
                rejected_claim: None,
            };
            let err = dispatch_bulk_unmask(
                &unmask_route(host, app_id).await,
                &DbBinding::cold_start(app_id),
                args,
            )
            .await
            .expect_err("bulk must refuse atomically");
            match err {
                zeroship_data_orm::error::DbError::Coded { code, .. } => {
                    assert_eq!(code, "bulk_unmask_partial_unauthorized");
                }
                other => panic!("expected Coded::bulk_unmask_partial_unauthorized, got {other:?}"),
            }

            // Single `denied` audit row covers the whole call.
            let audit = read_audit_rows(backend.as_ref(), app_id).await;
            assert_eq!(
                audit.len(),
                1,
                "atomic refuse -> single audit row: {audit:?}"
            );
            assert_eq!(audit[0].0, "denied");
        });
    })
}

/// **bulk gate #3**: unknown column on the schema raises the
/// typed `unmask_column_not_masked` error BEFORE any audit row writes.
#[test]
fn bulk_unmask_unknown_column_returns_typed_error_e2e() {
    Host::test(|host| {
        let schema = zeroship_data_sql::value!({
            "id":  { "type": "string" },
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            },
        });
        let app_id = "app_bulk_unknown_column";
        let collection = "users";

        host.run(async {
            let (_backend, _dir) = unmask_setup_with_schema(host, app_id, collection, schema).await;
            host.clear_mask_policy_cache(app_id);
            let args = BulkUnmaskArgs {
                collection: collection.to_string(),
                items: vec![BulkUnmaskItem {
                    row_pk: "u1".into(),
                    columns: vec!["does_not_exist".into()],
                }],
                actor: Some(zeroship_data_sql::value!({ "kind": "auto" })),
                reason: None,
                rejected_claim: None,
            };
            let err = dispatch_bulk_unmask(
                &unmask_route(host, app_id).await,
                &DbBinding::cold_start(app_id),
                args,
            )
            .await
            .expect_err("unknown column must refuse");
            match err {
                zeroship_data_orm::error::DbError::ValidationFailed { code, .. } => {
                    assert_eq!(code, "unmask_column_not_masked");
                }
                other => {
                    panic!("expected ValidationFailed::unmask_column_not_masked, got {other:?}")
                }
            }
        });
    })
}

/// **cold-open gate, query hint**: the same guard again, over this family's
/// fixture. The query hint's first cold operation is the authorization fence,
/// so the open has to happen before any policy load or denied audit write - and
/// this is what rules on the open.
///
/// The production line is `crud::mod`'s query hint, sanitised in `plan_find` and
/// resolved by the `find` dispatch through `tx_scope::ensure_backend`.
#[test]
fn cold_query_unmask_hint_open_comes_from_ensure_backend_not_the_fixture() {
    Host::test(|host| {
        let schema = zeroship_data_sql::value!({
            "id":  { "type": "string" },
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" },
            },
        });
        let app_id = "app_qhint_cold_open";
        let collection = "users";

        host.run(async {
            // `fixture` stays bound for the whole block: the assertion is an
            // address comparison against it.
            let (fixture, dir) =
                unmask_setup_with_schema(host, app_id, collection, schema.clone()).await;
            host.clear_mask_policy_cache(app_id);
            mask_policy::install_mask_policy(
                &DbBinding::cold_start(app_id),
                zeroship_data_sql::value!({ "user": ["spi"] }),
            )
            .expect("set_mask_policy");
            configure_cold_sqlite_unmask_fixture(
                host,
                &dir,
                app_id,
                collection,
                schema,
                zeroship_data_sql::value!({ "user": ["spi"] }),
            );
            assert_cold_open_installs_a_fresh_backend(host, fixture.as_ref()).await;
        });
    })
}

/// **per-query gate #1**: an authorised actor with a query
/// hint sees plaintext in the listed columns; non-listed masked
/// columns keep their `__zsmask__` wrapping.
///
/// The ATTACH is what this rules on. The OPEN is the harness's - see
/// `unmask_backend` - and is bound by the cold-open gate directly above.
#[test]
fn cold_query_unmask_hint_attaches_before_read() {
    Host::test(|host| {
        let schema = zeroship_data_sql::value!({
            "id":    { "type": "string" },
            "email": {
                "type": "string",
                "mask": { "kind": "email", "classification": "pii" }
            },
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            },
        });
        let app_id = "app_qhint_e2e";
        let collection = "users";

        host.run(async {
            let (backend, dir) =
                unmask_setup_with_schema(host, app_id, collection, schema.clone()).await;
            host.clear_mask_policy_cache(app_id);
            // Post-storage-flip layout: each field's own column holds the
            // mask; the raw sibling (named via `raw_column_name`, never
            // spelled out here) holds the real value `dispatch_unmask_for_query`
            // reads.
            let raw_email = raw_column_name("email");
            let raw_ssn = raw_column_name("ssn");
            backend
                .execute_fixture(
                    &format!(
                        "CREATE TABLE \"app_qhint_e2e\".\"users\" (\
                         id             TEXT PRIMARY KEY, \
                         \"{raw_email}\" TEXT, \
                         email          TEXT NOT NULL, \
                         \"{raw_ssn}\"   TEXT, \
                         ssn            TEXT NOT NULL\
                     )"
                    ),
                    &[],
                )
                .await
                .expect("CREATE TABLE");
            backend
                .execute_fixture(
                    &format!(
                        "INSERT INTO \"app_qhint_e2e\".\"users\" \
                     (id, \"{raw_email}\", email, \"{raw_ssn}\", ssn) VALUES \
                     ('u1', 'alice@example.com', 'a***@example.com', '123-45-6789', '***-**-6789')"
                    ),
                    &[],
                )
                .await
                .expect("INSERT");

            // Policy: `user` can unmask both pii and spi.
            let policy_v = zeroship_data_sql::value!({ "user": ["pii", "spi"] });
            mask_policy::install_mask_policy(&DbBinding::cold_start(app_id), policy_v.clone())
                .expect("set_mask_policy");

            // A fresh startup reinstalls the declaration before the first query.
            // Query authorization must attach the app database before reading it.
            assert!(!dir.path().join("mask_policies.json").exists());
            configure_cold_sqlite_unmask_fixture(host, &dir, app_id, collection, schema, policy_v);

            // Simulate the row shape `dispatch_find` would produce
            // AFTER `apply_mask_wrap_on_read` has wrapped the masked
            // columns. We're driving `dispatch_unmask_for_query` directly
            // since the full V8 round-trip is out of scope for this
            // integration test.
            let actor = Some(zeroship_data_sql::value!({ "kind": "user", "id": "actor_x" }));
            let reason = Some("dashboard view".to_string());

            // Step 1 — upfront auth fence.
            authorize_query_hint(
                &unmask_backend(host).await,
                &DbBinding::cold_start(app_id),
                collection,
                &["ssn".to_string()],
                &actor,
                None,
                &reason,
            )
            .await
            .expect("authorize_query_hint must succeed");

            // Step 2 — simulate post-wrap row + run unmask-for-query.
            let mut rows = vec![zeroship_data_sql::value!({
                "id": "u1",
                "email": {
                    "sentinel": "__zsmask__",
                    "masked": "a***@example.com",
                    "classification": "pii",
                    "_meta": { "collection": "users", "row_pk": "u1", "column": "email" },
                },
                "ssn": {
                    "sentinel": "__zsmask__",
                    "masked": "***-**-6789",
                    "classification": "spi",
                    "_meta": { "collection": "users", "row_pk": "u1", "column": "ssn" },
                },
            })];
            dispatch_unmask_for_query(
                &unmask_route(host, app_id).await,
                &DbBinding::cold_start(app_id),
                collection,
                &["ssn".to_string()],
                &mut rows,
            )
            .await
            .expect("dispatch_unmask_for_query");

            // `ssn` slot now carries plaintext; `email` slot keeps the
            // sentinel-wrapped form.
            let row = &rows[0];
            assert_eq!(
                row.get("ssn").and_then(|v| v.as_str()),
                Some("123-45-6789"),
                "ssn must be plaintext: {row:?}"
            );
            let email = row
                .get("email")
                .and_then(|v| v.as_object())
                .expect("email obj");
            assert_eq!(
                email.get("sentinel").and_then(|v| v.as_str()),
                Some("__zsmask__"),
                "email must remain wrapped: {row:?}"
            );

            // Step 3 — granted audit row lands.
            audit_query_hint_granted(
                &unmask_backend(host).await,
                &DbBinding::cold_start(app_id),
                collection,
                &["ssn".to_string()],
                &actor,
                None,
                &reason,
            )
            .await
            .expect("audit");
            let audit = read_audit_rows(backend.as_ref(), app_id).await;
            assert_eq!(audit.len(), 1, "one audit row for the query: {audit:?}");
            assert_eq!(audit[0].0, "granted");
            assert_eq!(audit[0].1, "user");

            // `dispatch_unmask_for_query` mutates only the in-memory `rows`
            // passed above - a direct read of the fields' own columns must
            // still show the mask, never the plaintext it just returned.
            let client = backend
                .fixture_session(app_id)
                .await
                .expect("acquire client");
            let direct = client
                .query(
                    "SELECT email, ssn FROM \"app_qhint_e2e\".\"users\" WHERE id = 'u1'",
                    &[],
                )
                .await
                .expect("direct SELECT of the fields' own columns");
            assert_eq!(direct[0][0].as_deref(), Some("a***@example.com"));
            assert_eq!(direct[0][1].as_deref(), Some("***-**-6789"));
            assert_ne!(direct[0][0].as_deref(), Some("alice@example.com"));
            assert_ne!(direct[0][1].as_deref(), Some("123-45-6789"));
        });
    })
}

/// **per-query gate #2**: an unauthorised actor REFUSES the
/// query entirely; we do not silently degrade to masked-only.
#[test]
fn per_query_unmask_hint_rejects_unauthorized_actor() {
    Host::test(|host| {
        let schema = zeroship_data_sql::value!({
            "id":  { "type": "string" },
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            },
        });
        let app_id = "app_qhint_refuse";
        let collection = "users";

        host.run(async {
            let (backend, _dir) = unmask_setup_with_schema(host, app_id, collection, schema).await;
            host.clear_mask_policy_cache(app_id);
            // Policy: `user` can only unmask `pii`, NOT `spi`.
            let policy_v = zeroship_data_sql::value!({ "user": ["pii"] });
            mask_policy::install_mask_policy(&DbBinding::cold_start(app_id), policy_v)
                .expect("set_mask_policy");

            let actor = Some(zeroship_data_sql::value!({ "kind": "user", "id": "actor_x" }));
            let err = authorize_query_hint(
                &unmask_backend(host).await,
                &DbBinding::cold_start(app_id),
                collection,
                &["ssn".to_string()],
                &actor,
                None,
                &None,
            )
            .await
            .expect_err("must refuse");
            match err {
                zeroship_data_orm::error::DbError::Coded { code, .. } => {
                    assert_eq!(code, "unmask_not_permitted");
                }
                other => panic!("expected Coded::unmask_not_permitted, got {other:?}"),
            }
            // The denied path wrote one audit row (`denied` outcome) so
            // operators see the attempt; assert it landed.
            let audit = read_audit_rows(backend.as_ref(), app_id).await;
            assert_eq!(audit.len(), 1, "denied path must audit: {audit:?}");
            assert_eq!(audit[0].0, "denied");
        });
    })
}

/// **per-query gate #3**: unknown column on the schema raises
/// the typed `unmask_column_not_masked` error before any DB hit.
#[test]
fn per_query_unmask_hint_unknown_column_returns_typed_error() {
    Host::test(|host| {
        let schema = zeroship_data_sql::value!({
            "id":  { "type": "string" },
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            },
        });
        let app_id = "app_qhint_unknown";
        let collection = "users";

        host.run(async {
            let (_backend, _dir) = unmask_setup_with_schema(host, app_id, collection, schema).await;
            host.clear_mask_policy_cache(app_id);
            let actor = Some(zeroship_data_sql::value!({ "kind": "auto" }));
            let err = authorize_query_hint(
                &unmask_backend(host).await,
                &DbBinding::cold_start(app_id),
                collection,
                &["does_not_exist".to_string()],
                &actor,
                None,
                &None,
            )
            .await
            .expect_err("must refuse on unknown");
            match err {
                zeroship_data_orm::error::DbError::ValidationFailed { code, .. } => {
                    assert_eq!(code, "unmask_column_not_masked");
                }
                other => {
                    panic!("expected ValidationFailed::unmask_column_not_masked, got {other:?}")
                }
            }
        });
    })
}
