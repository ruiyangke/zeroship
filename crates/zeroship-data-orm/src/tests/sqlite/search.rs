//! SQLite search contracts.
use super::fixtures::*;

use crate::tests::fixtures::Host;

use zeroship_data_orm::backend::BackendHandle;

use zeroship_data_orm::binding::DbBinding;

use zeroship_data_orm::error::DbError;

use zeroship_data_orm::backend::VectorMetric;

use zeroship_data_orm::backend::GeoPoint;

use zeroship_data_orm::sql::{
    statement::{
        ResolvedPredicate, ReturnedColumn, SpatialNearParts, SpatialNearStatement, Statement,
        StorageType, Table,
    },
    Ident, IdentRole,
};

#[cfg(test)]
use crate::tests::fixtures::DatabaseFixture;

use zeroship_data_orm::search::Search;

/// Encode a `Vec<f32>` as a SQLite `x'<hex>'` blob literal.
///
/// Uses the same little-endian representation as the SQLite search backend.
fn vec_to_hex_lit(v: &[f32]) -> String {
    let mut hex = String::with_capacity(v.len() * 8 + 4);
    hex.push_str("x'");
    for f in v {
        for byte in f.to_le_bytes() {
            hex.push_str(&format!("{byte:02x}"));
        }
    }
    hex.push('\'');
    hex
}

/// Deterministic pseudo-random unit vector — same construction as the
/// PG arm's `vector_search_returns_k_nearest` so the membership
/// expectations match across backends (modulo FP non-determinism in
/// low significand bits, which the test asserts as set membership
/// rather than ordinal positions).
fn mk_unit_vec(i: usize, dims: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; dims];
    for (j, slot) in v.iter_mut().enumerate().take(dims) {
        let x = (i.wrapping_mul(2_654_435_761)) ^ (j.wrapping_mul(40_503));
        *slot = ((x & 0xffff) as f32 / 65_536.0) - 0.5;
    }
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
    v
}

#[test]
fn vector_search_returns_k_nearest_sqlite() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("vector_topk")
                .await
                .expect("ensure_app_schema");

            // CREATE TABLE with the BLOB column the SDK's `t.vector(dims)`
            // lowering emits. The CHECK constraint pins the write-side
            // dimension contract at the canonical write surface.
            let dims = 8usize;
            backend
                .execute_fixture(
                    &format!(
                        "CREATE TABLE \"vector_topk\".\"docs\" (\
                       id INTEGER PRIMARY KEY AUTOINCREMENT, \
                       embedding BLOB CHECK(length(embedding) = 32) NOT NULL, \
                       {SYSTEM_COLUMNS_SQLITE_TAIL}\
                     )"
                    ),
                    &[],
                )
                .await
                .expect("CREATE TABLE docs");
            crate::tests::fixtures::cache_schema(
                "vector_topk",
                "docs",
                crate::value!({ "embedding": { "type": "vector", "vectorDims": 8 } }),
            );

            for i in 0..100usize {
                let v = mk_unit_vec(i, dims);
                let hex = vec_to_hex_lit(&v);
                let sql =
                    format!("INSERT INTO \"vector_topk\".\"docs\" (embedding) VALUES ({hex})");
                backend.execute_fixture(&sql, &[]).await.expect("INSERT");
            }

            // Query with row #0's exact vector — its own row must be in
            // the top-10. Assert MEMBERSHIP (not strict order) to mirror
            // the PG arm's relaxed expectation.
            let query = mk_unit_vec(0, dims);
            let binding = DbBinding::cold_start("vector_topk");
            let schema = zeroship_data_orm::descriptor::collection_schema(&binding, "docs")
                .expect("descriptor slice for the search fixture");
            let registration = zeroship_data_orm::sql::registration::SqlRegistration::sqlite();
            let rows = backend
                .vector_search(
                    None,
                    zeroship_data_orm::search::VectorSearch::compile(
                        &binding,
                        "docs",
                        "embedding",
                        &query,
                        10,
                        VectorMetric::Cosine,
                        &crate::value::Value::Null,
                        &schema,
                        &registration,
                    )
                    .unwrap(),
                )
                .await
                .expect("vector_search");

            assert_eq!(rows.len(), 10, "expected k=10 rows, got {}", rows.len());
            let ids: Vec<i64> = rows
                .iter()
                .filter_map(|r| r.get("id").and_then(crate::value::Value::as_i64))
                .collect();
            // SQLite's INTEGER PRIMARY KEY AUTOINCREMENT starts at 1; row
            // 1 is the i=0 insert, which has zero cosine distance to its
            // own query vector.
            assert!(
                ids.contains(&1),
                "exact-match row #1 must be in top-10, got ids={ids:?}"
            );
            for r in &rows {
                let d = r
                    .get("_distance")
                    .and_then(crate::value::Value::as_f64)
                    .expect("row must carry _distance");
                assert!(d.is_finite(), "_distance must be finite, got {d}");
                assert!(d >= 0.0, "cosine distance is non-negative, got {d}");
            }
            // Row #1 should be the nearest (distance ~ 0).
            let first_id = rows[0]
                .get("id")
                .and_then(crate::value::Value::as_i64)
                .expect("first row id");
            assert_eq!(
                first_id, 1,
                "exact-match query must place its own row first"
            );
            let first_d = rows[0]
                .get("_distance")
                .and_then(crate::value::Value::as_f64)
                .expect("first row _distance");
            assert!(
                first_d.abs() < 1e-5,
                "exact-match distance must be ~0, got {first_d}"
            );
        });
    })
}

