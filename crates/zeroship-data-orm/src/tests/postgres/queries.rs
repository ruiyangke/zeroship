//! PostgreSQL queries contracts.
use super::fixtures::*;

use crate::tests::fixtures::Host;

use compio_postgres::Pool;

use crate::value::{Value, value};

use crate::sql::compile::*;

/// The descriptor entry for the `weather` fixture table used by the Postgres
/// docs HAVING example. Aggregate builds its own SELECT from `$group`, so this
/// only has to declare the identifiers the pipeline names.
fn weather_schema() -> Value {
    value!({
        "city": { "type": "string" },
        "temp_lo": { "type": "int" },
        "temp_hi": { "type": "int" },
    })
}

#[test]
fn filter_comparison_operators() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
            let schema = crate::tests::fixtures::test_app_id!();
            let schema = schema.as_str();
            setup(&pool, schema).await;

            let docs = value!([
                {"title": "A", "category": "tech", "views": 10},
                {"title": "B", "category": "tech", "views": 20},
                {"title": "C", "category": "food", "views": 30},
                {"title": "D", "category": "food", "views": 40}
            ]);
            let bq = build_insert_many(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &notes_schema(),
                &docs,
            )
            .unwrap();
            exec_mutation(&pool, bq).await;

            // $gt 25
            let bq = build_find_with_schema(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &value!({"views": {"$gt": 25}}),
                None,
                None,
                None,
                None,
                &notes_schema(),
            )
            .unwrap();
            let rows = exec_query(&pool, bq).await;
            assert_eq!(rows.len(), 2);

            // $lte 20
            let bq = build_find_with_schema(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &value!({"views": {"$lte": 20}}),
                None,
                None,
                None,
                None,
                &notes_schema(),
            )
            .unwrap();
            let rows = exec_query(&pool, bq).await;
            assert_eq!(rows.len(), 2);

            // $in
            let bq = build_find_with_schema(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &value!({"category": {"$in": ["tech", "food"]}}),
                None,
                None,
                None,
                None,
                &notes_schema(),
            )
            .unwrap();
            let rows = exec_query(&pool, bq).await;
            assert_eq!(rows.len(), 4);

            // $nin
            let bq = build_find_with_schema(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &value!({"category": {"$nin": ["food"]}}),
                None,
                None,
                None,
                None,
                &notes_schema(),
            )
            .unwrap();
            let rows = exec_query(&pool, bq).await;
            assert_eq!(rows.len(), 2);

            // $ne
            let bq = build_find_with_schema(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &value!({"category": {"$ne": "food"}}),
                None,
                None,
                None,
                None,
                &notes_schema(),
            )
            .unwrap();
            let rows = exec_query(&pool, bq).await;
            assert_eq!(rows.len(), 2);
            release_pg(host, pool).await;
        })
    })
}

#[test]
fn filter_logical_operators() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
            let schema = crate::tests::fixtures::test_app_id!();
            let schema = schema.as_str();
            setup(&pool, schema).await;

            let docs = value!([
                {"title": "A", "category": "tech", "views": 10},
                {"title": "B", "category": "tech", "views": 50},
                {"title": "C", "category": "food", "views": 10}
            ]);
            let bq = build_insert_many(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &notes_schema(),
                &docs,
            )
            .unwrap();
            exec_mutation(&pool, bq).await;

            // $and: tech AND views > 20
            let bq = build_find_with_schema(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &value!({"$and": [{"category": "tech"}, {"views": {"$gt": 20}}]}),
                None,
                None,
                None,
                None,
                &notes_schema(),
            )
            .unwrap();
            let rows = exec_query(&pool, bq).await;
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0]["title"], "B");

            // $or: tech OR views > 20
            let bq = build_find_with_schema(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &value!({"$or": [{"category": "tech"}, {"views": {"$gt": 20}}]}),
                None,
                None,
                None,
                None,
                &notes_schema(),
            )
            .unwrap();
            let rows = exec_query(&pool, bq).await;
            assert_eq!(rows.len(), 2); // A and B

            // $not: NOT food
            let bq = build_find_with_schema(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &value!({"$not": {"category": "food"}}),
                None,
                None,
                None,
                None,
                &notes_schema(),
            )
            .unwrap();
            let rows = exec_query(&pool, bq).await;
            assert_eq!(rows.len(), 2);
            release_pg(host, pool).await;
        })
    })
}

