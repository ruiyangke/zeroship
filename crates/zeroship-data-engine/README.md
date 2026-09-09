# Runtime ORM

The Rust and worker JavaScript entry points share collection operations,
protection passes, transaction scopes, backend routing, and change publication.
This crate depends on the PostgreSQL and SQLite backends and has no V8 dependency.

The host supplies a `DbBinding`, a `BackendHandle`, and the deployment's runtime
collection descriptors. `Database::new` uses an installed descriptor;
`Database::from_schema` validates and installs field maps. Schema changes and
physical table creation belong to the migration engine and its service.

Rust collection metadata comes from the same migration-generated
`schema.runtime.json` used by the worker. The `schema!` macro reads the artifact
at compile time, with paths relative to the Rust source file, like `include_str!`.
Cargo tracks that artifact as a compilation input. The generated modules contain
collection identities, logical column types, typed fields, and write capabilities.
The macro performs no database I/O.

For a descriptor declaring `posts` with required `title`, nullable `payload`,
and nullable `nickname`, an application can write:

```rust
use zeroship_data_engine::{Database, orm::*};

schema!(pub models = "../generated/zeroship/schema.runtime.json");
use models::posts;

#[derive(Debug, FromRow)]
#[orm(entity = posts)]
struct Post {
    id: String,
    title: String,
    payload: Option<Vec<u8>>,
}

#[derive(Insertable)]
#[orm(entity = posts)]
struct NewPost {
    title: String,
    payload: Option<Vec<u8>>,
}

#[derive(Default, Changeset)]
#[orm(entity = posts)]
struct EditPost {
    title: Change<String>,
    nickname: Change<Option<String>>,
}

async fn publish(db: &Database, payload: Vec<u8>) -> Result<Vec<Post>, DbError> {
    db.transaction(|tx| async move {
        let collection = tx.entity::<posts::Entity>()?;
        let saved: Post = collection.insert(NewPost {
            title: "Hello".into(),
            payload: Some(payload),
        }).await?;

        let _: Option<Post> = collection.update(
            posts::id.eq(saved.id)?,
            EditPost {
                title: Change::Set("Published".into()),
                ..Default::default()
            },
        ).await?;

        collection.find::<Post>(
            posts::title.eq("Published")?, Default::default(),
        ).await
    }).await
}
```

`FromRow<Entity>` is independent of write inputs. A projection can derive it
with only the fields it needs; `find` selects those columns. Use
`#[orm(column = "databaseName")]` when a Rust field has another name. Missing
columns and unsupported Rust type mappings fail compilation against the generated
metadata. Nullable columns require nullable decoders. Missing fields in an actual
result remain errors, rather than being silently filled with Rust defaults.

An entity accepts any `Insertable<Entity>` and `Changeset<Entity>`. Insert derives
check that required fields without database defaults are supplied. System fields
are generated as read-only columns. The runtime still validates every operation,
including operations from handwritten trait implementations.

Write states are explicit:

- `Option::None` supplies SQL NULL to a nullable column.
- `Change::Keep` omits an update field. `Change::Set(None)` writes SQL NULL.
- An insert field with `#[orm(default)]` uses `Defaulted<T>`.
  `Defaulted::Default` omits the field so the database supplies its default;
  `Defaulted::Value(value)` supplies a value. The generated column must permit
  omission. For a nullable defaulted field, use `Defaulted<Option<T>>` to keep
  default and NULL distinct.

The derives call fallible native codecs and include collection and field names
in conversion errors. Owned strings and byte buffers move into records and back
into read models. Borrowed text and bytes are copied into owned records during
preparation. Models need no Serde traits. JSON columns use native `Value`; JSON
encoding happens at the database boundary. Custom domain types can implement
`EncodeValue<sql_types::Text>` and `DecodeValue<sql_types::Text>` (or the matching
logical type) without deriving Serde. `Protected<T>` retains a classified field's
masked display when the protection pipeline withholds its value.

`Database::entity` compares generated field metadata with the installed runtime
descriptor and refuses a mismatch. A typed handle also refuses changes to that
metadata after it was created. This checks the descriptor bound by the host;
physical catalog protection checks remain in the shared protection pipeline.

Transaction callbacks commit on success and roll back on error. Nested callbacks
use savepoints. SQL failures poison the transaction, and cancelled operations
report failure against the session generation that issued them. Collection
handles that escape a completed callback refuse further operations.

Dynamic `Collection` operations remain available for bulk writes, soft deletion,
restoration, aggregation, distinct values, and search. Typed operations use the
same `PreparedOperation` path as those operations and the V8 adapter.

Implementation: `src/orm.rs`, `src/orm/`, `src/crud/`, `src/transaction/`, and
`src/exec.rs`. Macro implementations live in `crates/zeroship-data-macros/`.

Run the engine tests and compiler contracts with:

```sh
cargo test -p zeroship-data-engine --lib
cargo test -p zeroship-data-engine --test derive_contract
```

The engine suite requires PostgreSQL; an unavailable server fails the run.
The live PostgreSQL round trip uses the repository's typed test database
configuration (`PG_TEST_URL` can override it):

```sh
cargo test -p zeroship-data-engine --lib orm::tests::postgres_native_models_round_trip
```

The fixtures render the migration IR into physical tables and check its generated
runtime descriptor against the artifact used by `schema!`.