#[test]
fn vector_dimension_mismatch_rejected_at_insert_sqlite() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("vector_dim")
                .await
                .expect("ensure_app_schema");

            // 128-d column = 512-byte CHECK.
            backend
                .execute_fixture(
                    "CREATE TABLE \"vector_dim\".\"docs\" (\
                   id INTEGER PRIMARY KEY AUTOINCREMENT, \
                   embedding BLOB CHECK(length(embedding) = 512) NOT NULL\
                 )",
                    &[],
                )
                .await
                .expect("CREATE TABLE docs");

            // Insert a 256-d vector into a 128-d column. The CHECK
            // constraint must reject — the DatabaseFixture surface should
            // surface a SchemaRefused {check_violation} typed error.
            let oversized = mk_unit_vec(0, 256);
            let hex = vec_to_hex_lit(&oversized);
            let sql = format!("INSERT INTO \"vector_dim\".\"docs\" (embedding) VALUES ({hex})");
            let err = backend
                .execute_fixture(&sql, &[])
                .await
                .expect_err("256-d into 128-d column must fail");
            match err {
                DbError::SchemaRefused { code, .. } => {
                    assert_eq!(
                        code, "check_violation",
                        "expected check_violation, got {code}"
                    );
                }
                other => panic!("expected SchemaRefused {{ check_violation }}, got {other:?}"),
            }
        });
    })
}

#[test]
fn vector_search_respects_filter_sqlite() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("vector_filter")
                .await
                .expect("ensure_app_schema");

            // 4-d column = 16-byte CHECK.
            backend
                .execute_fixture(
                    &format!(
                        "CREATE TABLE \"vector_filter\".\"docs\" (\
                       id INTEGER PRIMARY KEY AUTOINCREMENT, \
                       tenant TEXT NOT NULL, \
                       embedding BLOB CHECK(length(embedding) = 16) NOT NULL, \
                       {SYSTEM_COLUMNS_SQLITE_TAIL}\
                     )"
                    ),
                    &[],
                )
                .await
                .expect("CREATE TABLE docs");
            crate::tests::fixtures::cache_schema(
                "vector_filter",
                "docs",
                crate::value!({
                    "tenant": { "type": "string" },
                    "embedding": { "type": "vector", "vectorDims": 4 },
                }),
            );

            // Insert 10 rows in tenant "a" and 10 rows in tenant "b".
            // The first row of each tenant uses an identical query
            // vector so the filter discriminates BY tenant, not by
            // proximity.
            for i in 0..10usize {
                let v = mk_unit_vec(i, 4);
                let hex = vec_to_hex_lit(&v);
                backend
                    .execute_fixture(
                        &format!(
                            "INSERT INTO \"vector_filter\".\"docs\" \
                           (tenant, embedding) VALUES ('a', {hex})"
                        ),
                        &[],
                    )
                    .await
                    .expect("INSERT a");
                backend
                    .execute_fixture(
                        &format!(
                            "INSERT INTO \"vector_filter\".\"docs\" \
                           (tenant, embedding) VALUES ('b', {hex})"
                        ),
                        &[],
                    )
                    .await
                    .expect("INSERT b");
            }

            // Query with tenant='a' filter — every returned row must
            // have tenant='a'. The filter uses the `$eq` operator the
            // SDK already emits.
            let query = mk_unit_vec(0, 4);
            let filter = crate::value!({ "tenant": { "$eq": "a" } });
            let binding = DbBinding::cold_start("vector_filter");
            let schema = zeroship_data_orm::descriptor::collection_schema(&binding, "docs")
                .expect("descriptor slice for the search fixture");
            let registration = zeroship_data_orm::sql::registration::SqlRegistration::sqlite();
            let rows = backend
                .vector_search(
                    None,
                    zeroship_data_orm::search::VectorSearch::compile(
                        &binding,
                        "docs",
                        "embedding",
                        &query,
                        10,
                        VectorMetric::Cosine,
                        &filter,
                        &schema,
                        &registration,
                    )
                    .unwrap(),
                )
                .await
                .expect("vector_search with filter");

            assert!(!rows.is_empty(), "filter must not exclude every row");
            assert_eq!(
                rows.len(),
                10,
                "filter before ranking must fill the result from matching rows"
            );
            for r in &rows {
                let tenant = r
                    .get("tenant")
                    .and_then(crate::value::Value::as_str)
                    .expect("row must carry tenant");
                assert_eq!(
                    tenant, "a",
                    "vector_search with tenant=a filter returned tenant={tenant}: {r}"
                );
            }
        });
    })
}