#[test]
fn filter_pattern_operators() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
            let schema = crate::tests::fixtures::test_app_id!();
            let schema = schema.as_str();
            setup(&pool, schema).await;

            let docs = value!([
                {"title": "Hello World", "category": "tech"},
                {"title": "hello rust", "category": "tech"},
                {"title": "Goodbye", "category": "food"}
            ]);
            let bq = build_insert_many(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &notes_schema(),
                &docs,
            )
            .unwrap();
            exec_mutation(&pool, bq).await;

            // $like (case sensitive)
            let bq = build_find_with_schema(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &value!({"title": {"$like": "Hello%"}}),
                None,
                None,
                None,
                None,
                &notes_schema(),
            )
            .unwrap();
            let rows = exec_query(&pool, bq).await;
            assert_eq!(rows.len(), 1);

            // $ilike (case insensitive)
            let bq = build_find_with_schema(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &value!({"title": {"$ilike": "%hello%"}}),
                None,
                None,
                None,
                None,
                &notes_schema(),
            )
            .unwrap();
            let rows = exec_query(&pool, bq).await;
            assert_eq!(rows.len(), 2);
            release_pg(host, pool).await;
        })
    })
}

#[test]
fn find_with_options() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
            let schema = crate::tests::fixtures::test_app_id!();
            let schema = schema.as_str();
            setup(&pool, schema).await;

            let docs = value!([
                {"title": "C", "category": "tech", "views": 30},
                {"title": "A", "category": "tech", "views": 10},
                {"title": "B", "category": "tech", "views": 20}
            ]);
            let bq = build_insert_many(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &notes_schema(),
                &docs,
            )
            .unwrap();
            exec_mutation(&pool, bq).await;

            // Order by views ASC, limit 2
            let bq = build_find_with_schema(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &value!({}),
                Some(2),
                None,
                Some(&value!({"views": 1})),
                None,
                &notes_schema(),
            )
            .unwrap();
            let rows = exec_query(&pool, bq).await;
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0]["title"], "A");
            assert_eq!(rows[1]["title"], "B");

            // Order by views DESC, limit 1, offset 1
            let bq = build_find_with_schema(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &value!({}),
                Some(1),
                Some(1),
                Some(&value!({"views": -1})),
                None,
                &notes_schema(),
            )
            .unwrap();
            let rows = exec_query(&pool, bq).await;
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0]["title"], "B"); // 2nd highest
            release_pg(host, pool).await;
        })
    })
}

#[test]
fn find_with_projection() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
            let schema = crate::tests::fixtures::test_app_id!();
            let schema = schema.as_str();
            setup(&pool, schema).await;

            let bq = build_insert(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &notes_schema(),
                &value!({"title": "Proj", "body": "secret", "category": "tech"}),
            )
            .unwrap();
            exec_mutation(&pool, bq).await;

            let bq = build_find_with_schema(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &value!({}),
                None,
                None,
                None,
                Some(&value!(["title", "category"])),
                &notes_schema(),
            )
            .unwrap();
            let rows = exec_query(&pool, bq).await;
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0]["title"], "Proj");
            assert_eq!(rows[0]["category"], "tech");
            // Should NOT have body, id, views, etc.
            assert!(rows[0].get("body").is_none());
            assert!(rows[0].get("id").is_none());
            release_pg(host, pool).await;
        })
    })
}

