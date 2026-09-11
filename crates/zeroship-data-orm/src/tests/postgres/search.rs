//! PostgreSQL search contracts.
use super::fixtures::*;

use crate::tests::fixtures::Host;

use crate::tests::fixtures::{self};

use compio_postgres::Pool;

use zeroship_data_orm::binding::DbBinding;

use zeroship_data_sql::value::value;

/// Test gate for `vector_search_returns_k_nearest`.
///
/// Insert 100 rows x 128-d random unit vectors; query with a known
/// vector and assert the top-10 closest by cosine distance form the
/// expected SET (membership, not strict order -- FP determinism not
/// promised across pgvector versions).
///
/// Requires the `vector` extension, which a stock `postgres` image does not
/// bundle. `require_pgvector` refuses the run and names the image that carries
/// it (see docs/runbooks/docker-compose.md); there is no attribute that turns
/// the absence into a pass.
#[test]
fn vector_search_returns_k_nearest() {
    Host::test(|host| {
        host.run(async {
            use zeroship_data_orm::backend::VectorMetric;

            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());
            require_pgvector(&pool).await;

            let app = crate::tests::fixtures::test_app_id!();

            let app = app.as_str();
            let coll = "docs";
            // Provision the per-app ROLE, not just the schema. `vector_search` resolves
            // the binding before it plans, and a schema without its role fails closed
            // with `schema_not_provisioned` - which is what this test did from the day
            // it was written until 2026-09-01. It never surfaced because the test was
            // statically `#[ignore]`d, so a setup gap looked like a missing extension.
            let _role = provision_app_with_role(&pool, app).await;
            crate::tests::fixtures::roles::ensure_per_app_role(&pool, app)
                .await
                .unwrap();
            // The six non-`id` platform system columns are part of every real creator
            // table and are named unconditionally by the implicit read projection the
            // vector search builds, so the fixture carries them too.
            pool.execute(
                &format!(
                    "CREATE TABLE \"{app}\".\"{coll}\" (\
               id SERIAL PRIMARY KEY, \
               embedding vector(8) NOT NULL, \
               created_at TIMESTAMPTZ DEFAULT NOW(), \
               updated_at TIMESTAMPTZ DEFAULT NOW(), \
               created_by TEXT, \
               updated_by TEXT, \
               version INTEGER NOT NULL DEFAULT 1, \
               deleted_at TIMESTAMPTZ\
             )"
                ),
                &[],
            )
            .await
            .unwrap();

            // Deterministic pseudo-random unit vectors. We only care that the
            // top-k membership is reproducible; the absolute values don't matter
            // beyond being unique per row.
            fn mk_unit(i: usize, dims: usize) -> Vec<f32> {
                let mut v = vec![0.0f32; dims];
                for (j, slot) in v.iter_mut().enumerate() {
                    // splitmix-style scramble so adjacent rows don't accidentally
                    // collide on the unit sphere.
                    let x = (i.wrapping_mul(2654435761)) ^ (j.wrapping_mul(40503));
                    *slot = ((x & 0xffff) as f32 / 65536.0) - 0.5;
                }
                // Normalise.
                let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                if norm > 0.0 {
                    for x in v.iter_mut() {
                        *x /= norm;
                    }
                }
                v
            }

            fn fmt_vec(v: &[f32]) -> String {
                let parts: Vec<String> = v.iter().map(|x| x.to_string()).collect();
                format!("[{}]", parts.join(","))
            }

            // The per-app role gets NO table privileges from provisioning alone - the
            // grants are explicit and per-column, which is the same fact production
            // carries (a create-plus-migrate leaves the runtime role unable to read its
            // own tables until the grants run). Without this the search fails closed
            // with `permission denied for table docs`, correctly.
            fixtures::grant_all_runtime_table_columns(&pool, app, coll).await;

            let dims = 8usize;
            for i in 0..100usize {
                let v = mk_unit(i, dims);
                let lit = fmt_vec(&v);
                pool.execute(
                    &format!(
                        "INSERT INTO \"{app}\".\"{coll}\" (embedding) VALUES ($1::text::vector)"
                    ),
                    &[&lit as &(dyn compio_postgres::types::ToSql + Sync)],
                )
                .await
                .unwrap();
            }

            // Query with row #0's exact vector — its own row must be in the
            // top-10. We assert MEMBERSHIP (not strict order) because pgvector
            // distance ties between FP-close vectors can re-order across builds.
            let query = mk_unit(0, dims);
            let backend = zeroship_data_orm::backend::PostgresBackend::new(
                pool.clone(),
                url.clone(),
                host.key_source(),
            );
            // The search's projection is the descriptor's field list; install the entry
            // this deploy's runtime descriptor would have planted at boot.
            crate::tests::fixtures::cache_schema(
                app,
                coll,
                value!({ "embedding": { "type": "vector", "vectorDims": 8 } }),
            );
            let rows = zeroship_data_orm::search::Search::vector_search(
                &backend,
                None,
                zeroship_data_orm::search::VectorSearch {
                    binding: &DbBinding::cold_start(app),
                    collection: coll,
                    column: "embedding",
                    query: &query,
                    k: 10,
                    metric: VectorMetric::Cosine,
                    filter: &zeroship_data_sql::value::Value::Null,
                    schema: &zeroship_data_orm::descriptor::collection_schema(
                        &DbBinding::cold_start(app),
                        coll,
                    )
                    .expect("descriptor slice for the search fixture"),
                },
            )
            .await
            .unwrap_or_else(|e| panic!("vector_search failed: {e:?}"));

            assert_eq!(rows.len(), 10, "expected k=10 rows, got {}", rows.len());
            // Row id #1 (1-indexed via SERIAL) must be in the top-10 (it
            // matches the query exactly).
            let ids: Vec<i64> = rows
                .iter()
                .filter_map(|r| {
                    r.get("id")
                        .and_then(zeroship_data_sql::value::Value::as_i64)
                })
                .collect();
            assert!(
                ids.contains(&1),
                "exact-match row #1 must be in top-10, got ids={ids:?}"
            );
            // Every row must carry the synthetic _distance column.
            for r in &rows {
                assert!(r.get("_distance").is_some(), "row missing _distance: {r}");
            }
            drop(backend);
            release_pg(host, pool).await;
        })
    })
}