#[test]
fn vector_l2_distance_matches_cosine_for_unit_vectors_sqlite() {
    Host::test(|host| {
        // Sanity check on the math: for unit vectors, ||a-b||² = 2 * (1 - cos θ)
        // = 2 * cos_distance. Search the same source rows under each metric.
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("vector_math")
                .await
                .expect("ensure_app_schema");

            backend
                .execute_fixture(
                    &format!(
                        "CREATE TABLE \"vector_math\".\"docs\" (\
                       id INTEGER PRIMARY KEY AUTOINCREMENT, \
                       emb_cos BLOB CHECK(length(emb_cos) = 16) NOT NULL, \
                       emb_l2  BLOB CHECK(length(emb_l2)  = 16) NOT NULL, \
                       {SYSTEM_COLUMNS_SQLITE_TAIL}\
                     )"
                    ),
                    &[],
                )
                .await
                .expect("CREATE TABLE docs");
            crate::tests::fixtures::cache_schema(
                "vector_math",
                "docs",
                crate::value!({
                    "emb_cos": { "type": "vector", "vectorDims": 4 },
                    "emb_l2": { "type": "vector", "vectorDims": 4 },
                }),
            );

            let v1 = mk_unit_vec(0, 4);
            let v2 = mk_unit_vec(1, 4);
            let hex1 = vec_to_hex_lit(&v1);
            let hex2 = vec_to_hex_lit(&v2);
            backend
                .execute_fixture(
                    &format!(
                        "INSERT INTO \"vector_math\".\"docs\" (emb_cos, emb_l2) \
                     VALUES ({hex1}, {hex1})"
                    ),
                    &[],
                )
                .await
                .expect("INSERT v1");
            backend
                .execute_fixture(
                    &format!(
                        "INSERT INTO \"vector_math\".\"docs\" (emb_cos, emb_l2) \
                     VALUES ({hex2}, {hex2})"
                    ),
                    &[],
                )
                .await
                .expect("INSERT v2");

            // Query the cosine distance from row 1 (v1) to v2.
            let binding = DbBinding::cold_start("vector_math");
            let schema = zeroship_data_orm::descriptor::collection_schema(&binding, "docs")
                .expect("descriptor slice for the search fixture");
            let registration = zeroship_data_orm::sql::registration::SqlRegistration::sqlite();
            let cos_rows = backend
                .vector_search(
                    None,
                    zeroship_data_orm::search::VectorSearch::compile(
                        &binding,
                        "docs",
                        "emb_cos",
                        &v1,
                        2,
                        VectorMetric::Cosine,
                        &crate::value::Value::Null,
                        &schema,
                        &registration,
                    )
                    .unwrap(),
                )
                .await
                .expect("cosine search");
            let l2_rows = backend
                .vector_search(
                    None,
                    zeroship_data_orm::search::VectorSearch::compile(
                        &binding,
                        "docs",
                        "emb_l2",
                        &v1,
                        2,
                        VectorMetric::L2,
                        &crate::value::Value::Null,
                        &schema,
                        &registration,
                    )
                    .unwrap(),
                )
                .await
                .expect("l2 search");

            // The row with id=2 (the OTHER unit vector) must appear in
            // both result sets; its cosine and L2 distances must satisfy
            // L2² ≈ 2 * cos_distance.
            let find = |rows: &[crate::value::Value], target_id: i64| -> f64 {
                rows.iter()
                    .find(|r| r.get("id").and_then(crate::value::Value::as_i64) == Some(target_id))
                    .and_then(|r| r.get("_distance").and_then(crate::value::Value::as_f64))
                    .expect("row with target id must be present")
            };
            let cos_d = find(&cos_rows, 2);
            let l2_d = find(&l2_rows, 2);
            let lhs = l2_d * l2_d;
            let rhs = 2.0 * cos_d;
            assert!(
                (lhs - rhs).abs() < 1e-3,
                "||v1-v2||^2 = {lhs}, 2 * cos_d = {rhs}"
            );
        });
    })
}