#[test]
fn distinct_values() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
            let schema = crate::tests::fixtures::test_app_id!();
            let schema = schema.as_str();
            setup(&pool, schema).await;

            let docs = value!([
                {"title": "A", "category": "tech"},
                {"title": "B", "category": "tech"},
                {"title": "C", "category": "food"},
                {"title": "D", "category": "science"}
            ]);
            let bq = build_insert_many(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &notes_schema(),
                &docs,
            )
            .unwrap();
            exec_mutation(&pool, bq).await;

            let bq = build_distinct(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                "category",
                &value!({}),
                &notes_schema(),
            )
            .unwrap();
            let rows = exec_query(&pool, bq).await;
            let values: Vec<&str> = rows
                .iter()
                .map(|r| r["category"].as_str().unwrap())
                .collect();
            assert_eq!(values.len(), 3);
            assert!(values.contains(&"tech"));
            assert!(values.contains(&"food"));
            assert!(values.contains(&"science"));

            // Distinct with filter
            let bq = build_distinct(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                "category",
                &value!({"category": {"$ne": "science"}}),
                &notes_schema(),
            )
            .unwrap();
            let rows = exec_query(&pool, bq).await;
            assert_eq!(rows.len(), 2);
            release_pg(host, pool).await;
        })
    })
}

#[test]
fn count_with_filter() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
            let schema = crate::tests::fixtures::test_app_id!();
            let schema = schema.as_str();
            setup(&pool, schema).await;

            let docs = value!([
                {"title": "A", "category": "tech"},
                {"title": "B", "category": "tech"},
                {"title": "C", "category": "food"}
            ]);
            let bq = build_insert_many(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &notes_schema(),
                &docs,
            )
            .unwrap();
            exec_mutation(&pool, bq).await;

            // Count all
            let bq = build_count(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &notes_schema(),
                &value!({}),
            )
            .unwrap();
            let param_refs = &bq.params;
            let rows = zeroship_data_orm::backend::postgres::params::query(
                &pool.acquire().await.unwrap(),
                &bq.sql,
                param_refs,
            )
            .await
            .unwrap();
            assert_eq!(rows[0].get::<_, i64>("count"), 3);

            // Count with filter
            let bq = build_count(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &notes_schema(),
                &value!({"category": "tech"}),
            )
            .unwrap();
            let param_refs = &bq.params;
            let rows = zeroship_data_orm::backend::postgres::params::query(
                &pool.acquire().await.unwrap(),
                &bq.sql,
                param_refs,
            )
            .await
            .unwrap();
            assert_eq!(rows[0].get::<_, i64>("count"), 2);
            release_pg(host, pool).await;
        })
    })
}

#[test]
fn aggregate_full() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
            let schema = crate::tests::fixtures::test_app_id!();
            let schema = schema.as_str();
            setup(&pool, schema).await;

            let docs = value!([
                {"title": "A", "category": "tech", "views": 10},
                {"title": "B", "category": "tech", "views": 20},
                {"title": "C", "category": "tech", "views": 30},
                {"title": "D", "category": "food", "views": 100}
            ]);
            let bq = build_insert_many(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &notes_schema(),
                &docs,
            )
            .unwrap();
            exec_mutation(&pool, bq).await;

            let pipeline = value!([
                {"$match": {"category": "tech"}},
                {"$group": {
                    "by": "category",
                    "cnt": {"$count": true},
                    "total": {"$sum": "views"},
                    "average": {"$avg": "views"},
                    "lo": {"$min": "views"},
                    "hi": {"$max": "views"}
                }},
                {"$sort": {"cnt": -1}}
            ]);
            let bq = build_aggregate(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &pipeline,
                &notes_schema(),
            )
            .unwrap();
            let rows = exec_query(&pool, bq).await;
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0]["category"], "tech");
            assert_eq!(rows[0]["cnt"], 3);
            assert_eq!(rows[0]["total"], 60);
            assert_eq!(rows[0]["lo"], 10);
            assert_eq!(rows[0]["hi"], 30);
            release_pg(host, pool).await;
        })
    })
}

