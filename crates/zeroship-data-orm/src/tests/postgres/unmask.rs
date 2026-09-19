//! PostgreSQL unmask contracts.
use super::fixtures::*;

use crate::tests::fixtures::Host;
use crate::tests::fixtures::schema::fixture_table_sql;

use crate::tests::fixtures::{self};

use zeroship_migrate::schema::query::FkEmission;

use compio_postgres::Pool;


use crate::value::value;

use crate::sql::mapping::*;

use zeroship_data_orm::encryption;

/// Resolve the backend at the same boundary as the V8 dispatcher.
async fn unmask_backend(host: &Host) -> zeroship_data_orm::backend::BackendHandle {
    host.backend()
        .await
        .expect("the backend the V8 dispatcher would have opened")
}

/// The route the unmask dispatchers now take, in place of a bare handle.
///
/// See the twin in `mask_flip.rs` for why. No fixture that reaches it here
/// parks a transaction, so every call binds `in_tx = false` and takes the lane
/// it took before.
async fn unmask_route(host: &Host, app: &str) -> zeroship_data_orm::tx_route::TxRoute {
    zeroship_data_orm::exec::ambient_route_for_tests(&crate::tests::fixtures::harness_binding(app), unmask_backend(host).await)
}

#[test]
fn unmask_fetch_runs_under_per_app_role_via_rls() {
    Host::test(|host| {
        host.run(async {
            use zeroship_data_orm::protection::unmask::{self, UnmaskFieldArgs};

            let (_postgres, url) = require_pg(host).await;
            let admin_pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

            let app = crate::tests::fixtures::test_app_id!();

            let app = app.as_str();
            let coll = "users";
            let role = provision_app_with_role(&admin_pool, app).await;
            crate::tests::fixtures::roles::ensure_binding_ladder(&admin_pool, &crate::tests::fixtures::harness_binding(app))
                .await
                .unwrap();
            let schema = value!({
                "ssn": {
                    "type": "string",
                    "mask": { "kind": "last4", "classification": "spi" }
                }
            });
            let ssn_raw = raw_column_name("ssn");
            // Built with the platform's own emitter, not hand-spelled, so the fixture
            // cannot drift from the runtime's DDL shape: `ssn` gets the bare-TEXT mask
            // column and `__zs_raw__ssn` gets the declared type for the real value.
            let create_table = fixture_table_sql(
                &crate::sql::SchemaName::new(app).expect("fixture schema name"),
                coll,
                &schema,
                &FkEmission::Inline,
            )
            .expect("emitter must build the users DDL");
            admin_pool.batch_execute(&create_table).await.unwrap();
            admin_pool
                .execute(
                    &format!(
                        "INSERT INTO \"{app}\".\"{coll}\" (id, \"{ssn_raw}\", ssn) \
             VALUES ('u1', '123-45-6789', '***-**-6789')"
                    ),
                    &[],
                )
                .await
                .unwrap();
            fixtures::grant_runtime_select_columns(&admin_pool, &crate::tests::fixtures::harness_binding(app), coll, &["id", &ssn_raw]).await;
            install_role_bound_select_policy(&admin_pool, app, coll, &role).await;
            let login_role = "p6a_unmask_login";
            let (login_url, login_pool) =
                provision_platform_login_pool(&admin_pool, &url, login_role, "test", &role, app)
                    .await;

            // The SENSITIVE value now lives in the raw sibling column (the storage
            // flip), so that is the column this proof must show is unreachable by
            // direct SQL before `dispatch_unmask` narrows to the per-app role.
            let blocked = login_pool
                .query_text_params(
                    &format!("SELECT \"{ssn_raw}\" FROM \"{app}\".\"{coll}\" WHERE id = 'u1'"),
                    &[],
                )
                .await
                .unwrap();
            assert!(
                blocked.is_empty(),
                "login role must be blocked by FORCE RLS from the raw column before unmask proves \
         the role fence"
            );

            host.install_postgres_pool(login_pool.clone(), &login_url);
            crate::tests::fixtures::cache_schema(app, coll, schema);
            host.clear_mask_policy_cache(app);

            let result = unmask::dispatch_unmask(
                &unmask_route(host, app).await,
                &crate::tests::fixtures::harness_binding(app),
                UnmaskFieldArgs {
                    collection: coll.to_string(),
                    row_pk: "u1".to_string(),
                    column: "ssn".to_string(),
                    actor: Some(value!({ "kind": "auto" })),
                    reason: Some("security regression".to_string()),
                    rejected_claim: None,
                },
            )
            .await
            .expect("unmask must read under the per-app role");
            assert_eq!(result.plaintext, "123-45-6789");

            drop(login_pool);
            let _ = admin_pool
                .execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
                .await;
            let _ = admin_pool
                .execute(&format!("DROP ROLE IF EXISTS \"{login_role}\""), &[])
                .await;
            let _ = admin_pool
                .execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[])
                .await;
            release_pg(host, admin_pool).await;
        })
    })
}

