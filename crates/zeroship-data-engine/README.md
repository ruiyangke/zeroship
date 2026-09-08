# Runtime ORM

This crate provides the database API shared by native Rust callers and worker
JavaScript. It owns collection operations, model mapping, protection passes,
transaction scopes, backend routing, and change-event publication. It depends
on the PostgreSQL and SQLite backends and has no V8 dependency.

The host supplies a `DbBinding`, a `BackendHandle`, and the deployment's runtime
descriptor. `Database::new` uses an installed descriptor;
`Database::from_schema` validates and installs collection descriptors. Neither
method creates tables. Schema changes belong to the migration engine and its
service.

Rust applications can map collection rows to their own types:

```rust
use serde::{Deserialize, Serialize};
use serde_json::json;
use zeroship_data_core::error::DbError;
use zeroship_data_engine::{Database, orm::Model};

#[derive(Deserialize)]
struct Post {
    id: String,
    title: String,
}

#[derive(Serialize)]
struct NewPost {
    title: String,
}

impl Model for Post {
    const COLLECTION: &'static str = "posts";
    type Insert = NewPost;
}

async fn publish(db: &Database) -> Result<Vec<Post>, DbError> {
    db.transaction(|tx| async move {
        let posts = tx.model::<Post>()?;
        posts.insert(&NewPost { title: "Hello".into() }).await?;
        posts.find(json!({}), json!({})).await
    }).await
}
```

`Collection` also accepts the dynamic `Operation` vocabulary for bulk writes,
soft deletion, restoration, aggregation, distinct values, and search. Model
mapping uses the same descriptor and protection rules as dynamic operations.

Preparation captures the binding, actor, read dependencies, and transaction
route before execution yields. The V8 adapter calls `PreparedOperation` and
encodes its `Output`; it does not duplicate CRUD orchestration. Filter JSON is
decoded into the query builder's typed predicate grammar before SQL emission.

Transaction callbacks commit on success and roll back on error. Nested callbacks
use savepoints. SQL failures poison the transaction, and cancelled operations
report failure against the session generation that issued them. Collection
handles that escape a completed callback refuse further operations.

The main entry points are `src/orm.rs`, `src/crud/`, `src/transaction/`, and
`src/exec.rs`. Native integration coverage lives in `src/orm/tests.rs`; worker
coverage lives in `crates/zeroship-plugin-db/tests/`.
