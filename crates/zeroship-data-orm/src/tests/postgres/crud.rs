//! PostgreSQL crud contracts.
use super::fixtures::*;

use crate::tests::fixtures::Host;

use compio_postgres::Pool;

use crate::value::value;

use crate::sql::compile::*;

#[test]
fn insert_and_find() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
            let schema = crate::tests::fixtures::test_app_id!();
            let schema = schema.as_str();
            setup(&pool, schema).await;

            // Insert
            let bq = build_insert(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &notes_schema(),
                &value!({"title": "Hello", "body": "World", "category": "tech"}),
            )
            .unwrap();
            let inserted = exec_mutation(&pool, bq).await;
            assert_eq!(inserted.len(), 1);
            assert_eq!(inserted[0]["title"], "Hello");
            assert_eq!(inserted[0]["body"], "World");
            assert!(inserted[0]["id"].as_i64().unwrap() > 0);

            // Find
            let bq = build_find_with_schema(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &value!({}),
                None,
                None,
                None,
                None,
                &notes_schema(),
            )
            .unwrap();
            let rows = exec_query(&pool, bq).await;
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0]["title"], "Hello");
            release_pg(host, pool).await;
        })
    })
}

#[test]
fn insert_many_round_trip() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
            let schema = crate::tests::fixtures::test_app_id!();
            let schema = schema.as_str();
            setup(&pool, schema).await;

            let docs = value!([
                {"title": "A", "body": "one", "category": "tech"},
                {"title": "B", "body": "two", "category": "food"},
                {"title": "C", "body": "three", "category": "tech"}
            ]);
            let bq = build_insert_many(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &notes_schema(),
                &docs,
            )
            .unwrap();
            let inserted = exec_mutation(&pool, bq).await;
            assert_eq!(inserted.len(), 3);

            // Verify all in DB
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
            let count: i64 = rows[0].get("count");
            assert_eq!(count, 3);
            release_pg(host, pool).await;
        })
    })
}

#[test]
fn update_one_inc() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
            let schema = crate::tests::fixtures::test_app_id!();
            let schema = schema.as_str();
            setup(&pool, schema).await;

            // Insert
            let bq = build_insert(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &notes_schema(),
                &value!({"title": "Counter", "category": "tech", "views": 0}),
            )
            .unwrap();
            exec_mutation(&pool, bq).await;

            // $inc views by 5
            let bq = build_update_one(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &notes_schema(),
                &value!({"title": "Counter"}),
                &value!({"views": {"$inc": 5}}),
            )
            .unwrap();
            let updated = exec_mutation(&pool, bq).await;
            assert_eq!(updated.len(), 1);
            assert_eq!(updated[0]["views"], 5);

            // $inc again
            let bq = build_update_one(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &notes_schema(),
                &value!({"title": "Counter"}),
                &value!({"views": {"$inc": 3}}),
            )
            .unwrap();
            let updated = exec_mutation(&pool, bq).await;
            assert_eq!(updated[0]["views"], 8);
            release_pg(host, pool).await;
        })
    })
}

#[test]
fn update_one_dec_mul() {
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
                &value!({"title": "Math", "category": "tech", "views": 10}),
            )
            .unwrap();
            exec_mutation(&pool, bq).await;

            // $dec
            let bq = build_update_one(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &notes_schema(),
                &value!({"title": "Math"}),
                &value!({"views": {"$dec": 3}}),
            )
            .unwrap();
            let updated = exec_mutation(&pool, bq).await;
            assert_eq!(updated[0]["views"], 7);

            // $mul
            let bq = build_update_one(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &notes_schema(),
                &value!({"title": "Math"}),
                &value!({"views": {"$mul": 2}}),
            )
            .unwrap();
            let updated = exec_mutation(&pool, bq).await;
            assert_eq!(updated[0]["views"], 14);
            release_pg(host, pool).await;
        })
    })
}

#[test]
fn update_one_jsonb_array_ops() {
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
                &value!({"title": "Tags", "category": "tech"}),
            )
            .unwrap();
            exec_mutation(&pool, bq).await;

            // $push "rust"
            let bq = build_update_one(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &notes_schema(),
                &value!({"title": "Tags"}),
                &value!({"tags": {"$push": "rust"}}),
            )
            .unwrap();
            let updated = exec_mutation(&pool, bq).await;
            let tags = updated[0]["tags"].as_array().unwrap();
            assert!(tags.contains(&value!("rust")));

            // $push "go"
            let bq = build_update_one(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &notes_schema(),
                &value!({"title": "Tags"}),
                &value!({"tags": {"$push": "go"}}),
            )
            .unwrap();
            let updated = exec_mutation(&pool, bq).await;
            let tags = updated[0]["tags"].as_array().unwrap();
            assert_eq!(tags.len(), 2);
            assert!(tags.contains(&value!("rust")));
            assert!(tags.contains(&value!("go")));

            // $addToSet "rust" (duplicate — should NOT add)
            let bq = build_update_one(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &notes_schema(),
                &value!({"title": "Tags"}),
                &value!({"tags": {"$addToSet": "rust"}}),
            )
            .unwrap();
            let updated = exec_mutation(&pool, bq).await;
            let tags = updated[0]["tags"].as_array().unwrap();
            assert_eq!(tags.len(), 2); // still 2

            // $addToSet "python" (new — should add)
            let bq = build_update_one(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &notes_schema(),
                &value!({"title": "Tags"}),
                &value!({"tags": {"$addToSet": "python"}}),
            )
            .unwrap();
            let updated = exec_mutation(&pool, bq).await;
            let tags = updated[0]["tags"].as_array().unwrap();
            assert_eq!(tags.len(), 3);

            // $pull "go"
            let bq = build_update_one(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &notes_schema(),
                &value!({"title": "Tags"}),
                &value!({"tags": {"$pull": "go"}}),
            )
            .unwrap();
            let updated = exec_mutation(&pool, bq).await;
            let tags = updated[0]["tags"].as_array().unwrap();
            assert_eq!(tags.len(), 2);
            assert!(!tags.contains(&value!("go")));
            release_pg(host, pool).await;
        })
    })
}