/// Encode a `GeoPoint` as a SQLite `x'<hex>'` blob literal — 2× LE
/// f64 = 16 bytes. Mirrors `vec_to_hex_lit` for vectors. We use this
/// because the session actor's text-param path can't carry raw bytes
/// at the SQL boundary.
fn point_to_hex_lit(p: GeoPoint) -> String {
    let mut hex = String::with_capacity(16 * 2 + 4);
    hex.push_str("x'");
    for byte in p.lat.to_le_bytes() {
        hex.push_str(&format!("{byte:02x}"));
    }
    for byte in p.lng.to_le_bytes() {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex.push('\'');
    hex
}

/// **Test gate**: `near_returns_within_radius` (SQLite).
///
/// 10 points around London at varying distances from the centre
/// `(51.5074, -0.1278)`. `near()` with a 1km radius returns only the
/// points actually within 1km — assert by membership set. No PostGIS
/// dependency on this arm: the haversine math is pure Rust + the
/// `geoPoint` column is a plain BLOB.
#[test]
fn near_returns_within_radius() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("near_radius")
                .await
                .expect("ensure_app_schema");

            backend
                .execute_fixture(
                    &format!(
                        "CREATE TABLE \"near_radius\".\"places\" (\
                       id INTEGER PRIMARY KEY AUTOINCREMENT, \
                       location BLOB CHECK(length(location) = 16) NOT NULL, \
                       {SYSTEM_COLUMNS_SQLITE_TAIL}\
                     )"
                    ),
                    &[],
                )
                .await
                .expect("CREATE TABLE places");
            crate::tests::fixtures::cache_schema(
                "near_radius",
                "places",
                crate::value!({ "location": { "type": "geoPoint" } }),
            );

            let london = GeoPoint {
                lat: 51.5074,
                lng: -0.1278,
            };
            // 10 points: 5 within ~1km (small lat/lng offsets) and 5
            // well outside (several km away). One degree of latitude is
            // ~111km, so 0.005 deg ≈ 555m and 0.05 deg ≈ 5.5km.
            let offsets: Vec<(f64, f64, bool)> = vec![
                (0.0, 0.0, true),     // dead-centre
                (0.001, 0.001, true), // ~140m
                (0.003, 0.003, true), // ~420m
                (-0.005, 0.0, true),  // ~555m south
                (0.0, 0.005, true),   // ~350m east at cos(51.5deg) ≈ 0.62
                (0.05, 0.0, false),   // ~5.5km north
                (-0.05, 0.0, false),  // ~5.5km south
                (0.0, 0.05, false),   // ~3.5km east
                (0.0, -0.05, false),  // ~3.5km west
                (0.1, 0.1, false),    // ~11km NE
            ];
            let mut expected_within: Vec<i64> = Vec::new();
            for (i, (dlat, dlng, within_1km)) in offsets.iter().enumerate() {
                let p = GeoPoint {
                    lat: london.lat + dlat,
                    lng: london.lng + dlng,
                };
                let hex = point_to_hex_lit(p);
                let sql =
                    format!("INSERT INTO \"near_radius\".\"places\" (location) VALUES ({hex})");
                backend
                    .execute_fixture(&sql, &[])
                    .await
                    .expect("INSERT location");
                if *within_1km {
                    expected_within.push((i + 1) as i64);
                }
            }

            let binding = DbBinding::cold_start("near_radius");
            let schema = zeroship_data_orm::descriptor::collection_schema(&binding, "places")
                .expect("descriptor slice for the search fixture");
            let registration = zeroship_data_orm::sql::registration::SqlRegistration::sqlite();
            let rows = backend
                .spatial_near(
                    None,
                    zeroship_data_orm::search::SpatialSearch::compile(
                        &binding,
                        "places",
                        "location",
                        london,
                        1000.0,
                        &crate::value::Value::Null,
                        None,
                        &schema,
                        &registration,
                    )
                    .unwrap(),
                )
                .await
                .expect("spatial_near");

            let returned_ids: std::collections::BTreeSet<i64> = rows
                .iter()
                .filter_map(|r| r.get("id").and_then(crate::value::Value::as_i64))
                .collect();
            let expected: std::collections::BTreeSet<i64> = expected_within.into_iter().collect();
            assert_eq!(
                returned_ids, expected,
                "near(1km) membership mismatch: returned={returned_ids:?} expected={expected:?}"
            );
            // Every row carries the synthetic `_distance_m` column.
            for r in &rows {
                let d = r
                    .get("_distance_m")
                    .and_then(crate::value::Value::as_f64)
                    .expect("row must carry _distance_m");
                assert!(d.is_finite(), "_distance_m must be finite, got {d}");
                assert!(
                    d <= 1000.0 + 1e-6,
                    "_distance_m={d} must be within the 1km radius (FP slack)"
                );
            }
            // The dead-centre row (id=1) is the closest.
            let first_id = rows[0]
                .get("id")
                .and_then(crate::value::Value::as_i64)
                .expect("first row id");
            assert_eq!(
                first_id, 1,
                "dead-centre (offset (0,0)) row must be first by distance"
            );
            let first_d = rows[0]
                .get("_distance_m")
                .and_then(crate::value::Value::as_f64)
                .expect("first row _distance_m");
            assert!(
                first_d < 1.0,
                "dead-centre distance must be < 1m, got {first_d}"
            );
        });
    })
}

