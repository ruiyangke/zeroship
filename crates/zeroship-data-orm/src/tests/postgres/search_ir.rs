//! Cross-checks the relational search grammar against the ORM compiler on live PostgreSQL.
//! The same fixtures exercise compiler output and independent relational plans.

#![allow(clippy::items_after_statements)]

use crate::tests::fixtures::Host;

use compio_postgres::Pool;
use crate::sql::render::postgres::render_search;
use crate::value;
use crate::sql::{
    CompareOp, GeoPoint, Ident, IdentRole, Literal, Operand, Predicate, ProjectedField, Projection,
    QueryVector, RadiusMetres, RowLimit, Search, SearchCriterion, VectorMetric,
};

const SCHEMA: &str = "search_ir_live";
const DIMS: usize = 8;

async fn pool() -> (crate::tests::fixtures::postgres::Postgres, Pool) {
    let postgres = crate::tests::fixtures::postgres::Postgres::start();
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
    let rows = crate::backend::postgres::params::query(
        &pool.acquire().await.unwrap(),
        sql.sql(),
        sql.params(),
    )
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
/// [`crate::sql::render::ValueFormat::vector_placeholder`] documents,
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
    Host::test(|host| {
        host.run(async {
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
            let rows = crate::backend::postgres::params::query(
                &pool.acquire().await.unwrap(),
                sql.sql(),
                sql.params(),
            )
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
    Host::test(|host| {
        host.run(async {
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
                "id": {"type":"integer", "primaryKey":true},
                "title": { "type": "string" },
                "tenant_id": { "type": "number" },
                "embedding": { "type": "vector", "vectorDims": DIMS },
            });
            let binding = zeroship_data_orm::binding::DbBinding::cold_start(SCHEMA);
            let registration = zeroship_data_orm::sql::registration::SqlRegistration::builtin(
                crate::sql::compile::SqlDialect::Postgres,
            );
            let shipped = zeroship_data_orm::search::VectorSearch::compile(
                &binding,
                "docs",
                "embedding",
                &query,
                10,
                crate::sql::descriptors::VectorMetric::Cosine,
                &value!({ "tenant_id": 1 }),
                &schema_hint,
                &registration,
            )
            .expect("the ORM compiler accepts this search")
            .query;
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

/// PostgreSQL executes inner-product search; SQLite refuses it during preparation.
#[test]
fn postgres_serves_the_inner_product_that_sqlite_refuses() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, pool) = pool().await;
            setup(&pool).await;
            seed_vectors(&pool, 20).await;

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

            use zeroship_data_orm::backend::VectorMetric as BackendMetric;
            use zeroship_data_orm::binding::DbBinding;
            let dir = tempfile::tempdir().expect("tempdir");
            let sqlite = zeroship_data_orm::backend_selection::new_sqlite_backend(
                std::path::PathBuf::from(dir.path()),
                host.key_source(),
            )
            .expect("open SqliteBackend");
            let binding = DbBinding::cold_start("search_ir_live");
            let schema = crate::value!({
                "id":{"type":"integer","primaryKey":true,"required":true},
                "embedding":{"type":"vector","vectorDims":3}
            });
            let registration = zeroship_data_orm::sql::registration::SqlRegistration::builtin(
                zeroship_data_orm::sql::compile::SqlDialect::Sqlite,
            );
            let refused = zeroship_data_orm::search::VectorSearch::compile(
                &binding,
                "docs",
                "embedding",
                &unit_vector(3),
                5,
                BackendMetric::InnerProduct,
                &crate::value::Value::Null,
                &schema,
                &registration,
            )
            .expect_err("SQLite has no inner-product metric");
            let refused_code = match &refused {
                zeroship_data_orm::error::DbError::Configuration { code, .. } => *code,
                other => panic!("expected a typed Configuration refusal, got {other:?}"),
            };
            assert_eq!(refused_code, "vector_unsupported_metric");

            // A supported metric reaches execution on the same missing fixture.
            let other = sqlite
                .vector_search(
                    None,
                    zeroship_data_orm::search::VectorSearch::compile(
                        &binding,
                        "docs",
                        "embedding",
                        &unit_vector(3),
                        5,
                        BackendMetric::Cosine,
                        &crate::value::Value::Null,
                        &schema,
                        &registration,
                    )
                    .unwrap(),
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

            println!(
                "postgres executed inner-product search; sqlite refused it as \
                 `{refused_code}` and admitted cosine to execution ({other_code:?})"
            );
        });
    })
}

/// The relational geo lowering runs against real PostGIS.
#[test]
fn a_geo_search_finds_the_near_rows_and_the_coordinate_order_is_load_bearing() {
    Host::test(|host| {
        host.run(async {
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
/// (`crates/zeroship-data-orm/src/backend/sqlite/vector.rs`) so every `k`
/// is a distinct statement and a distinct cache entry.
#[test]
fn one_statement_serves_every_k() {
    Host::test(|host| {
        host.run(async {
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
                let rows = crate::backend::postgres::params::query(
                    &pool.acquire().await.unwrap(),
                    rendered.sql(),
                    rendered.params(),
                )
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
