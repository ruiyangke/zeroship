//! The `DbPlan` search family, executed against a real server.
//!
//! # Why this target exists separately from the IR's own tests
//!
//! `zeroship-data-sql` declares no dependencies at all, so its tests can
//! compare the SQL it renders against a fixture string and nothing more. A
//! string comparison cannot tell whether `"embedding" <=> $1::vector` is
//! syntax `pgvector` accepts, whether `ST_DWithin` over a `geography` column
//! uses the operand order the plan claims, or whether the ranking the plan
//! promises is the ranking the server produces.
//!
//! Everything here therefore executes. Nothing asserts on SQL text.
//!
//! # Run it
//!
//! `cargo xtask test data --filter 'test(search_ir_live::)'`
//! PostgreSQL comes from an owned testcontainer; Docker is required.
//!
//! The server needs **both** `vector` and `postgis`; the tests create the
//! extensions themselves and fail loudly, naming the extension, if the server
//! cannot supply one. They do not skip: a skipped extension check is a green
//! run that measured nothing, and the divergence this family is about is
//! invisible without a server that has both.
//!
//! Tests share a schema and serialize its lifetime through the runtime entry.
//!
//! # What these arms do NOT establish
//!
//! **The shipped product path does not use the IR.** No crate outside
//! `zeroship-data-sql` depends on it - the dependency this target adds is a
//! `[dev-dependencies]` one, declared for these tests. `env.db.<coll>.search()`
//! still reaches `zeroship_data_sql::compile::build_vector_search`
//! (`crates/zeroship-data-orm/src/backend/postgres/implementation.rs:456`).
//!
//! That is why `the_ir_and_the_shipped_builder_rank_identically` is here. It is
//! the only arm that ties the two together, and it does it the one way that is
//! available without rewiring: both builders are asked for the same search,
//! both statements are executed against the same rows, and the two orderings
//! are compared. It rules on behaviour, not on a call graph.

#![allow(clippy::items_after_statements)]

use compio_postgres::Pool;
use zeroship_data_sql::render::postgres::render_search;
use zeroship_data_sql::value;
use zeroship_data_sql::{
    CompareOp, GeoPoint, Ident, IdentRole, Literal, Operand, Predicate, ProjectedField, Projection,
    QueryVector, RadiusMetres, RowLimit, Search, SearchCriterion, VectorMetric,
};

const SCHEMA: &str = "search_ir_live";
const DIMS: usize = 8;

fn run<F: std::future::Future>(f: F) -> F::Output {
    crate::tests::host::run(f)
}

async fn pool() -> (crate::support::postgres::Postgres, Pool) {
    let postgres = crate::support::postgres::Postgres::start();
    let pool = Pool::connect(&postgres.url(), 2)
        .await
        .expect("connect search fixture");
    (postgres, pool)
}

/// Which server actually answered.
///
/// Printed by every arm that sets up, and taken from `server_version_num`
/// rather than a container tag: a tag names the image someone meant to start,
/// and the number names the process that replied.
async fn report_server(pool: &Pool) -> i32 {
    let rows = pool
        .query("SHOW server_version_num", &[])
        .await
        .expect("server_version_num");
    let version: i32 = rows[0]
        .get::<_, String>(0)
        .parse()
        .expect("server_version_num is an integer");
    let ext = pool
        .query(
            "SELECT extname, extversion FROM pg_extension \
             WHERE extname IN ('vector','postgis') ORDER BY extname",
            &[],
        )
        .await
        .expect("extension probe");
    let listed: Vec<String> = ext
        .iter()
        .map(|r| format!("{}={}", r.get::<_, String>(0), r.get::<_, String>(1)))
        .collect();
    println!(
        "search oracle: server_version_num={version} extensions=[{}]",
        listed.join(" ")
    );
    version
}