/// A `near()` inside `db.transaction(fn)` must scan the transaction's own
/// connection.
///
/// The SQLite half of what `src/tests/search_tx_lane.rs` rules on for PostgreSQL,
/// and it is a separate question rather than the same one twice: SC-2 Decision 1
/// gave this backend TWO connections, `op_conn` for autocommit reads and
/// `tx_conn` for the creator's transaction, and `SpatialIndex::spatial_near`
/// took `&self` - which can only ever mean `op_conn`. So a `near` issued inside
/// a transaction scanned a connection that cannot see that transaction's own
/// uncommitted rows.
///
/// **The control differs in one variable: `route.in_tx()`.** The same `near`,
/// over the same row, at the same instant, on a route captured outside the
/// transaction must return nothing - because on `op_conn` the row genuinely is
/// not there. That is what makes the subject arm a statement about the lane
/// rather than about the fixture.
#[test]
fn a_near_inside_a_transaction_sees_the_row_that_transaction_inserted() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            let app = "near_tx_lane";
            backend
                .attach_app_file(app)
                .await
                .expect("attach the app database");
            backend
                .execute_fixture(
                    &format!(
                        "CREATE TABLE \"{app}\".\"places\" (\
                       id INTEGER PRIMARY KEY AUTOINCREMENT, \
                       location BLOB CHECK(length(location) = 16) NOT NULL, \
                       {SYSTEM_COLUMNS_SQLITE_TAIL}\
                     )"
                    ),
                    &[],
                )
                .await
                .expect("CREATE TABLE places");
            crate::tests::fixtures::cache_schema(
                app,
                "places",
                crate::value!({ "location": { "type": "geoPoint" } }),
            );

            let london = GeoPoint {
                lat: 51.5074,
                lng: -0.1278,
            };

            let handle = BackendHandle::new(std::rc::Rc::new(backend));
            let admission =
                zeroship_data_orm::transaction::TxAdmission::acquire(app.to_owned()).await;
            zeroship_data_orm::transaction::exec_begin_or_savepoint(
                false,
                None,
                app,
                crate::sql::SchemaName::new(app).unwrap(),
                handle.clone(),
            )
            .await
            .unwrap();
            admission.handed_to_reducer();
            zeroship_data_orm::transaction::driver::run_operation(
                app,
                &format!(
                    "INSERT INTO \"{app}\".\"places\" (location) VALUES ({})",
                    point_to_hex_lit(london)
                ),
                &[],
            )
            .await
            .expect("write inside the transaction");

            let binding = DbBinding::cold_start(app);
            let args = crate::value!({
                "field": "location",
                "point": { "lat": london.lat, "lng": london.lng },
                "radius": 1000.0,
            });
            let near_on = async |route| {
                let plan = zeroship_data_orm::crud::plan_near(
                    &binding,
                    &zeroship_data_orm::sql::registration::SqlRegistration::sqlite(),
                    "places",
                    &args,
                )
                .expect("plan_near");
                zeroship_data_orm::crud::run_near(
                    &route,
                    binding.clone(),
                    "places".to_string(),
                    plan,
                )
                .await
                .expect("run_near")
                .rows
            };

            // ---- CONTROL: a route captured OUTSIDE the transaction. `op_conn`
            // cannot see the row, so an empty result here is what proves the
            // subject arm below is about the lane.
            let outside = near_on(
                zeroship_data_orm::tx_route::CapturedRoute::pool_for_tests(
                    app,
                    crate::sql::registration::SqlRegistration::sqlite(),
                )
                .bind(handle.clone())
                .unwrap(),
            )
            .await;
            assert!(
                outside.is_empty(),
                "the row must be invisible on the autocommit connection, or the \
             subject arm below cannot distinguish the two lanes: {outside:?}",
            );

            // ---- SUBJECT: the same near on the transaction's own lane.
            let inside = near_on(
                zeroship_data_orm::tx_route::CapturedRoute::tx_for_tests(
                    app,
                    crate::sql::registration::SqlRegistration::sqlite(),
                )
                .bind(handle.clone())
                .unwrap(),
            )
            .await;
            assert_eq!(
                inside.len(),
                1,
                "a near inside a transaction must reach the row that transaction \
             inserted; an empty result means the scan took `op_conn`: {inside:?}",
            );
            assert_eq!(
                inside[0]["location"],
                crate::value!({"lat":london.lat, "lng":london.lng})
            );
            assert!(
                inside[0]
                    .get("_distance_m")
                    .and_then(crate::value::Value::as_f64)
                    .is_some_and(|d| d < 1.0),
                "the row must carry its synthetic distance: {inside:?}",
            );

            assert!(matches!(
                zeroship_data_orm::transaction::exec_settle(app, false, None).await,
                zeroship_data_orm::transaction::SettleOutcome::Ok
            ));
        });
        host.reset();
    })
}