/// Test gate for `pgvector_extension_missing_reports_typed_error`.
///
/// Drops the `vector` extension (if present), constructs a fresh
/// backend so the probe cache starts empty, and asserts that
/// `vector_search` surfaces
/// `DbError::Configuration { code: "vector_extension_missing", .. }`.
///
/// It searches TWICE on purpose. `ensure_pgvector_available` has two
/// miss arms -- one that runs the `pg_extension` probe and caches
/// `Some(false)`, one that reads that cache -- and they construct the
/// error separately. The first call takes the probe arm, the second the
/// cached arm, so a divergence between them fails here. The pairing used
/// to fall out of calling `ensure_vector_index` then `vector_search`;
/// with the DDL half deleted the second arm would otherwise go unruled-on.
///
/// The DROP requires sufficient privileges; tests run as the bootstrap
/// `postgres` superuser, which has them. An extension that survives the drop -
/// because another object depends on it - FAILS this test, naming the query
/// that finds the dependents. The drop is the fixture, not a cleanup: with the
/// extension still installed the typed-error arm is never reached, so a pass
/// there would report a contract nobody checked.
///
/// THIS COMMENT DESCRIBED THE OPPOSITE UNTIL 2026-09-08, AND IT DESCRIBED
/// NEITHER THE CODE BELOW NOR ITS OWN REASONING. It said the test would
/// "silently re-skip" and that "we don't fail the suite in that case because
/// the typed-error assertion is the load-bearing part of the contract" - which
/// is the argument FOR failing, since a re-skip is precisely the case where
/// that load-bearing assertion did not run.
#[test]
fn pgvector_extension_missing_reports_typed_error() {
    Host::test(|host| {
        host.run(async {
            use zeroship_data_orm::backend::{PostgresBackend, VectorMetric};
            use zeroship_data_orm::error::DbError;

            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

            // This test needs the extension ABSENT - it asserts the shape of the error
            // raised when it is missing - so the drop is the fixture, not a cleanup.
            let dropped = pool
                .execute("DROP EXTENSION IF EXISTS vector CASCADE", &[])
                .await;

            let still_present = pool
                .query_text_params("SELECT 1 FROM pg_extension WHERE extname='vector'", &[])
                .await
                .map(|rows| !rows.is_empty())
                .unwrap_or(false);
            assert!(
                !still_present,
                "The `vector` extension could not be removed, and this test needs it ABSENT.\n\
         \n\
         \x20 backend:   PostgreSQL\n\
         \x20 extension: vector (pgvector)\n\
         \x20 drop said: {dropped:?}\n\
         \n\
         This is the inverse of the other pgvector tests: it asserts the TYPED\n\
         ERROR raised when the extension is missing, so an installed one leaves\n\
         that arm unexercised. It used to skip here, which reported the same\n\
         green as a run that had ruled on the error shape.\n\
         \n\
         The usual cause is another object depending on it - a `vector` column\n\
         or index left behind by a sibling test, or by a run that was\n\
         interrupted. Find the dependents and drop them:\n\
         \x20 SELECT * FROM pg_depend d JOIN pg_extension e ON d.refobjid = e.oid\n\
         \x20 WHERE e.extname = 'vector';\n\
         \n\
         A database used by nothing else is the cheaper fix; this suite creates\n\
         its own schemas and expects to own the database it is pointed at.\n\
         \n\
         There is no environment variable that makes this a skip."
            );

            let backend = zeroship_data_orm::backend::PostgresBackend::new(
                pool.clone(),
                url.clone(),
                host.key_source(),
            );

            // No descriptor entry is installed for `vector_missing`, and that is
            // deliberate: `ensure_pgvector_available` runs BEFORE the schema resolve,
            // so the extension error must still be the one that surfaces. If the order
            // ever flipped, this would fail with `collection_not_declared` instead.
            async fn search(backend: &PostgresBackend) -> DbError {
                zeroship_data_orm::search::Search::vector_search(
                    backend,
                    None,
                    zeroship_data_orm::search::VectorSearch {
                        binding: &DbBinding::cold_start("vector_missing"),
                        collection: "any",
                        column: "any",
                        query: &[0.0f32; 8],
                        k: 10,
                        metric: VectorMetric::Cosine,
                        filter: &zeroship_data_sql::value::Value::Null,
                        schema: &zeroship_data_sql::value::Value::Null,
                    },
                )
                .await
                .expect_err("missing extension must yield a typed error on search")
            }

            // First call: the probe arm (cache empty -> `SELECT 1 FROM pg_extension`).
            let probe_err = search(&backend).await;
            // Second call: the cached arm (`pgvector_available == Some(false)`).
            let cached_err = search(&backend).await;

            // RESTORE the extension BEFORE asserting: this test deliberately drops a
            // SHARED, cluster-/db-wide object (the `vector` extension lives in
            // `public`, not in a per-app schema), so leaving it dropped breaks every
            // vector-dependent test ordered after this one in a single-threaded run
            // (e.g. `p4_round_trip_encrypted_masked_vector_via_descriptor_metadata`).
            // Restore happens before the
            // assertions so a failed assertion can never leak the dropped state.
            pool.execute("CREATE EXTENSION IF NOT EXISTS vector", &[])
                .await
                .expect("restore the shared vector extension after the missing-extension probe");

            for (arm, err) in [("probe", probe_err), ("cached", cached_err)] {
                match err {
                    DbError::Configuration {
                        code,
                        message,
                        hint,
                    } => {
                        assert_eq!(code, "vector_extension_missing", "{arm} arm: got {message}");
                        assert!(
                            hint.as_deref()
                                .map(|h| h.contains("CREATE EXTENSION"))
                                .unwrap_or(false),
                            "{arm} arm: hint must mention `CREATE EXTENSION vector;`: {hint:?}"
                        );
                    }
                    other => panic!(
                        "{arm} arm: expected Configuration {{ vector_extension_missing }}, got {other:?}"
                    ),
                }
            }
            drop(backend);
            release_pg(host, pool).await;
        })
    })
}