#[test]
fn update_many_round_trip() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
            let schema = crate::tests::fixtures::test_app_id!();
            let schema = schema.as_str();
            setup(&pool, schema).await;

            // Insert 3 tech, 1 food
            let docs = value!([
                {"title": "A", "category": "tech", "views": 0},
                {"title": "B", "category": "tech", "views": 0},
                {"title": "C", "category": "tech", "views": 0},
                {"title": "D", "category": "food", "views": 0}
            ]);
            let bq = build_insert_many(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &notes_schema(),
                &docs,
            )
            .unwrap();
            exec_mutation(&pool, bq).await;

            // Update all tech views +1
            let bq = build_update_many(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &notes_schema(),
                &value!({"category": "tech"}),
                &value!({"views": {"$inc": 1}}),
            )
            .unwrap();
            let affected = zeroship_data_orm::backend::postgres::params::execute(
                &pool.acquire().await.unwrap(),
                &bq.sql,
                &bq.params,
            )
            .await
            .unwrap();
            assert_eq!(affected, 3);

            // Verify food unchanged
            let bq = build_find_with_schema(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &value!({"category": "food"}),
                None,
                None,
                None,
                None,
                &notes_schema(),
            )
            .unwrap();
            let rows = exec_query(&pool, bq).await;
            assert_eq!(rows[0]["views"], 0);

            // Verify tech updated
            let bq = build_find_with_schema(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &value!({"category": "tech"}),
                None,
                None,
                None,
                None,
                &notes_schema(),
            )
            .unwrap();
            let rows = exec_query(&pool, bq).await;
            for row in &rows {
                assert_eq!(row["views"], 1);
            }
            release_pg(host, pool).await;
        })
    })
}

#[test]
fn delete_operations() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
            let schema = crate::tests::fixtures::test_app_id!();
            let schema = schema.as_str();
            setup(&pool, schema).await;

            let docs = value!([
                {"title": "Keep1", "category": "tech"},
                {"title": "Keep2", "category": "tech"},
                {"title": "Del1", "category": "food"},
                {"title": "Del2", "category": "food"},
                {"title": "Del3", "category": "food"}
            ]);
            let bq = build_insert_many(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &notes_schema(),
                &docs,
            )
            .unwrap();
            exec_mutation(&pool, bq).await;

            // Delete one food
            let bq = build_delete_one(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &notes_schema(),
                &value!({"category": "food"}),
            )
            .unwrap();
            let deleted = exec_mutation(&pool, bq).await;
            assert_eq!(deleted.len(), 1);

            // 4 remaining
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
            assert_eq!(rows[0].get::<_, i64>("count"), 4);

            // Delete many remaining food
            let bq = build_delete_many(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &notes_schema(),
                &value!({"category": "food"}),
                SqlDialect::Postgres,
            )
            .unwrap();
            let affected = zeroship_data_orm::backend::postgres::params::execute(
                &pool.acquire().await.unwrap(),
                &bq.sql,
                &bq.params,
            )
            .await
            .unwrap();
            assert_eq!(affected, 2);

            // 2 tech remaining
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
            assert_eq!(rows[0].get::<_, i64>("count"), 2);
            release_pg(host, pool).await;
        })
    })
}

#[test]
fn mixed_update() {
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
                &value!({"title": "Mix", "category": "tech", "views": 10}),
            )
            .unwrap();
            exec_mutation(&pool, bq).await;

            // Update: set category + inc views + push tag
            let bq = build_update_one(
                &crate::sql::SchemaName::new(schema).expect("fixture schema name"),
                "notes",
                &notes_schema(),
                &value!({"title": "Mix"}),
                &value!({"category": "science", "views": {"$inc": 5}, "tags": {"$push": "new"}}),
            )
            .unwrap();
            let updated = exec_mutation(&pool, bq).await;
            assert_eq!(updated[0]["category"], "science");
            assert_eq!(updated[0]["views"], 15);
            let tags = updated[0]["tags"].as_array().unwrap();
            assert!(tags.contains(&value!("new")));
            release_pg(host, pool).await;
        })
    })
}