/// An encrypted masked column must preserve PostgreSQL's binary storage through
/// the unmask pipeline and return its decrypted value.
#[test]
fn unmask_encrypted_column_on_pg_reads_bytea_raw_sibling() {
    Host::test(|host| {
        host.run(async {
            use zeroship_data_orm::protection::unmask::{self, UnmaskFieldArgs};

            let (_postgres, url) = require_pg(host).await;
            let admin_pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());
            // Synthetic 32-byte root key, same shape as the encrypted round-trip gate.

            let app = crate::tests::fixtures::test_app_id!();

            let app = app.as_str();
            let _keys = host.supply_project_key(&[app], &"b".repeat(64));
            let coll = "users";
            let role = provision_app_with_role(&admin_pool, app).await;
            crate::tests::fixtures::roles::ensure_binding_ladder(&admin_pool, &crate::tests::fixtures::harness_binding(app))
                .await
                .unwrap();
            let schema = value!({
                "ssn": {
                    "type": "string",
                    "mask": { "kind": "last4", "classification": "spi" },
                    "encrypted": true
                }
            });
            let ssn_raw = raw_column_name("ssn");
            // The emitter decides the raw sibling's type. For an encrypted column that
            // is BYTEA, which is the whole point of this test - so build the DDL rather
            // than hand-spelling it, or the fixture proves nothing about the runtime.
            let create_table = fixture_table_sql(
                &crate::sql::SchemaName::new(app).expect("fixture schema name"),
                coll,
                &schema,
                &FkEmission::Inline,
            )
            .expect("emitter must build the users DDL");
            admin_pool.batch_execute(&create_table).await.unwrap();

            // Real ciphertext from the platform's own encryptor, under the AAD the read
            // path recomputes: canonical_aad(collection, column, row_pk).
            let backend = zeroship_data_orm::backend::PostgresBackend::new(
                admin_pool.clone(),
                url.clone(),
                host.key_source(),
            );
            let key = backend.key_store().resolve(app).await.expect("resolve_key");
            let aad = encryption::canonical_aad(app, coll, "ssn", b"u1");
            let ct = zeroship_data_orm::encryption::aead::encrypt(&key, b"123-45-6789", &aad)
                .expect("encrypt");
            let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &ct);
            admin_pool
                .execute(
                    &format!(
                        "INSERT INTO \"{app}\".\"{coll}\" (id, \"{ssn_raw}\", ssn) \
                 VALUES ('u1', decode($1, 'base64')::bytea, '***-**-6789')"
                    ),
                    &[&b64.as_str()],
                )
                .await
                .unwrap();
            fixtures::grant_runtime_select_columns(&admin_pool, &crate::tests::fixtures::harness_binding(app), coll, &["id", &ssn_raw]).await;
            install_role_bound_select_policy(&admin_pool, app, coll, &role).await;
            let login_role = "p6a_unmask_enc_login";
            let (login_url, login_pool) =
                provision_platform_login_pool(&admin_pool, &url, login_role, "test", &role, app)
                    .await;

            host.install_postgres_pool(login_pool.clone(), &login_url);
            crate::tests::fixtures::cache_schema(app, coll, schema);
            host.clear_mask_policy_cache(app);

            let result = unmask::dispatch_unmask(
                &unmask_route(host, app).await,
                &crate::tests::fixtures::harness_binding(app),
                UnmaskFieldArgs {
                    collection: coll.to_string(),
                    row_pk: "u1".to_string(),
                    column: "ssn".to_string(),
                    actor: Some(value!({ "kind": "auto" })),
                    reason: Some("encrypted unmask regression".to_string()),
                    rejected_claim: None,
                },
            )
            .await;

            // Surface the real error rather than a bare unwrap panic: on the pre-fix
            // code this printed `error deserializing column 0`, which is the evidence
            // that the failure is the BYTEA decode and nothing else.
            let unmasked = result.unwrap_or_else(|e| {
                panic!("unmask of an ENCRYPTED column must recover the plaintext, got: {e:?}")
            });
            assert_eq!(unmasked.plaintext, "123-45-6789");

            drop(login_pool);
            let _ = admin_pool
                .execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
                .await;
            let _ = admin_pool
                .execute(&format!("DROP ROLE IF EXISTS \"{login_role}\""), &[])
                .await;
            let _ = admin_pool
                .execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[])
                .await;
            release_pg(host, admin_pool).await;
        })
    })
}