/// Test gate for `vector_dimension_mismatch_rejected_at_insert`.
///
/// pgvector enforces the declared dim at INSERT time (the `vector(N)`
/// column type rejects a literal whose dim ≠ N at parse-cast). This
/// test asserts the failure is observable and surfaces as a typed
/// `DbError::CheckViolation` / `Internal` / `Transient` — we don't pin
/// the variant strictly because pgvector reports as ERROR 22000
/// (`data_exception`), which our SQLSTATE classifier maps to
/// `Internal`. The shape contract: the error message MUST mention the
/// expected vs. actual dim count.
#[test]
fn vector_dimension_mismatch_rejected_at_insert() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());
            require_pgvector(&pool).await;

            let app = crate::tests::fixtures::test_app_id!();

            let app = app.as_str();
            let coll = "docs";
            // Same provisioning gap as `vector_search_returns_k_nearest`: a schema
            // without its per-app role fails closed before the insert is ever attempted.
            let _role = provision_app_with_role(&pool, app).await;
            crate::tests::fixtures::roles::ensure_per_app_role(&pool, app)
                .await
                .unwrap();
            pool.execute(
                &format!(
                    "CREATE TABLE \"{app}\".\"{coll}\" (\
               id SERIAL PRIMARY KEY, \
               embedding vector(128) NOT NULL\
             )"
                ),
                &[],
            )
            .await
            .unwrap();

            // Insert a 256-d vector into a 128-d column — pgvector must reject.
            let mut parts = Vec::with_capacity(256);
            for i in 0..256 {
                parts.push(format!("{}.0", i as f32 / 256.0));
            }
            let lit = format!("[{}]", parts.join(","));
            let result = pool
                .query_text_params(
                    &format!(
                        "INSERT INTO \"{app}\".\"{coll}\" (embedding) VALUES ($1::text::vector)"
                    ),
                    &[&lit],
                )
                .await;
            let err = result.expect_err("256-d into vector(128) column must fail");
            // `{err}` is NOT enough: `compio_postgres::Error`'s Display renders the bare
            // string "db error" and puts the server's message only in the source chain,
            // so this assertion was checking a constant. Measured 2026-09-01 - the
            // server sends "expected 128 dimensions, not 256" and `{err}` shows none of
            // it. Production is unaffected because `pg_error::classify` walks the chain
            // (`walk_pg_chain`) rather than formatting; anything that formats a driver
            // error with `{}` for an operator loses the cause.
            let msg = format!("{err:?}");
            // pgvector messages vary across versions; assert on the digits 256
            // and 128 (both should appear) and on "vector" anchor.
            assert!(
                msg.contains("128") || msg.contains("256") || msg.to_lowercase().contains("vector"),
                "error message must mention dim mismatch: {msg}"
            );
            release_pg(host, pool).await;
        })
    })
}