#[test]
fn aggregate_multi_group() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
            let schema = crate::tests::fixtures::test_app_id!();
            let schema = schema.as_str();
            setup(&pool, schema).await;

            let docs = value!([
                {"title": "A", "category": "tech", "body": "rust", "views": 10},
                {"title": "B", "category": "tech", "body": "rust", "views": 20},
                {"title": "C", "category": "tech", "body": "go", "views": 5},
                {"title": "D", "category": "food", "body": "pasta", "views": 50}
            ]);
            let bq = build_insert_many(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &notes_schema(),
                &docs,
            )
            .unwrap();
            exec_mutation(&pool, bq).await;

            let pipeline = value!([
                {"$group": {
                    "by": ["category", "body"],
                    "cnt": {"$count": true}
                }},
                {"$sort": {"cnt": -1}}
            ]);
            let bq = build_aggregate(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &pipeline,
                &notes_schema(),
            )
            .unwrap();
            let rows = exec_query(&pool, bq).await;
            // tech/rust=2, tech/go=1, food/pasta=1
            assert_eq!(rows.len(), 3);
            assert_eq!(rows[0]["cnt"], 2); // highest count first
            release_pg(host, pool).await;
        })
    })
}

#[test]
fn aggregate_having() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
            let schema = crate::tests::fixtures::test_app_id!();
            let schema = schema.as_str();
            setup(&pool, schema).await;

            let docs = value!([
                {"title": "A", "category": "tech", "views": 10},
                {"title": "B", "category": "tech", "views": 20},
                {"title": "C", "category": "tech", "views": 30},
                {"title": "D", "category": "food", "views": 5}
            ]);
            let bq = build_insert_many(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &notes_schema(),
                &docs,
            )
            .unwrap();
            exec_mutation(&pool, bq).await;

            // HAVING with alias → resolved to aggregate expression
            let pipeline = value!([
                {"$group": {
                    "by": "category",
                    "cnt": {"$count": true}
                }},
                {"$having": {"cnt": {"$gt": 1}}},
                {"$sort": {"cnt": -1}}
            ]);
            let bq = build_aggregate(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &pipeline,
                &notes_schema(),
            )
            .unwrap();
            let rows = exec_query(&pool, bq).await;
            // Only tech has count > 1
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0]["category"], "tech");
            assert_eq!(rows[0]["cnt"], 3);
            release_pg(host, pool).await;
        })
    })
}

#[test]
fn null_handling() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
            let schema = crate::tests::fixtures::test_app_id!();
            let schema = schema.as_str();
            setup(&pool, schema).await;

            // Insert with body
            let bq = build_insert(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &notes_schema(),
                &value!({"title": "WithBody", "body": "has content", "category": "tech"}),
            )
            .unwrap();
            exec_mutation(&pool, bq).await;
            // Insert without body (column defaults to NULL)
            let bq = build_insert(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &notes_schema(),
                &value!({"title": "NoBody", "category": "tech"}),
            )
            .unwrap();
            exec_mutation(&pool, bq).await;

            // Find where body IS NULL
            let bq = build_find_with_schema(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &value!({"body": null}),
                None,
                None,
                None,
                None,
                &notes_schema(),
            )
            .unwrap();
            let rows = exec_query(&pool, bq).await;
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0]["title"], "NoBody");

            // Find where body IS NOT NULL
            let bq = build_find_with_schema(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &value!({"body": {"$ne": null}}),
                None,
                None,
                None,
                None,
                &notes_schema(),
            )
            .unwrap();
            let rows = exec_query(&pool, bq).await;
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0]["title"], "WithBody");

            // $exists: true
            let bq = build_find_with_schema(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &value!({"body": {"$exists": true}}),
                None,
                None,
                None,
                None,
                &notes_schema(),
            )
            .unwrap();
            let rows = exec_query(&pool, bq).await;
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0]["title"], "WithBody");
            release_pg(host, pool).await;
        })
    })
}