/// Create the schema, the extensions and the fixture table.
///
/// The extensions are created rather than probed-and-skipped. A skip here would
/// turn "this server has no pgvector" into a pass, which is the shape that let
/// four gates report a clean tree while examining nothing.
async fn setup(pool: &Pool) -> i32 {
    for extension in ["vector", "postgis"] {
        pool.execute(&format!("CREATE EXTENSION IF NOT EXISTS {extension}"), &[])
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "this suite needs the `{extension}` extension and the server refused to \
                     create it: {e}. Start a server that has both, e.g. pgvector/pgvector:pg17 \
                     with postgresql-17-postgis-3 installed"
                )
            });
    }
    let version = report_server(pool).await;

    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{SCHEMA}\" CASCADE"), &[])
        .await
        .expect("drop schema");
    pool.execute(&format!("CREATE SCHEMA \"{SCHEMA}\""), &[])
        .await
        .expect("create schema");
    pool.execute(
        &format!(
            "CREATE TABLE \"{SCHEMA}\".\"docs\" (
               id text PRIMARY KEY,
               title text NOT NULL,
               tenant_id bigint NOT NULL,
               embedding vector({DIMS}) NOT NULL,
               location geography(POINT, 4326),
               created_at timestamptz NOT NULL DEFAULT NOW(),
               updated_at timestamptz NOT NULL DEFAULT NOW(),
               created_by text,
               updated_by text,
               version bigint NOT NULL DEFAULT 1,
               deleted_at timestamptz
             )"
        ),
        &[],
    )
    .await
    .expect("create table");
    version
}

/// A deterministic unit vector, so a query for row `i`'s own vector has an
/// exact nearest neighbour and every other row is strictly further away.
fn unit_vector(index: usize) -> Vec<f32> {
    let mut values = vec![0.0_f32; DIMS];
    #[allow(clippy::cast_precision_loss)]
    let seed = index as f32;
    for (position, value) in values.iter_mut().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let offset = position as f32;
        *value = (seed * 0.37 + offset * 1.13).sin();
    }
    let norm: f32 = values.iter().map(|v| v * v).sum::<f32>().sqrt();
    for value in &mut values {
        *value /= norm;
    }
    values
}

fn vector_text(values: &[f32]) -> String {
    let mut out = String::from("[");
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&value.to_string());
    }
    out.push(']');
    out
}

/// The seam a consumer of [`zeroship_data_sql::RenderedSql`] must implement:
/// a typed parameter to whatever the driver takes.
///
/// It is written **here** rather than in the IR because it is the driver's
/// half. Parameters are encoded from their types, and binary interpretation
/// belongs to the compiled expression.
///
/// Text format is used because it is the channel the shipped path uses
/// (`query_text_params`, reached from
/// `crates/zeroship-data-orm/src/exec.rs`), so the differential arm below
/// compares two statements over one execution mechanism rather than over two.
fn bind_text(params: &[Literal]) -> Vec<String> {
    params
        .iter()
        .map(|value| match value {
            Literal::Bool(b) => b.to_string(),
            Literal::Int(i) => i.to_string(),
            Literal::Float(f) => f.get().to_string(),
            Literal::Text(t) | Literal::Json(t) => t.clone(),
            Literal::Bytes(b) => {
                let mut out = String::from("\\x");
                for byte in b {
                    out.push_str(&format!("{byte:02x}"));
                }
                out
            }
            Literal::Vector(v) => {
                let elements: Vec<f32> = v.elements().iter().map(|e| e.get()).collect();
                vector_text(&elements)
            }
        })
        .collect()
}

fn column(name: &str) -> Ident {
    Ident::parse_as(name, IdentRole::Column).expect("valid column")
}

fn docs_projection() -> Projection {
    Projection::rows(vec![
        ProjectedField::column(column("id")).expect("identity field"),
        ProjectedField::column(column("title")).expect("field"),
        ProjectedField::column(column("tenant_id")).expect("field"),
    ])
    .expect("row projection")
}

fn docs_search(criterion: SearchCriterion, limit: i64) -> Search {
    Search::builder(
        Ident::parse_as("docs", IdentRole::Collection).expect("collection"),
        criterion,
        docs_projection(),
    )
    .namespace(Ident::parse_as(SCHEMA, IdentRole::Namespace).expect("namespace"))
    .limit(RowLimit::new(limit).expect("limit"))
    .build()
    .expect("buildable")
}

/// Execute a rendered plan and return the `id` column in result order.
async fn ranked_ids(pool: &Pool, plan: &Search) -> Vec<String> {
    let sql = render_search(plan).expect("the postgres backend serves this plan");
    let owned = bind_text(sql.params());
    let params: Vec<&str> = owned.iter().map(String::as_str).collect();
    let rows = pool
        .query_text_params(sql.sql(), &params)
        .await
        .unwrap_or_else(|e| panic!("the IR rendered SQL the server refused: {e}\n{}", sql.sql()));
    rows.iter().map(|r| r.get::<_, String>("id")).collect()
}