#[test]
fn near_uses_an_unreadable_identity_without_returning_it() {
    Host::test(|host| {
        host.run(async {
            let app = "near_hidden_identity";
            let (backend, _dir) = fresh_backend(host);
            backend.attach_app_file(app).await.unwrap();
            backend
                .execute_fixture(
                    &format!(
                        "CREATE TABLE \"{app}\".\"places\" (\
                         id INTEGER PRIMARY KEY, \
                         label TEXT NOT NULL, \
                         location BLOB NOT NULL)"
                    ),
                    &[],
                )
                .await
                .unwrap();
            let point = GeoPoint { lat: 1.0, lng: 2.0 };
            backend
                .execute_fixture(
                    &format!(
                        "INSERT INTO \"{app}\".\"places\" (id, label, location) \
                         VALUES (7, 'visible', {})",
                        point_to_hex_lit(point)
                    ),
                    &[],
                )
                .await
                .unwrap();

            let binding = DbBinding::cold_start(app);
            let schema = crate::value!({
                "id":{
                    "type":"integer",
                    "required":true,
                    "primaryKey":true,
                    "readable":false
                },
                "label":{"type":"string"},
                "location":{"type":"geoPoint"}
            });
            zeroship_data_orm::schema_cache::with_mut(|cache| {
                cache.insert_one(
                    &binding,
                    "places",
                    crate::tests::fixtures::native_fields(schema),
                )
            });
            let registration = zeroship_data_orm::sql::registration::SqlRegistration::sqlite();
            let route = zeroship_data_orm::tx_route::CapturedRoute::pool_for_tests(
                app,
                registration.clone(),
            )
            .bind(BackendHandle::new(std::rc::Rc::new(backend)))
            .unwrap();
            let plan = zeroship_data_orm::crud::plan_near(
                &binding,
                &registration,
                "places",
                &crate::value!({
                    "field":"location",
                    "point":{"lat":point.lat,"lng":point.lng},
                    "radius":1.0
                }),
            )
            .unwrap();
            let rows = zeroship_data_orm::crud::run_near(&route, binding, "places".into(), plan)
                .await
                .unwrap()
                .rows;

            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0]["label"], "visible");
            assert!(rows[0].get("id").is_none());
            assert!(rows[0]["_distance_m"].as_f64().is_some());
        });
        host.reset();
    })
}