#[test]
fn timestamps_as_numbers() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
            let schema = crate::tests::fixtures::test_app_id!();
            let schema = schema.as_str();
            setup(&pool, schema).await;

            let bq = build_insert(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &notes_schema(),
                &value!({"title": "Time", "category": "tech"}),
            )
            .unwrap();
            let inserted = exec_mutation(&pool, bq).await;

            let ts = inserted[0]["created_at"].as_i64().unwrap();
            // Should be a reasonable Unix millisecond timestamp (after 2020)
            assert!(ts > 1_577_836_800_000); // 2020-01-01
            assert!(ts < 2_000_000_000_000); // ~2033
            release_pg(host, pool).await;
        })
    })
}

#[test]
fn aggregate_having_postgres_docs_example() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            let schema = crate::tests::fixtures::test_app_id!();
            let schema = schema.as_str();
            let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());

            // The schema is this test's own. It used to be the shared `plugin_db_test`,
            // which this test never created - it inherited whichever sibling had run
            // `setup` most recently, so running it alone failed with `3F000 schema does
            // not exist`.
            pool.execute(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"), &[])
                .await
                .unwrap();
            pool.execute(&format!("CREATE SCHEMA \"{schema}\""), &[])
                .await
                .unwrap();

            // Set up weather table. The seven platform system columns are here for the
            // same reason `notes` carries them: a write's `RETURNING` is now an
            // explicit list of the system columns plus the declared fields, so a table
            // missing them is not a table `insertMany` can write. Omitting them makes
            // the statement fail with `42703 column does not exist` - loudly, which is
            // the whole point of naming columns instead of starring them.
            pool.execute(
                &format!(
                    r#"CREATE TABLE "{schema}"."weather" (
                id SERIAL PRIMARY KEY,
                city TEXT,
                temp_lo INTEGER,
                temp_hi INTEGER,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                created_by TEXT,
                updated_by TEXT,
                version INTEGER NOT NULL DEFAULT 1,
                deleted_at TIMESTAMPTZ
            )"#
                ),
                &[],
            )
            .await
            .unwrap();

            let docs = value!([
                {"city": "San Francisco", "temp_lo": 46, "temp_hi": 50},
                {"city": "San Francisco", "temp_lo": 43, "temp_hi": 57},
                {"city": "San Francisco", "temp_lo": 35, "temp_hi": 65},
                {"city": "Hayward", "temp_lo": 37, "temp_hi": 54},
                {"city": "Hayward", "temp_lo": 38, "temp_hi": 52},
                {"city": "Hayward", "temp_lo": 41, "temp_hi": 55}
            ]);
            let bq = build_insert_many(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "weather",
                &weather_schema(),
                &docs,
            )
            .unwrap();
            exec_mutation(&pool, bq).await;

            // Equivalent of: SELECT city, count(*), max(temp_lo)
            //                FROM weather GROUP BY city HAVING max(temp_lo) < 42
            let pipeline = value!([
                {"$group": {
                    "by": "city",
                    "cnt": {"$count": true},
                    "max_temp": {"$max": "temp_lo"}
                }},
                {"$having": {"max_temp": {"$lt": 42}}}
            ]);
            let bq = build_aggregate(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "weather",
                &pipeline,
                &weather_schema(),
            )
            .unwrap();

            // Verify SQL has the resolved expression, not the alias
            assert!(
                bq.sql.contains("HAVING MAX(\"temp_lo\") < $"),
                "sql: {}",
                bq.sql
            );

            let rows = exec_query(&pool, bq).await;

            // Only Hayward has max(temp_lo) = 41 < 42
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0]["city"], "Hayward");
            assert_eq!(rows[0]["cnt"], 3);
            assert_eq!(rows[0]["max_temp"], 41);
            release_pg(host, pool).await;
        })
    })
}