/// EVERY statement `dispatch_unmask` issues must go through `SET LOCAL ROLE`,
/// including the audit INSERT.
///
/// WHY THE SIBLING ABOVE DOES NOT COVER THIS.
/// `unmask_fetch_runs_under_per_app_role_via_rls` blocks the login role with
/// FORCE RLS on the DATA table only, and its login role holds an INHERITING
/// membership plus direct `USAGE`/`SELECT` grants. The audit table carries no
/// RLS, so `write_audit_unmask_row`'s INSERT succeeded there through ambient
/// inheritance whether or not it was fenced - it passed identically before and
/// after this fix, which is the one shape a regression guard must not have.
///
/// THE FIXTURE IS PRODUCTION'S POSTURE, not an RLS stand-in for it. The login
/// role is granted the app role `WITH INHERIT FALSE` - what
/// `zeroship-migrate-server`'s `runtime_dependents_sql` now emits - and NOTHING
/// directly. Under that grant a statement that omits `SET LOCAL ROLE` has no
/// privilege at all, so this case binds the whole dispatch rather than one
/// table: fetch, decrypt-or-plaintext, and audit all have to narrow or the
/// call fails.
///
#[test]
fn unmask_audit_insert_runs_under_the_per_app_role_not_the_login_role() {
    Host::test(|host| {
        host.run(async {
            use zeroship_data_orm::protection::unmask::{self, UnmaskFieldArgs};

            let (_postgres, url) = require_pg(host).await;
            let admin_pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

            let app = crate::tests::fixtures::test_app_id!();

            let app = app.as_str();
            let coll = "patients";
            let role = provision_app_with_role(&admin_pool, app).await;
            crate::tests::fixtures::roles::ensure_binding_ladder(&admin_pool, &crate::tests::fixtures::harness_binding(app))
                .await
                .unwrap();
            let schema = value!({
                "ssn": {
                    "type": "string",
                    "mask": { "kind": "last4", "classification": "phi" }
                }
            });
            let ssn_raw = raw_column_name("ssn");
            // Built with the platform's own emitter, not hand-spelled, so the fixture
            // cannot drift from the runtime's DDL shape: `ssn` gets the bare-TEXT mask
            // column and `__zs_raw__ssn` gets the declared type for the real value.
            let create_table = fixture_table_sql(
                &crate::sql::SchemaName::new(app).expect("fixture schema name"),
                coll,
                &schema,
                &FkEmission::Inline,
            )
            .expect("emitter must build the patients DDL");
            admin_pool.batch_execute(&create_table).await.unwrap();
            admin_pool
                .execute(
                    &format!(
                        "INSERT INTO \"{app}\".\"{coll}\" (id, \"{ssn_raw}\", ssn) \
                 VALUES ('p1', '555-44-3333', '***-**-3333')"
                    ),
                    &[],
                )
                .await
                .unwrap();
            fixtures::grant_runtime_select_columns(&admin_pool, &crate::tests::fixtures::harness_binding(app), coll, &["id", &ssn_raw]).await;

            let login_role = "p6a_unmask_audit_login";
            let _ = admin_pool
                .execute(&format!("DROP ROLE IF EXISTS \"{login_role}\""), &[])
                .await;
            admin_pool
                .execute(
                    &format!("CREATE ROLE \"{login_role}\" LOGIN PASSWORD 'test' INHERIT"),
                    &[],
                )
                .await
                .unwrap();
            // The production grant. `INHERIT` on the role above is deliberate and is
            // the point: the ROLE ATTRIBUTE says inherit, the MEMBERSHIP says do not,
            // and PostgreSQL 16+ honours the membership - so this fixture also pins
            // that the attribute is not what fences anything.
            admin_pool
                .execute(
                    &format!("GRANT \"{role}\" TO \"{login_role}\" WITH INHERIT FALSE"),
                    &[],
                )
                .await
                .unwrap();

            let login_url = login_role_test_url(&url, login_role, "test");
            let login_pool = std::rc::Rc::new(Pool::connect(&login_url, 4).await.unwrap());

            // THE CONTROL. Without this the case would pass just as happily if the app
            // role had never been granted anything: "denied" is the resting state of a
            // role with no privileges. This proves the login role is genuinely fenced
            // out, so the success below can only come from narrowing.
            let ambient = login_pool
                .query_text_params(
                    &format!("SELECT ssn FROM \"{app}\".\"{coll}\" WHERE id = 'p1'"),
                    &[],
                )
                .await;
            assert!(
                ambient.is_err(),
                "the login role must reach nothing ambiently under WITH INHERIT FALSE"
            );

            host.install_postgres_pool(login_pool.clone(), &login_url);
            crate::tests::fixtures::cache_schema(app, coll, schema);
            host.clear_mask_policy_cache(app);

            let result = unmask::dispatch_unmask(
                &unmask_route(host, app).await,
                &crate::tests::fixtures::harness_binding(app),
                UnmaskFieldArgs {
                    collection: coll.to_string(),
                    row_pk: "p1".to_string(),
                    column: "ssn".to_string(),
                    actor: Some(value!({ "kind": "auto" })),
                    reason: Some("audit fence regression".to_string()),
                    rejected_claim: None,
                },
            )
            .await
            .expect(
                "every statement in dispatch_unmask must narrow to the per-app role - \
         a failure here names the one that did not",
            );
            assert_eq!(result.plaintext, "555-44-3333");

            // THE AUDIT ROW MUST EXIST. `dispatch_unmask` propagates the INSERT's error
            // with `?`, so a swallowed audit write would return plaintext with no
            // record of who read it - strictly worse than refusing. Read back through
            // the ADMIN pool, which is not the one under test.
            let audited = admin_pool
                .query_text_params(
                    &format!(
                        "SELECT outcome FROM \"{app}\".\"__zeroship_audit_unmask\" \
                  WHERE collection = $1 AND row_pk = 'p1' AND \"column\" = 'ssn'"
                    ),
                    &[coll],
                )
                .await
                .unwrap();
            assert_eq!(
                audited.len(),
                1,
                "the granted unmask must have written exactly one audit row"
            );
            assert_eq!(audited[0].get::<_, &str>("outcome"), "granted");

            drop(login_pool);
            let _ = admin_pool
                .execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
                .await;
            let _ = admin_pool
                .execute(&format!("DROP ROLE IF EXISTS \"{login_role}\""), &[])
                .await;
            let _ = admin_pool
                .execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[])
                .await;
            release_pg(host, admin_pool).await;
        })
    })
}