/// Seed rows through the **text-inference** channel, not through typed binds.
///
/// This is not a stylistic choice and it is worth recording, because getting it
/// wrong is the first thing that happened here. Binding the vector as a typed
/// `String` against `$4::vector` fails in the driver, not the server:
///
/// ```text
/// ToSql(3), WrongType { postgres: Other { name: "vector", oid: 16389 },
///                       rust: "alloc::string::String" }
/// ```
///
/// PostgreSQL resolves `$4` to `vector` from the cast, the driver then checks
/// its `ToSql` impl against that resolved type, and `String` does not claim to
/// serialise a `vector`. The `vector` OID is per-database - 16389 on this
/// server - so no `ToSql` impl can be written against it in advance.
///
/// That is precisely the constraint
/// [`zeroship_data_sql::render::ValueFormat::vector_placeholder`] documents,
/// observed rather than quoted, and it is why `query_text_params` (an empty OID
/// list, server-side inference) is the channel both this fixture and the
/// shipped path use.
async fn seed_vectors(pool: &Pool, count: usize) {
    for index in 0..count {
        let vector = vector_text(&unit_vector(index));
        let id = format!("doc_{index:03}");
        let title = format!("title {index}");
        let tenant = (index % 2).to_string();
        pool.query_text_params(
            &format!(
                "INSERT INTO \"{SCHEMA}\".\"docs\" (id, title, tenant_id, embedding) \
                 VALUES ($1, $2, $3, $4::vector)"
            ),
            &[&id, &title, &tenant, &vector],
        )
        .await
        .expect("insert");
    }
}

// ---------------------------------------------------------------------------

/// The vector lowering runs, and it ranks.
///
/// The query is row 0's own vector, so its distance is exactly zero and it must
/// come first - a stronger claim than the membership assertion the shipped
/// tests make, and it holds because the fixture vectors are constructed to have
/// no ties.
///
/// The distances are also asserted to be **non-decreasing**, which is the arm
/// that would catch an `ORDER BY` that had silently stopped ordering by the
/// distance at all - the failure a top-k membership check cannot see.
#[test]
fn a_vector_search_ranks_by_distance_on_real_pgvector() {
    crate::tests::host::in_test(|| {
        run(async {
            let (_postgres, pool) = pool().await;
            setup(&pool).await;
            seed_vectors(&pool, 100).await;

            let plan = docs_search(
                SearchCriterion::Vector {
                    column: column("embedding"),
                    query: QueryVector::new(&unit_vector(0)).expect("query vector"),
                    metric: VectorMetric::Cosine,
                },
                10,
            );
            let sql = render_search(&plan).expect("renderable");
            let owned = bind_text(sql.params());
            let params: Vec<&str> = owned.iter().map(String::as_str).collect();
            let rows = pool
                .query_text_params(sql.sql(), &params)
                .await
                .unwrap_or_else(|e| panic!("pgvector refused the IR's SQL: {e}\n{}", sql.sql()));

            assert_eq!(rows.len(), 10, "k is the bound and it is bound as $2");
            assert_eq!(
                rows[0].get::<_, String>("id"),
                "doc_000",
                "the query is row 0's own vector; its distance is zero"
            );

            let distances: Vec<f64> = rows.iter().map(|r| r.get::<_, f64>("_distance")).collect();
            assert!(
                distances[0].abs() < 1e-6,
                "an exact match must have zero cosine distance, got {}",
                distances[0]
            );
            assert!(
                distances.windows(2).all(|w| w[0] <= w[1]),
                "the ORDER BY must actually order: {distances:?}"
            );
            println!("ruled on 10 ranked rows over 100 seeded vectors");
        });
    })
}