/// Test gate for `near_returns_within_radius`.
///
/// 10 points around London at varying distances from the centre
/// `(51.5074, -0.1278)`. `near()` with a 1km radius returns only the
/// points actually within 1km (assert by membership set, not strict
/// ordering — ST_Distance is FP-deterministic in modern PostGIS but we
/// don't pin the order).
///
/// Requires PostGIS: `require_postgis` FAILS on an image without the extension
/// rather than skipping, and no attribute removes this test from the run.
#[test]
fn near_returns_within_radius() {
    Host::test(|host| {
        host.run(async {
            use zeroship_data_orm::backend::GeoPoint;

            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());
            require_postgis(&pool).await;

            let app = crate::tests::fixtures::test_app_id!();

            let app = app.as_str();
            let coll = "places";
            pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
                .await
                .unwrap();
            pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[])
                .await
                .unwrap();
            // System columns for the same reason as the vector fixture above: the
            // spatial base query projects the descriptor's field list plus all seven.
            pool.execute(
                &format!(
                    "CREATE TABLE \"{app}\".\"{coll}\" (\
               id SERIAL PRIMARY KEY, \
               location geography(POINT, 4326) NOT NULL, \
               created_at TIMESTAMPTZ DEFAULT NOW(), \
               updated_at TIMESTAMPTZ DEFAULT NOW(), \
               created_by TEXT, \
               updated_by TEXT, \
               version INTEGER NOT NULL DEFAULT 1, \
               deleted_at TIMESTAMPTZ\
             )"
                ),
                &[],
            )
            .await
            .unwrap();
            crate::tests::fixtures::cache_schema(
                app,
                coll,
                value!({ "id": {"type":"integer", "primaryKey":true}, "location": { "type": "geoPoint" } }),
            );

            crate::tests::fixtures::roles::ensure_per_app_role(&pool, app)
                .await
                .unwrap();
            fixtures::grant_all_runtime_table_columns(&pool, app, coll).await;

            let london = GeoPoint {
                lat: 51.5074,
                lng: -0.1278,
            };
            // 10 points: 5 within ~1km of London (small lat/lng offsets) and
            // 5 well outside (several km away). One degree of latitude is
            // ~111km, so 0.005 deg ≈ 555m and 0.05 deg ≈ 5.5km.
            let offsets: Vec<(f64, f64, bool)> = vec![
                (0.0, 0.0, true),     // dead-centre
                (0.001, 0.001, true), // ~140m
                (0.003, 0.003, true), // ~420m
                (-0.005, 0.0, true),  // ~555m south
                (0.0, 0.005, true),   // about 350m east (cos(51.5 deg) ~= 0.62)
                (0.05, 0.0, false),   // ~5.5km north
                (-0.05, 0.0, false),  // ~5.5km south
                (0.0, 0.05, false),   // ~3.5km east
                (0.0, -0.05, false),  // ~3.5km west
                (0.1, 0.1, false),    // ~11km NE
            ];
            let mut expected_within: Vec<i64> = Vec::new();
            for (i, (dlat, dlng, within_1km)) in offsets.iter().enumerate() {
                let lng = london.lng + dlng;
                let lat = london.lat + dlat;
                let lit = format!("POINT({lng} {lat})");
                pool.execute(
                    &format!(
                        "INSERT INTO \"{app}\".\"{coll}\" (location) VALUES (ST_GeogFromText($1))"
                    ),
                    &[&lit as &(dyn compio_postgres::types::ToSql + Sync)],
                )
                .await
                .unwrap();
                if *within_1km {
                    expected_within.push((i + 1) as i64);
                }
            }

            let backend = zeroship_data_orm::backend::PostgresBackend::new(
                pool.clone(),
                url.clone(),
                host.key_source(),
            );
            let rows = zeroship_data_orm::search::Search::spatial_near(
                &backend,
                None,
                zeroship_data_orm::search::SpatialSearch {
                    binding: &DbBinding::cold_start(app),
                    collection: coll,
                    column: "location",
                    point: london,
                    radius_m: 1000.0,
                    filter: &zeroship_data_sql::value::Value::Null,
                    limit: None,
                    schema: &value!({ "id": {"type":"integer", "primaryKey":true}, "location": { "type": "geoPoint" } }),
                },
            )
            .await
            .unwrap_or_else(|e| panic!("spatial_near failed: {e:?}"));

            let returned_ids: std::collections::BTreeSet<i64> = rows
                .iter()
                .filter_map(|r| {
                    r.get("id")
                        .and_then(zeroship_data_sql::value::Value::as_i64)
                })
                .collect();
            let expected: std::collections::BTreeSet<i64> = expected_within.into_iter().collect();
            assert_eq!(
                returned_ids, expected,
                "near(1km) membership mismatch: returned={returned_ids:?} expected={expected:?}"
            );
            for r in &rows {
                assert!(
                    r.get("_distance_m").is_some(),
                    "row missing _distance_m: {r}"
                );
            }
            drop(backend);
            release_pg(host, pool).await;
        })
    })
}

