# Runtime ORM

The Rust and worker JavaScript entry points share collection operations,
protection passes, transaction scopes, backend routing, and change publication.
PostgreSQL and SQLite adapters are modules in this crate. The physical driver contract covers connection acquisition and SQL sessions.
Host services supply tenant routing, protection, search, and change publication;
none of these are requirements on a driver. The crate has no V8 dependency.
Architecture and ownership: `docs/architecture/data-orm.md`.

`cdc` owns change events, capture and delivery contracts, subscription messages,
the process broker, and read-set matching. Database adapters and the V8 bridge
use these shared contracts. The relay wire protocol remains a separate crate;
PostgreSQL replication runs in the separate CDC relay service.

Use the `bench_row_decode` and `bench_first_row_or_null` targets with
`cargo bench -p zeroship-data-orm`. They exercise the row codec without
constructing a V8 runtime.

The host supplies a `DbBinding`, a `BackendHandle`, and the deployment's runtime
collection descriptors. `Database::new` takes an explicit `OrmContext` with installed descriptors;
`Database::from_schema` creates an independent context, validates and installs field maps. `Database::connect`
opens the configured backend through `ConnectOptions`, using the same URL grammar
as the worker. Application functions take a backend-independent `&Database`. Schema changes and
physical table creation belong to the migration engine and its service.
Native platform services call `ConnectOptions::connection_authority` with a URL
that authenticates as their provisioned service role. This preserves the login
role while keeping the ORM's transaction-local resource limits. The option does
not accept a role name or grant privileges, and worker connections retain the
default per-app role narrowing.
SQLite requires filesystem storage. Memory selectors and URI options are
rejected; tests create and own their temporary database files explicitly.

Rust collection metadata comes from the same migration-generated
`schema.runtime.json` used by the worker. The `schema!` macro reads the artifact
at compile time, with paths relative to the Rust source file, like `include_str!`.
Cargo tracks that artifact as a compilation input. The generated modules contain
collection identities, logical column types, typed fields, and write capabilities.
The macro performs no database I/O.
The artifact is a build input; a running Rust service does not need the file or
V8. `Database::from_schema` accepts in-memory descriptors, and generated entities
expose their embedded descriptors through `Entity::schema()`.

For a descriptor declaring `posts` with required `title`, nullable `payload`,
and nullable `nickname`, an application can write:

```rust
use zeroship_data_orm::{Database, orm::*};

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
check that required fields without database defaults are supplied. Descriptor-assigned fields
are generated as read-only columns. The runtime still validates every operation,
including operations from handwritten trait implementations.

Changeset records contain literal field assignments. `Change::Set(value)` and
`Field::set(value)` preserve JSON objects as data, including objects with keys
that look like update operators. `Field::eq(value)` compares the complete JSON
value without interpreting its object keys as filter operators.

Native update operators are checked against the installed field descriptor
before row lookup: arithmetic requires a numeric field, and array operations
require an array field. An invalid operation is refused even when no row matches.
Encrypted and masked fields accept literal assignments only; their stored
representation cannot be mutated with arithmetic or array operators.

The shared codec checks array element types on inserts, replacements, and array
operators, including arrays nested in typed objects and unions. Native primitive
arrays are inspected in place. Non-temporal encoded arrays are checked without
rewriting their numeric spellings. Invalid stored array types fail decoding without
including the stored values in the error.

Combine field-builder patches with `first.and(second)?`. The result is fallible:
assigning the same column in both patches returns `invalid_update`. Assignments
to distinct columns move into the combined patch without copying their values.

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

Named schema references generate relation selectors. A forward relation returns
the parent alongside an optional protected target, preserving the scalar foreign
key:

```rust,ignore
let rows: Vec<(Post, Option<User>)> = posts.query()
    .with_related(models::posts::relations::author)
    .all().await?;
```

The shared loader batches target reads on the captured transaction route and
uses database equality when matching keys. The schema declares the name once
on the reference; callers do not repeat the foreign-key column.

Transaction callbacks commit on success and roll back on error. Nested callbacks
use savepoints. SQL failures poison the transaction, and cancelled operations
report failure against the session generation that issued them. Collection
handles that escape a completed callback refuse further operations.

Dynamic `Collection` operations remain available for bulk writes, soft deletion,
restoration, aggregation, distinct values, and search. Typed operations use the
same `PreparedOperation` path as those operations and the V8 adapter.

Implementation: `src/orm.rs`, `src/orm/`, `src/crud/`, `src/transaction/`, and
`src/exec.rs`, `src/executor.rs`, `src/protection/`, and `src/search.rs`. Macro implementations live in `crates/zeroship-data-macros/`.

Run the ORM tests and compiler contracts with:

```sh
cargo test -p zeroship-data-orm --lib
cargo test -p zeroship-data-orm --test derive_contract
```

The ORM suite starts PostgreSQL through an owned testcontainer. Docker is
required; startup failure fails the test. No external database URL is needed:

```sh
cargo test -p zeroship-data-orm --lib orm::tests::postgres_native_models_round_trip
```

The fixtures render the migration IR into physical tables and check its generated
runtime descriptor against the artifact used by `schema!`.

`OrmContext` owns descriptors, immutable startup policies, catalog protection
floors, and transaction lanes. Cloned database handles share that owner;
independently constructed databases do not. Prepared operations and transaction
cleanup retain their originating context across asynchronous work and drop.
The V8 host shares a thread context across its dispatches. Schema and policy
entries are keyed by the complete app/deploy/schema binding.

Native values live in `value`; query grammar, physical codecs, and SQL compilation
live in `sql`. The SQL module performs no database I/O. Its integration contracts
live in `tests/sql`; run them with `cargo test -p zeroship-data-orm --test sql`.
The `bench_query_build` benchmark exercises query construction.
`Catalog` and `Search` are the runtime service contracts. Database contracts
live in `src/tests/postgres/` and `src/tests/sqlite/`, grouped by behavior.
`tests::fixtures::Host::test` passes an explicit fixture owner to the test body;
it owns the runtime, connections and ORM context through teardown. Shared state
setup and tracing capture stay inside `tests::fixtures`. Snapshot fixtures compile under
`#[cfg(test)]`. Application code uses the native driver/session contract.