/// THE DIFFERENTIAL ARM. The IR and the shipped builder return the same rows in
/// the same order.
///
/// This is what ties a grammar nobody calls yet to the code that is actually
/// dispatched. Both builders are handed the same search; both statements run
/// against the same 100 rows through the same execution channel; the two
/// orderings must match.
///
/// The **control** is the third statement: the same IR plan with a different
/// metric, which must produce a DIFFERENT ordering. Without it, "the two agree"
/// would also be true if both had returned the table in physical order, or if
/// the fixture had made every ordering identical.
#[test]
fn the_ir_and_the_shipped_builder_rank_identically() {
    crate::tests::host::in_test(|| {
        run(async {
            let (_postgres, pool) = pool().await;
            setup(&pool).await;
            seed_vectors(&pool, 100).await;

            let query = unit_vector(7);
            let filter = Predicate::Compare {
                lhs: Operand::column(column("tenant_id")),
                op: CompareOp::Eq,
                rhs: Operand::Lit(Literal::Int(1)),
            };

            // The IR's answer.
            let mut plan_builder = Search::builder(
                Ident::parse_as("docs", IdentRole::Collection).expect("collection"),
                SearchCriterion::Vector {
                    column: column("embedding"),
                    query: QueryVector::new(&query).expect("query vector"),
                    metric: VectorMetric::Cosine,
                },
                docs_projection(),
            )
            .namespace(Ident::parse_as(SCHEMA, IdentRole::Namespace).expect("namespace"))
            .limit(RowLimit::new(10).expect("limit"));
            plan_builder = plan_builder.filter(filter);
            let plan = plan_builder.build().expect("buildable");
            let ir_ids = ranked_ids(&pool, &plan).await;

            // The shipped builder's answer, for the same search. The schema hint is
            // what `PostgresBackend::vector_search` passes it.
            let schema_hint = value!({
                "title": { "type": "string" },
                "tenant_id": { "type": "number" },
                "embedding": { "type": "vector", "vectorDims": DIMS },
            });
            let shipped = zeroship_data_sql::compile::build_vector_search(
                &zeroship_data_sql::SchemaName::new(SCHEMA).expect("fixture schema name"),
                "docs",
                "embedding",
                &query,
                10,
                zeroship_data_sql::descriptors::VectorMetric::Cosine,
                &value!({ "tenant_id": 1 }),
                &schema_hint,
            )
            .expect("the shipped builder accepts this search");
            let shipped_params = &shipped.params;
            let shipped_rows = zeroship_data_orm::backend::postgres::params::query(
                &pool.acquire().await.unwrap(),
                &shipped.sql,
                shipped_params,
            )
            .await
            .expect("the shipped builder's SQL runs");
            let shipped_ids: Vec<String> = shipped_rows
                .iter()
                .map(|r| r.get::<_, String>("id"))
                .collect();

            assert!(
                !ir_ids.is_empty(),
                "the fixture must produce rows, or the comparison below is between two \
             empty lists and holds vacuously"
            );
            assert_eq!(
                ir_ids, shipped_ids,
                "the IR and the shipped builder disagree about the ranking"
            );

            // THE CONTROL. A different metric must reorder, or the agreement above
            // is a property of the fixture rather than of the two builders.
            let l2 = docs_search(
                SearchCriterion::Vector {
                    column: column("embedding"),
                    query: QueryVector::new(&query).expect("query vector"),
                    metric: VectorMetric::InnerProduct,
                },
                10,
            );
            let l2_ids = ranked_ids(&pool, &l2).await;
            assert_ne!(
                ir_ids, l2_ids,
                "cosine and inner product returned the same ordering, so the fixture \
             cannot distinguish a metric and the agreement above proves nothing"
            );

            println!(
                "ruled on {} ranked ids from 2 builders plus 1 control",
                ir_ids.len()
            );
        });
    })
}