#[test]
fn low_level_near_preserves_public_aliases_and_hides_ranking_identity() {
    Host::test(|host| {
        host.run(async {
            let app = "near_projection_aliases";
            let (backend, _dir) = fresh_backend(host);
            backend.attach_app_file(app).await.unwrap();
            backend
                .execute_fixture(
                    &format!(
                        "CREATE TABLE \"{app}\".\"places\" (\
                         id INTEGER PRIMARY KEY, \
                         label TEXT NOT NULL, \
                         location BLOB NOT NULL)"
                    ),
                    &[],
                )
                .await
                .unwrap();
            let point = GeoPoint { lat: 1.0, lng: 2.0 };
            backend
                .execute_fixture(
                    &format!(
                        "INSERT INTO \"{app}\".\"places\" (id, label, location) \
                         VALUES (7, 'visible', {})",
                        point_to_hex_lit(point)
                    ),
                    &[],
                )
                .await
                .unwrap();

            let binding = DbBinding::cold_start(app);
            let registration = zeroship_data_orm::sql::registration::SqlRegistration::sqlite();
            let compile = |column: &str, alias: &str| {
                let table = Table::aliased(
                    binding.schema().clone(),
                    Ident::parse_as("places", IdentRole::Collection).unwrap(),
                    Ident::parse_as("source", IdentRole::Alias).unwrap(),
                    [
                        ("id", StorageType::Integer),
                        ("label", StorageType::Text),
                        ("location", StorageType::GeoPoint),
                    ]
                    .map(|(name, storage)| {
                        (
                            Ident::parse_as(name, IdentRole::StoredColumn).unwrap(),
                            storage,
                        )
                    }),
                )
                .unwrap();
                registration
                    .compile(Statement::SpatialNear(
                        SpatialNearStatement::new(SpatialNearParts {
                            projection: vec![
                                ReturnedColumn {
                                    column: table.column(column).unwrap(),
                                    alias: Some(Ident::parse_as(alias, IdentRole::Alias).unwrap()),
                                },
                                ReturnedColumn {
                                    column: table.column("location").unwrap(),
                                    alias: None,
                                },
                            ],
                            identity: table.column("id").unwrap(),
                            spatial: table.column("location").unwrap(),
                            point: crate::value!({"lat":point.lat,"lng":point.lng}),
                            radius_m: 1.0,
                            predicate: ResolvedPredicate::Const(true),
                            limit: 1,
                            table,
                        })
                        .unwrap(),
                    ))
                    .unwrap()
            };

            let label_as_id = backend
                .spatial_near(
                    None,
                    zeroship_data_orm::search::SpatialSearch {
                        binding: &binding,
                        query: compile("label", "id"),
                        column: "location",
                        point,
                        radius_m: 1.0,
                        limit: 1,
                    },
                )
                .await
                .unwrap();
            assert_eq!(label_as_id[0]["id"], "visible");
            assert!(label_as_id[0].get("__zs_spatial_identity").is_none());

            let id_as_key = backend
                .spatial_near(
                    None,
                    zeroship_data_orm::search::SpatialSearch {
                        binding: &binding,
                        query: compile("id", "key"),
                        column: "location",
                        point,
                        radius_m: 1.0,
                        limit: 1,
                    },
                )
                .await
                .unwrap();
            assert_eq!(id_as_key[0]["key"], 7);
            assert!(id_as_key[0].get("id").is_none());
            assert!(id_as_key[0].get("__zs_spatial_identity").is_none());
        });
        host.reset();
    })
}

/// The six non-`id` system columns, for a fixture that keeps its own `id`
/// declaration. The vector / spatial fixtures use an `INTEGER PRIMARY KEY
/// AUTOINCREMENT` rowid so their assertions can name `id: 1`, but they still
/// need the other six: the implicit read projection those searches build names
/// all seven system columns unconditionally, and a table missing them is not a
/// table the data plane can read.
const SYSTEM_COLUMNS_SQLITE_TAIL: &str = "\
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, \
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, \
  created_by TEXT NULL, \
  updated_by TEXT NULL, \
  version INTEGER NOT NULL DEFAULT 1, \
  deleted_at TEXT NULL";
