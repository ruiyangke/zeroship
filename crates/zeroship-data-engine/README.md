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
use zeroship_data_core::error::DbError;
use zeroship_data_engine::{Database, orm::{Model, EncodeRecord, Field, Row, Record}};

struct Post { id: String, title: String }
struct NewPost { title: String }

impl EncodeRecord for NewPost {
    fn into_record(self) -> Record {
        [("title".into(), self.title.into())].into()
    }
}

impl Model for Post {
    const COLLECTION: &'static str = "posts";
    type Insert = NewPost;
    fn from_row(mut row: Row) -> Result<Self, DbError> {
        Ok(Self { id: row.take("id")?, title: row.take("title")? })
    }
}

impl Post {
    const TITLE: Field<Self, String> = Field::new("title");
}

async fn publish(db: &Database) -> Result<Vec<Post>, DbError> {
    db.transaction(|tx| async move {
        let posts = tx.model::<Post>()?;
        posts.insert(NewPost { title: "Hello".into() }).await?;
        posts.find(Post::TITLE.eq("Hello".into()), Default::default()).await
    }).await
}
```

Models require no Serde traits. Inserts move owned fields into a native record;
`Row::take` moves strings and byte buffers into the returned model. `Field` ties
filters and patches to a model and field type; use `try_eq` or `try_set` for
fallible conversions such as finite floating-point values.

`Collection` also accepts the dynamic `Operation` vocabulary for bulk writes,
soft deletion, restoration, aggregation, distinct values, and search. Model
mapping uses the same descriptor and protection rules as dynamic operations.

Preparation captures the binding, actor, read dependencies, and transaction
route before execution yields. The V8 adapter calls `PreparedOperation` and
encodes its `Output`; it does not duplicate CRUD orchestration. Native filter records are
decoded into the query builder's typed predicate grammar before SQL emission.

Transaction callbacks commit on success and roll back on error. Nested callbacks
use savepoints. SQL failures poison the transaction, and cancelled operations
report failure against the session generation that issued them. Collection
handles that escape a completed callback refuse further operations.

The main entry points are `src/orm.rs`, `src/crud/`, `src/transaction/`, and
`src/exec.rs`. Native integration coverage lives in `src/orm/tests.rs`; worker
coverage lives in `crates/zeroship-plugin-db/tests/`.