/// THE DIVERGENCE ARM, executed on both sides.
///
/// `pgvector` serves inner product; `vec0` does not. The IR represents the
/// metric either way, and the two backends are asserted to **differ**: the same
/// metric that Postgres ranks by here is refused by the SQLite backend with
/// `vector_unsupported_metric`.
///
/// The **control** is what makes this more than a pair of unrelated
/// assertions. `SqliteBackend::vector_search` is called twice on one backend
/// with one fixture - no table, no vec0 relation - changing exactly one
/// variable, the metric:
///
/// * with `InnerProduct` it must fail with `vector_unsupported_metric`;
/// * with `Cosine` it must fail with something ELSE.
///
/// Without the second call, the first would pass just as well if the refusal
/// came from the missing table, and the arm would be reporting a fixture gap as
/// a designed divergence.
#[test]
fn postgres_serves_the_inner_product_that_sqlite_refuses() {
    crate::tests::host::in_test(|| {
        run(async {
            let (_postgres, pool) = pool().await;
            setup(&pool).await;
            seed_vectors(&pool, 20).await;

            // The Postgres half: it executes and it ranks.
            let plan = docs_search(
                SearchCriterion::Vector {
                    column: column("embedding"),
                    query: QueryVector::new(&unit_vector(3)).expect("query vector"),
                    metric: VectorMetric::InnerProduct,
                },
                5,
            );
            let ids = ranked_ids(&pool, &plan).await;
            assert_eq!(
                ids.len(),
                5,
                "pgvector serves inner product through vector_ip_ops"
            );

            // The SQLite half: the same metric, refused.
            use zeroship_data_orm::backend::VectorMetric as BackendMetric;
            use zeroship_data_orm::binding::DbBinding;
            let dir = tempfile::tempdir().expect("tempdir");
            let sqlite = zeroship_data_orm::backend_selection::new_sqlite_backend(
                std::path::PathBuf::from(dir.path()),
                crate::tests::host::isolate_key_source(),
            )
            .expect("open SqliteBackend");

            let refused = sqlite
                .vector_search(
                    None,
                    zeroship_data_orm::search::VectorSearch {
                        binding: &DbBinding::cold_start("search_ir_live"),
                        collection: "docs",
                        column: "embedding",
                        query: &unit_vector(3),
                        k: 5,
                        metric: BackendMetric::InnerProduct,
                        filter: &zeroship_data_sql::value::Value::Null,
                        schema: &zeroship_data_sql::value::Value::Null,
                    },
                )
                .await
                .expect_err("vec0 has no inner-product metric");
            let refused_code = match &refused {
                zeroship_data_orm::error::DbError::Configuration { code, .. } => *code,
                other => panic!("expected a typed Configuration refusal, got {other:?}"),
            };
            assert_eq!(refused_code, "vector_unsupported_metric");

            // THE CONTROL: one variable changed. A supported metric on the same
            // backend with the same (absent) fixture must fail differently, which is
            // what proves the refusal above is keyed to the METRIC and not to the
            // missing relation.
            let other = sqlite
                .vector_search(
                    None,
                    zeroship_data_orm::search::VectorSearch {
                        binding: &DbBinding::cold_start("search_ir_live"),
                        collection: "docs",
                        column: "embedding",
                        query: &unit_vector(3),
                        k: 5,
                        metric: BackendMetric::Cosine,
                        filter: &zeroship_data_sql::value::Value::Null,
                        schema: &zeroship_data_sql::value::Value::Null,
                    },
                )
                .await
                .expect_err("there is no table, so this fails too - but for another reason");
            let other_code = match &other {
                zeroship_data_orm::error::DbError::Configuration { code, .. } => Some(*code),
                _ => None,
            };
            assert_ne!(
                other_code,
                Some("vector_unsupported_metric"),
                "cosine produced the metric refusal too, so the arm above is reporting the \
             missing fixture rather than the divergence: {other:?}"
            );

            // Recorded on the PASSING path, not only in a failure message. What the
            // control produced is the evidence that the refusal above is keyed to
            // the metric, and a reader of a green run should be able to see it
            // without re-deriving it.
            println!(
                "ruled on 1 metric across 2 backends: postgres ranked {} rows by inner \
             product; sqlite refused it as `{refused_code}`; the cosine control on the \
             same backend failed differently ({other_code:?})",
                ids.len()
            );
        });
    })
}