/// The startup declaration authorizes unmasking on PostgreSQL without a
/// database policy store. An undeclared role remains denied.
#[test]
fn pg_declared_mask_policy_authorizes_unmask_without_durable_store() {
    Host::test(|host| {
        host.run(async {
            use zeroship_data_orm::protection::mask_policy;
            use zeroship_data_orm::protection::unmask::{self, UnmaskFieldArgs};

            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
            let app = crate::tests::fixtures::test_app_id!();
            let app = app.as_str();
            let coll = "patients";

            // Drops the schema and the cluster-scoped per-app role, then
            // recreates the schema. `ensure_per_app_role` below creates the role
            // the read path checks for -- without it the unmask SELECT refuses
            // with `schema_not_provisioned` before authorization is ever reached.
            let role = provision_app_with_role(&pool, app).await;
            crate::tests::fixtures::roles::ensure_binding_ladder(&pool, &crate::tests::fixtures::harness_binding(app))
                .await
                .unwrap();

            let schema = value!({
                "ssn": {
                    "type": "string",
                    "mask": { "kind": "last4", "classification": "spi" }
                }
            });
            let ssn_raw = raw_column_name("ssn");
            // Built with the platform's own emitter, not hand-spelled, so the fixture
            // cannot drift from the runtime's DDL shape: `ssn` gets the bare-TEXT mask
            // column and `__zs_raw__ssn` gets the declared type for the real value.
            let create_table = fixture_table_sql(
                &crate::sql::SchemaName::new(app).expect("fixture schema name"),
                coll,
                &schema,
                &FkEmission::Inline,
            )
            .expect("emitter must build the patients DDL");
            pool.batch_execute(&create_table).await.unwrap();
            pool.execute(
                &format!(
                    "INSERT INTO \"{app}\".\"{coll}\" (id, \"{ssn_raw}\", ssn) \
             VALUES ('u1', '123-45-6789', '***-**-6789')"
                ),
                &[],
            )
            .await
            .unwrap();
            fixtures::grant_runtime_select_columns(&pool, &crate::tests::fixtures::harness_binding(app), coll, &["id", &ssn_raw]).await;

            host.install_postgres_pool(pool.clone(), &url);
            crate::tests::fixtures::cache_schema(app, coll, schema);
            host.clear_mask_policy_cache(app);

            // Install the app-provided policy as the isolate does at boot.
            mask_policy::install_mask_policy(
                &crate::tests::fixtures::harness_binding(app),
                value!({ "support": ["spi"] }),
            )
            .expect("setMaskPolicy must install the declared policy on PG");

            // A role the declared policy grants reads through.
            let granted = unmask::dispatch_unmask(
                &unmask_route(host, app).await,
                &crate::tests::fixtures::harness_binding(app),
                UnmaskFieldArgs {
                    collection: coll.to_string(),
                    row_pk: "u1".to_string(),
                    column: "ssn".to_string(),
                    actor: Some(value!({ "kind": "support" })),
                    reason: Some("declared policy grant".to_string()),
                    rejected_claim: None,
                },
            )
            .await
            .expect("the declared policy must authorize the role it lists");
            assert_eq!(granted.plaintext, "123-45-6789");

            // A role the policy does NOT list is refused. Without this arm the
            // test would pass on an implementation that authorized everything,
            // which is exactly the failure mode a cache-only policy could hide.
            let err = unmask::dispatch_unmask(
                &unmask_route(host, app).await,
                &crate::tests::fixtures::harness_binding(app),
                UnmaskFieldArgs {
                    collection: coll.to_string(),
                    row_pk: "u1".to_string(),
                    column: "ssn".to_string(),
                    actor: Some(value!({ "kind": "intern" })),
                    reason: Some("declared policy deny".to_string()),
                    rejected_claim: None,
                },
            )
            .await
            .expect_err("a role absent from the declared policy must be refused");
            match err {
                zeroship_data_orm::error::DbError::Coded { ref code, .. } => {
                    assert_eq!(code, "unmask_not_permitted", "got {err:?}");
                }
                other => panic!("expected unmask_not_permitted, got {other:?}"),
            }

            // Roles are CLUSTER-scoped, not database-scoped: leaving this one
            // behind makes every later run of this test anywhere on the same
            // server fail at `CREATE ROLE` with 42710, in a database that looks
            // pristine. Drop the schema first so the role owns nothing.
            let _ = pool
                .execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
                .await;
            let _ = pool
                .execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[])
                .await;
            release_pg(host, pool).await;
        })
    })
}