/// Test gate for `postgis_extension_missing_reports_typed_error`.
///
/// When the database has no PostGIS, `spatial_near` must surface
/// `DbError::Configuration { code: "postgis_extension_missing", .. }`.
/// Same shape as `pgvector_extension_missing_reports_typed_error`,
/// including the two-call pairing that rules on `ensure_postgis_available`'s
/// probe arm and its cached arm separately.
#[test]
fn postgis_extension_missing_reports_typed_error() {
    Host::test(|host| {
        host.run(async {
            use zeroship_data_orm::backend::{GeoPoint, PostgresBackend};
            use zeroship_data_orm::error::DbError;

            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

            // This test needs the extension ABSENT - it asserts the shape of the error
            // raised when it is missing - so the drop is the fixture, not a cleanup.
            let dropped = pool
                .execute("DROP EXTENSION IF EXISTS postgis CASCADE", &[])
                .await;

            let still_present = pool
                .query_text_params("SELECT 1 FROM pg_extension WHERE extname='postgis'", &[])
                .await
                .map(|rows| !rows.is_empty())
                .unwrap_or(false);
            assert!(
                !still_present,
                "The `postgis` extension could not be removed, and this test needs it ABSENT.\n\
         \n\
         \x20 backend:   PostgreSQL\n\
         \x20 extension: postgis\n\
         \x20 drop said: {dropped:?}\n\
         \n\
         This is the inverse of the other PostGIS tests: it asserts the TYPED\n\
         ERROR raised when the extension is missing, so an installed one leaves\n\
         that arm unexercised. It used to skip here, which reported the same\n\
         green as a run that had ruled on the error shape.\n\
         \n\
         The usual cause is another object depending on it - a `geography`\n\
         column or spatial index left behind by a sibling test, or by a run that\n\
         was interrupted. Find the dependents and drop them:\n\
         \x20 SELECT * FROM pg_depend d JOIN pg_extension e ON d.refobjid = e.oid\n\
         \x20 WHERE e.extname = 'postgis';\n\
         \n\
         A database used by nothing else is the cheaper fix; this suite creates\n\
         its own schemas and expects to own the database it is pointed at.\n\
         \n\
         There is no environment variable that makes this a skip."
            );

            let backend = zeroship_data_orm::backend::PostgresBackend::new(
                pool.clone(),
                url.clone(),
                host.key_source(),
            );

            // No descriptor entry, deliberately: the extension probe runs BEFORE the
            // schema resolve, so this must still surface `postgis_extension_missing`.
            async fn near(backend: &PostgresBackend) -> DbError {
                zeroship_data_orm::search::Search::spatial_near(
                    backend,
                    None,
                    zeroship_data_orm::search::SpatialSearch {
                        binding: &DbBinding::cold_start("postgis_missing"),
                        collection: "any",
                        column: "any",
                        point: GeoPoint { lat: 0.0, lng: 0.0 },
                        radius_m: 1000.0,
                        filter: &zeroship_data_sql::value::Value::Null,
                        limit: None,
                        schema: &zeroship_data_sql::value::Value::Null,
                    },
                )
                .await
                .expect_err("missing PostGIS must yield a typed error on near")
            }

            // First call takes the probe arm, second the cached arm.
            let probe_err = near(&backend).await;
            let cached_err = near(&backend).await;
            for (arm, err) in [("probe", probe_err), ("cached", cached_err)] {
                match err {
                    DbError::Configuration {
                        code,
                        message,
                        hint,
                    } => {
                        assert_eq!(
                            code, "postgis_extension_missing",
                            "{arm} arm: got {message}"
                        );
                        assert!(
                            hint.as_deref()
                                .map(|h| h.contains("CREATE EXTENSION"))
                                .unwrap_or(false),
                            "{arm} arm: hint must mention `CREATE EXTENSION postgis;`: {hint:?}"
                        );
                    }
                    other => panic!(
                        "{arm} arm: expected Configuration {{ postgis_extension_missing }}, got {other:?}"
                    ),
                }
            }
            drop(backend);
            release_pg(host, pool).await;
        })
    })
}