/// The geo lowering runs against real PostGIS, and the coordinate order is the
/// one the plan claims.
///
/// The fixture is what makes this an arm about `ST_MakePoint`'s `(x, y)` order
/// rather than about SQL syntax: the rows are placed so that a **transposed**
/// query point lands in the Southern Ocean and matches nothing. A lowering that
/// wrote latitude first would return zero rows here, not a different ranking.
#[test]
fn a_geo_search_finds_the_near_rows_and_the_coordinate_order_is_load_bearing() {
    crate::tests::host::in_test(|| {
        run(async {
            let (_postgres, pool) = pool().await;
            setup(&pool).await;

            // London, and a point ~2 km away. Transposing either pair gives a
            // latitude near -0.12 and a longitude near 51.5, which is open ocean.
            let places = [
                ("doc_near", 51.5007, -0.1246),
                ("doc_alsonear", 51.5155, -0.1420),
                ("doc_far", 48.8584, 2.2945),
            ];
            for (id, latitude, longitude) in places {
                let id = id.to_string();
                let embedding = vector_text(&unit_vector(0));
                // Longitude first here too: `ST_MakePoint` is `(x, y)`.
                let longitude = longitude.to_string();
                let latitude = latitude.to_string();
                pool.query_text_params(
                    &format!(
                        "INSERT INTO \"{SCHEMA}\".\"docs\" \
                       (id, title, tenant_id, embedding, location) \
                     VALUES ($1, $1, 0, $2::vector, \
                             ST_SetSRID(ST_MakePoint($3, $4), 4326)::geography)"
                    ),
                    &[&id, &embedding, &longitude, &latitude],
                )
                .await
                .expect("insert place");
            }

            let plan = docs_search(
                SearchCriterion::Geo {
                    column: column("location"),
                    point: GeoPoint::new(51.5007, -0.1246).expect("London"),
                    radius: RadiusMetres::new(5_000.0).expect("5 km"),
                },
                10,
            );
            let ids = ranked_ids(&pool, &plan).await;

            assert_eq!(
                ids,
                vec!["doc_near".to_string(), "doc_alsonear".to_string()],
                "ST_DWithin must keep the two London rows and drop Paris, and ST_Distance \
             must rank the exact match first"
            );

            // THE CONTROL for the coordinate order. The transposed point is a valid
            // GeoPoint - both numbers are in range - and it is a different place, so
            // it must match nothing. If the lowering wrote latitude into
            // ST_MakePoint's x slot, THIS is the query that would have returned the
            // London rows.
            let transposed = docs_search(
                SearchCriterion::Geo {
                    column: column("location"),
                    point: GeoPoint::new(-0.1246, 51.5007).expect("a valid, wrong point"),
                    radius: RadiusMetres::new(5_000.0).expect("5 km"),
                },
                10,
            );
            let transposed_ids = ranked_ids(&pool, &transposed).await;
            assert!(
                transposed_ids.is_empty(),
                "a transposed pair is a different place on Earth and must match nothing; \
             it returned {transposed_ids:?}, which means ST_MakePoint got (lat, lng)"
            );

            println!("ruled on 3 placed rows, 1 query and 1 transposition control");
        });
    })
}

/// The bound reaches the server as a parameter, so one statement text serves
/// every `k`.
///
/// Asserted by executing the **same SQL string** twice with different arguments
/// and getting different row counts - which is the property the shipped SQLite
/// arm does not have, formatting `k` into the statement
/// (`crates/zeroship-data-orm/src/backend/sqlite/vector.rs:117`) so every `k`
/// is a distinct statement and a distinct cache entry.
#[test]
fn one_statement_serves_every_k() {
    crate::tests::host::in_test(|| {
        run(async {
            let (_postgres, pool) = pool().await;
            setup(&pool).await;
            seed_vectors(&pool, 50).await;

            let criterion = || SearchCriterion::Vector {
                column: column("embedding"),
                query: QueryVector::new(&unit_vector(0)).expect("query vector"),
                metric: VectorMetric::Cosine,
            };
            let five = render_search(&docs_search(criterion(), 5)).expect("renderable");
            let twenty = render_search(&docs_search(criterion(), 20)).expect("renderable");

            assert_eq!(
                five.sql(),
                twenty.sql(),
                "two values of k must be one statement, or the prepared-statement cache \
             holds one entry per page size"
            );

            let mut counts = Vec::new();
            for rendered in [&five, &twenty] {
                let owned = bind_text(rendered.params());
                let params: Vec<&str> = owned.iter().map(String::as_str).collect();
                let rows = pool
                    .query_text_params(rendered.sql(), &params)
                    .await
                    .expect("runs");
                counts.push(rows.len());
            }
            assert_eq!(
                counts,
                vec![5, 20],
                "one statement, two arguments, two page sizes"
            );
            println!("ruled on 2 executions of 1 statement text");
        });
    })
}

#[allow(unused_imports)]
use zeroship_data_orm::search::Search as _;
