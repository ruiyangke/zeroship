# ORM and driver architecture

Rust applications and worker TypeScript use the same ORM behavior. Application
models, collection operations, and transaction callbacks do not carry a backend
type parameter. The host chooses the database during setup.

```text
Rust models                         Worker TypeScript
     |                                     |
     |                              zeroship-data-v8
     |                              V8 arguments/results
     +------------------+------------------+
                        |
                zeroship-data-orm
                Database / Collection
                        |
             CRUD + protection + search
                        |
                data-orm::sql
                SQL + native parameters
                        |
                ScopedExecutor
                select database + apply authority
                        |
                Driver.acquire()
                owned Session
                 /          \
          PostgresDriver   SqliteDriver
                 |          |
          compio-postgres   SQLite actor
                 |          |
            PostgreSQL    SQLite

Host Backend = scoped executor + catalog + protection + search
               + committed-change source
```

## Crate responsibilities

| Crate | Responsibility |
| --- | --- |
| `zeroship-data-orm` | Public database API, native values, model codecs, SQL compilation, protection, transaction protocol, runtime state, driver contracts, and built-in backend adapters. |
| `zeroship-data-macros` | Native Rust schema declarations and model derives. It performs no database I/O. |
| `zeroship-data-v8` | V8 capture and result encoding, isolate composition, and worker lifecycle integration. |

```text
crates/
  zeroship-data-orm/
    src/orm/                 Rust models and codecs
    src/schema/              native metadata, artifact decoding and validation
    src/value.rs             native values shared with drivers and V8
    src/sql/                 query grammar, storage codecs, SQL compilation
    src/connection/          backend factories and shared local initialization
    src/driver.rs            physical acquisition and session contracts
    src/executor.rs          scoped execution contract
    src/protection/          policy, encryption, masking, unmask authorization
    src/search.rs            ORM search extension contract
    src/cdc/                 change events, capture contracts, broker, query matching
    src/backend/postgres/    PostgreSQL adapter
    src/backend/sqlite/      SQLite adapter
    src/crud/                CRUD orchestration and read/write pipelines
    src/transaction/         shared transaction protocol
  zeroship-data-macros/       schema and mapping derives
  zeroship-data-v8/           V8 adapter
libs/
  compio-postgres/            standalone transport and pool
```

PostgreSQL and SQLite implementations live under the ORM’s `backend` module.
`compio-postgres` is a standalone library with no dependency on the ORM.

The SQL module does no database I/O and receives no application policy or keys.
Native values live in `zeroship_data_orm::value`. The physical schema identity
is `zeroship_core::schema_name::SchemaName`; migration services share that type
without depending on ORM execution. SQL consumers own identifier quoting.

Migration services and the CDC relay retain their process boundaries. The ORM
registration contract grants no DDL, backup, replication, or provisioning power.
Shared CDC contracts live in `zeroship_data_orm::cdc`: `ChangeEvent`, `ChangeOp`,
`ChangeSink`, subscription messages, and process-wide readiness leases. The
broker and read-set matching live under that module. SQLite commit capture and
the PostgreSQL relay client feed the same broker. V8 owns JavaScript wrappers
and isolate cleanup. The bounded transport protocol lives in
`zeroship-data-cdc-wire`, independent of the ORM.

The standalone relay is implemented in `zeroship-data-cdc-server`. Native hosts
can configure `cdc::relay::RelayConfig` with a TLS endpoint and their enrolled
worker `ServiceAuth`, register a broker subscription, and await `spawn` before
reading the initial snapshot. `RelayHandle` controls shutdown and completion.
The client resynchronizes subscriptions after reconnect. A private certificate
authority can be supplied through `with_ca_file` or `with_tls_connector`; otherwise it uses the
host trust store.

Relay notifications carry collection and operation only. PostgreSQL events have
no primary key, changed-column list, or row images, so predicate matching falls
back to collection invalidation. Re-reads still pass through the ORM's normal
access controls. SQLite retains its local row-image matching.

## Setup and application code

The host supplies a validated `DbBinding`, database configuration, a project-key
source, and a native `Schema`:

```rust,ignore
use zeroship_data_orm::{ConnectOptions, Database};

let options = ConnectOptions::new(database_url, key_source);
let db = Database::connect(binding, options, models::schema()).await?;
```

Changing the configured PostgreSQL or SQLite URL does not change application
functions taking `&Database`. `Database::from_schema` also accepts an explicitly
registered backend, allowing a host-defined implementation or instrumentation
wrapper. Rust models continue using `schema!`, `FromRow`, `Insertable`, and
`Changeset`. TypeScript continues using `env.db`.

Rust `schema!` declarations emit native collection metadata without loading files.
Creator hosts decode their runtime artifact with `Schema::from_runtime_descriptor`.
Both enter the same validation and immutable registration path. Column types,
capabilities, generators, references, and protection remain part of entity
compatibility; equivalent artifact spellings normalize at the decoding boundary.
Typed operations check their bound metadata during preparation and again before
execution, including counts, existence checks, and mutations. A pending operation
refuses replaced metadata with `orm_schema_mismatch` before accessing rows.
Physical schema changes remain the migration engine's responsibility.

Native platform services use the same `Database` API without a V8 adapter. The
service database URL authenticates as the service role already provisioned by
the platform schema. `connection_authority` keeps that login authority instead
of deriving a creator role from the bound schema:

```rust,ignore
use zeroship_data_orm::{binding::DbBinding, sql::SchemaName, ConnectOptions, Database};

let db = Database::connect(
    DbBinding::new(
        "zeroship_control",
        schema_revision,
        SchemaName::new("zeroship")?,
    ),
    ConnectOptions::new(control_database_url, project_keys)
        .connection_authority(),
    control_collections,
).await?;
```

The option accepts no role name and grants no privilege. PostgreSQL permissions
come from the URL's login role, such as `zeroship_control`. The ORM still applies
transaction-local statement, lock, and idle limits. Both pooled operations and
explicit transactions use the authority fixed when the backend opens, and that
choice participates in connection identity. A route captured from one backend
therefore cannot settle on a backend opened with another authority.

`DbBinding` remains role-free. For a platform service, its logical id scopes
transactions and in-memory metadata, its deploy token identifies the installed
schema revision, and its schema names the qualified SQL namespace. Control
owns access to platform tables. Workers retain the default per-app role path and
reach Control-owned metadata through authenticated service APIs.

Connection configuration contains credentials and is excluded from Debug output.
Connection setup does not create application tables.
Rust callers declare native schema metadata; creator bundles carry a runtime
descriptor decoded into the same metadata. Migrations create the physical schema.

Every ORM collection declares a required `id` as its sole primary key. Artifact
packing, runtime installation and Rust schema generation validate this contract.
ID values come from the declared generator or explicit input; the ORM injects neither columns
nor generators. Other column names and lifecycle assignments remain
schema-driven. Projections and aggregate results may omit `id`.

Worker hosts supply the ORM factory to the V8 service:

```rust,ignore
use zeroship_data_orm::connection::ConnectionFactory;
use zeroship_data_v8::service::{DbService, DbServiceConfig};

let service = DbService::new(DbServiceConfig {
    connection: ConnectionFactory::for_url(database_url)?,
    project_keys,
    cdc_relay,
    meter,
})?;
let plugin = service.plugin();
```

`ConnectionFactory` validates built-in configuration without opening a database.
It can also wrap a host-defined `BackendFactory`, whose configuration is safe to
share across threads and whose `connect` future produces a local `BackendHandle`.
The factory owns backend selection and pool configuration; the V8 adapter
knows only this contract. Custom factory identities must distinguish credentials
and routing configurations that cannot safely share a backend.

On each worker thread, the ORM's `LocalConnection` shares pending initialization
among callers and caches the resulting backend. Dropping a waiter leaves the
pending open available for another caller to resume. Failed opens preserve their
typed errors and allow a later attempt. Replacing the configured factory installs
a separate local connection, so an older pending open cannot overwrite the new
binding. Reinstalling the same identity preserves the existing connection.
Connection identity and factory internals are opaque in Debug output.

### Usage reporting

The ORM measures database usage at its operation boundaries and reports it,
after an operation succeeds, to the `metrics::UsageSink` attached to the
binding. `metrics` names the reported quantities (`DB_READS`, `DB_WRITES`,
`DB_ROWS_WRITTEN`). The sink decides attribution: the ORM derives none from the
binding and depends on no metering implementation.

- A Rust host attaches a sink with `Database::with_usage_sink`. Transactions
  the database opens report to the same sink.
- A host that captures routes itself passes the sink to
  `CapturedRoute::capture`. The V8 adapter does so for every creator dispatch:
  it wraps `DbServiceConfig::meter` in a `MeterHandle` for the binding's app,
  and refuses a metered binding whose app id is not an app id with
  `invalid_meter_app_id`.
- A binding without a sink reports nothing and is never refused on
  attribution, so a platform binding served on a creator isolate's thread is
  neither billed to that app nor refused.

SQLite opens or creates a filesystem database, for example
`sqlite:.zeroship/dev.sqlite`. Memory selectors, empty paths, and SQLite URI
options are rejected. Tests provide explicit temporary files; the ORM owns no
temporary directory and never removes database files when a backend closes.

## Typed Rust queries

Generated fields build predicates and ordering keys without JSON encoding.
`eq`, `ne`, ordered comparisons, `in_values`, `not_in_values`, `is_null` and
`is_not_null` compose with `and`, `or` and negation. Ordered comparisons require
a comparable logical type; descriptor checks still enforce field capabilities
and protection rules.

```rust,ignore
let posts = db.entity::<schema::posts::Entity>()?;
let rows: Vec<PostSummary> = posts.query()
    .filter(schema::posts::score.gte(Some(minimum_score))?
        .and(schema::posts::title.in_values(allowed_titles)?))
    .order_by(schema::posts::score.desc().nulls_last())
    .order_by(schema::posts::id.asc())
    .limit(page_size)?
    .all().await?;
```

`first` returns an optional model. `count` counts matching visible rows;
`exists` returns a boolean and stops at the first match without reading model
fields. Both ignore ordering and page bounds. Use `entity.exists(filter)` or
`entity.query().filter(filter).exists()`. `include_deleted` opts into rows hidden by the
declared deletion lifecycle. The existing `find(filter, options)` method uses
the same typed query path.

Aliased columns support scalar `select`, optional joined scalars through
`select_optional`, and `count`, `count_distinct`, `sum`, `avg`, `min` and `max`.
`count_rows()` represents `COUNT(*)`; grouping and `having` use the shared
relational compiler.

```rust,ignore
use zeroship_data_orm::orm::count_rows;

let p = posts.alias("p")?;
let totals: Vec<(Option<String>, i64, Option<f64>)> = db.from(&p)
    .group_by(p.column(schema::posts::nickname))
    .having(count_rows().gte(minimum_group_size)?)
    .select((
        p.column(schema::posts::nickname).select(),
        count_rows(),
        p.column(schema::posts::score).avg(),
    ))?
    .all().await?;
```

Counts decode as `i64`. Integer sums decode as `Option<i64>` and numeric sums
and averages as `Option<f64>`. Minima and maxima preserve the column's logical
type. Aggregates other than counts are optional because an empty input yields
SQL null. Aggregate results use native codecs and may omit entity identity.

## Bulk mutations

`updateMany`, `deleteMany`, `restoreMany`, and `purgeMany` return affected-row
counts. Their SQL omits `RETURNING`; `ScopedExecutor::exec` and
`DriverSession::exec` expose the database count without decoding records.
Single-row writes and inserts retain their record-returning paths.

Rust exposes these as `update_many`, `delete_many`, `restore_many` and
`purge_many`. `insert_many` accepts typed insert models and returns decoded
models atomically. Single-row `restore` and `purge` return an optional model.
Typed upsert names its conflict columns explicitly:

```rust,ignore
use zeroship_data_orm::orm::ConflictTarget;

let saved: Post = posts.upsert(
    input,
    ConflictTarget::new(schema::posts::slug),
).await?;
```

Typed patches support `increment`, `decrement`, `multiply`, `push`, `pull`,
and `add_to_set`. They compose with literal assignments and derived changesets:

```rust,ignore
let changes = posts::counter.increment(1_i64)?
    .and(posts::title.set("published")?)?;
let updated: Option<Post> = posts.update(posts::id.eq(post_id)?, changes).await?;
```

`Changeset::into_changes` returns `Patch<Entity>`. Derives and `set` keep
operator-shaped JSON literal; expression methods feed the shared atomic update
planner. Arithmetic uses the column's numeric codec, and array methods use the
schema-declared element codec. Duplicate assignments are refused. Field
capabilities, protection, generators and transaction routing remain enforced by
the ORM.

The conflict target must belong to the entity and match database uniqueness.
Composite unique targets use `ConflictTarget::new(field).and(other_field)`.
These methods share the normal generators, protection passes and transaction
route; they do not expose driver or SQL details to application code.

Ordinary bulk operations affect every matching row. The read-query limit does
not truncate writes, and an operation does not split itself into independently
committed batches. Per-row encrypted updates retain their target cap and atomic
write frame. Hosts retain their existing transaction and statement budgets;
large maintenance jobs should choose explicit batches.

SQLite publishes committed changes through its capture hooks. PostgreSQL's
local fallback emits a collection invalidation for a successful bulk statement
that affected rows, deferred until commit inside a transaction. A connected
relay remains the authoritative PostgreSQL change source. Removing returned
records avoids result-buffer growth; it does not eliminate database locking,
WAL work, or SQLite's bounded CDC buffers.

## Array storage

An array column has a logical element type and a declared physical storage.
JSON storage keeps the array in a JSON document. Native storage uses the
database's own array type and is available for text elements:

```rust,ignore
schema! { pub models { grants {
    #[orm(primary_key)] id: Text,
    #[orm(array_storage = "native")] scopes: Array<Text>,
    #[orm(array_storage = "native")] amr: Nullable<Array<Text>>,
    tags: Array<Text>,
}}}
```

Runtime descriptors spell the same declaration as the migration engine's
`textArray` column type. Native storage on another element type, on a nested
member, or combined with encryption or masking fails schema validation with
`invalid_schema`; the `schema!` macro refuses the same declarations at compile
time.

```text
Array<Text> + storage        PostgreSQL                 SQLite
-------------------------    -----------------------    --------------------
Json (default)               jsonb                      JSON text
Native                       text[]                     JSON text
```

Every array column uses the `sql_types::Array<S>` codec. Models read `Vec<T>`
and nullable columns `Option<Vec<T>>`; writes accept `Vec<T>` or `&[T]` whose
elements encode through `S`. Arrays support `eq`, `ne`, `in_values`,
`not_in_values`, `is_null` and `is_not_null` against whole values, and `push`,
`pull` and `add_to_set` for elements. Equality is exact: element order and
duplicates are significant. Arrays have no ordered comparisons, grouping,
distinct selection or column-to-column comparisons; those do not compile or are
refused before execution.

PostgreSQL binds native arrays in the binary array protocol with an explicit
`text[]` cast, so order, duplicates, empty strings and the text `NULL` reach the
database unchanged. Equality and membership use PostgreSQL array equality.
`push` uses `array_append`, `pull` uses `array_remove`, and `add_to_set` appends
only when `array_position` finds no equal element, leaving existing duplicates in
place. SQLite has no array type, so native declarations keep JSON text, the
structural JSON equality function and the JSON array renderer described below.
Both backends return the same values for the same operations.

Native text elements are strings without NUL characters, and a null element is
refused on write with `invalid_array_element` on both backends. SQL NULL, an
empty array and an array containing the text `NULL` remain distinct. A
PostgreSQL value with a NULL element, more than one dimension or a lower bound
other than one fails to decode with `row_decode_failed` rather than being
flattened. JSON array values bind as JSON text, so a declaration whose storage
differs from the physical column fails at its first bind in either direction.

## Explicit joins

`Database::from` builds source-qualified reads from generated entity aliases.
Model projections reuse `FromRow`; a left-joined model uses `Option`:

```rust,ignore
let o = db.entity::<schema::orders::Entity>()?.alias("o")?;
let c = db.entity::<schema::customers::Entity>()?.alias("c")?;
let rows: Vec<(OrderSummary, Option<CustomerSummary>)> = db
    .from(&o)
    .left_join(&c, o.column(schema::orders::customerId)
        .eq(c.column(schema::customers::id))?)?
    .select((o.row::<OrderSummary>(), c.optional_row::<CustomerSummary>()))?
    .order_by(o.column(schema::orders::id).asc())
    .limit(page_size)?
    .all().await?;
```

Comparisons accept native values or compatible expressions through the same
methods: `field.eq(value)`, `field.eq(other_field)`, and
`column.gte(other_column)`. Unqualified fields must belong to the same entity;
aliased expressions must belong to registered sources on the same database.
`and`, `or`, and `negate` compose predicates without exposing SQL nodes.
The builder retains expression origins for schema and transaction validation.
This follows [Diesel's operand conversion approach](https://diesel.rs/guides/extending-diesel.html).

`orm::ReadQuery` is the structured operation beneath the Rust builder and the
TypeScript adapter. It supports explicit inner and left joins, named scalar
projections, grouping and aggregates. The ORM resolves every source descriptor,
captures the transaction route and records collection read dependencies before
execution yields. Field types and access flags come from the descriptor;
column names do not select a codec.

The SQL module renders qualified expressions and native parameters for PostgreSQL
and SQLite. The ORM restores each projected row's source identity before the
protection and codec passes. An unmatched optional row becomes `None` in Rust
and `null` in TypeScript, including when the selected fields are nullable.
Explicit joins preserve row multiplication and paginate joined rows. Named
relation loading preserves the parent query's rows and pagination.

Entity, joined, and related queries expose `first`, `count`, and `exists`.
`first` keeps ordering and offset. `count` and `exists` ignore ordering and
page bounds, execute a scalar query in the database, and retain scope/schema
validation. Joined counts include row multiplication; grouped counts count
surviving groups after `HAVING`. A selected global aggregate is a result row,
including for empty input. Related-query summaries count parents without
loading relation projections.

Repeated `filter` and `having` calls combine with AND. Ordering accepts
`nulls_first` and `nulls_last`. An alias's `include_deleted()` applies only
to that source, including a joined source's visibility condition.

## Row locks

Typed reads on a transaction handle can take exclusive row locks, held until
the transaction commits or rolls back:

```rust,ignore
db.transaction(|tx| async move {
    let state: Option<CredentialState> = tx.entity::<schema::users::Entity>()?
        .query()
        .filter(schema::users::id.eq(user_id)?)
        .for_update()?
        .first()
        .await?;
    let s = tx.entity::<schema::sessions::Entity>()?.alias("s")?;
    let g = tx.entity::<schema::grants::Entity>()?.alias("g")?;
    let rows = tx.from(&s)
        .inner_join(&g, g.column(schema::grants::id).eq(s.column(schema::sessions::grantId))?)?
        .for_update_of(&s)?
        .select((s.row::<Session>(), g.row::<Grant>()))?
        .all()
        .await?;
    Ok(())
}).await?;
```

`for_update` locks the rows of every source. `for_update_of` names the sources
to lock and can be repeated; the two cannot be mixed on one read. PostgreSQL
renders `FOR UPDATE [OF ...]` after the page bounds, using the aliases the read
already emits, so only the locked sources need `UPDATE` privilege. The strength
is always exclusive and a competing lock waits. There is no `NOWAIT` or
`SKIP LOCKED`: skipping locked rows would silently omit them. Waits are bounded
by the transaction's lock timeout (`budgets::DB_LOCK_TIMEOUT_MS`) and surface as
`lock_not_available`.

Under read committed, a waiter returns the latest committed version of the row
it waited for. Under repeatable read or serializable, locking a row changed
after the transaction's snapshot fails with `serialization_failure`. A lock
taken inside a nested callback whose savepoint rolls back is released with that
savepoint; locks taken in the enclosing frame survive it.

The builders refuse a root handle with `transaction_required`. A root handle
stays a pooled receiver even inside another handle's callback. A lock target
must be a source already registered on the same database handle. Preparation
checks the captured transaction route again and refuses, before any SQL runs,
`count`, `exists`, aggregates, grouping, relation loading, an unqualified lock
on a read with a left join, and a lock on the nullable side of a left join
(`invalid_read`). SQLite has no row locks and refuses locking reads with
`unsupported_backend_feature`; the transaction stays usable. The V8 adapter's
`ReadQuery` decoding has no lock input.

Every ORM read carries a row limit. A lock set larger than one page is taken as
keyset pages in one transaction, in a stable order; each page's locks
accumulate until settlement.

Single-row updates and deletes lock their target through a first-row
subselect, and protected writes probe their targets the same way. These
internal write-target probes render no locking clause on SQLite, whose single
writer serializes writes.

## Named relations

A foreign-key descriptor can carry a logical `relation` name. Authoring declares
it with `.references("users", "id", { relation: "author" })`; the generated
runtime descriptor retains the source field, target collection, target column,
and relation name together. The name is independent of the database constraint
name and cannot collide with another edge or a column in its source collection.

```text
posts.author_id -- relation: author --> users.id

Rust with_related(posts::relations::author) --+
                                             +--> shared ORM loader
JS   with({ author: true }) -----------------+
                                                   |
                                         parent page + reference keys
                                                   |
                                         bounded target queries
                                                   |
                                         target protection pipeline
                                                   |
                                         parent + related record
```

The loader resolves named edges from installed descriptors, captures the route
and read dependencies before yielding, and batches distinct reference values.
Related reads use the parent query's captured transaction route. Outside a
transaction they use the ordinary pool path. A transaction's isolation level
determines visibility across the statements; relation loading does not add a
snapshot guarantee to PostgreSQL's default isolation.

Target rows pass through their own codecs, encryption and masking stages.
Unreadable or protected reference keys cannot be used for loading. A null key
or an absent or deleted target produces `None` in Rust and `null` in JavaScript.
The scalar foreign key remains unchanged. Parent projections can omit that key;
the loader obtains it internally and removes it from the returned projection.

Rust uses generated relation handles and `FromRow` projections, returning parent
and optional target tuples. V8 recursively converts the same protected native
values. The TypeScript SDK maps declared field names and tracks live-query
dependencies; it executes no secondary queries.

This loader handles forward references to unique columns. Reverse collections,
many-to-many loading, nested relations and target projection options remain
future work. Batched reads are the initial execution strategy; a later joined
strategy must preserve the same result and protection contracts.

## Driver contract

`driver::Driver` exposes a configured physical connection source: acquisition
and pool diagnostics. It accepts no application binding or keys and
requires no search, catalog, masking, or change-publication implementation.
`DriverSession` executes SQL with native parameters, returns rows or affected-row
counts, and handles settlement, cancellation, cleanup, and discard.

`executor::ScopedExecutor` resolves the app's physical SQL namespace and applies its authority
on the connection that executes its statements. PostgreSQL uses a transaction
with local role and timeout settings. SQLite attaches the app's database file
and selects its transaction lane before exposing a physical connection source.
A SQLite binding on schema `main` addresses the file the backend opened: it
attaches no `zs-<app>.sqlite`, and its catalog, protection floor and statements
all read that file.

`backend::Backend` is the host registration contract above the driver:

- `ScopedExecutor` supplies routed, authorized statement execution.
- `protection::Catalog` supplies live protection evidence for the bound physical
  schema. Transaction catalog reads reuse the active session and its cancellation
  protocol. Catalog failures remain failures; they cannot become an empty floor.
- `protection::Protection` supplies column keys.
- `search::Search` supplies optional ORM search strategies. Default methods
  reject unsupported operations explicitly.
- The host declares its committed-change source so database hooks and ORM
  publication do not emit the same mutation twice.

A driver author implements connection mechanics. A host integrating that driver
with the multi-tenant ORM supplies the corresponding authority and metadata
services. Neither task changes Rust models or worker TypeScript.

Mask policy comes from the app's `defineMaskPolicy()` declaration. Native plugin
finalization installs it in memory for the app-at-deploy binding, including the empty default.
The declaration is fixed after startup; changing it requires a new deployment.
Neither PostgreSQL nor SQLite persists policy, and unmask authorization never
loads policy from a database or sidecar. Policy installation opens no connection.

`BackendHandle` owns the host registration as an `Rc<dyn Backend>`.
`driver::Session` owns a
`Box<dyn DriverSession>`. Erasure happens at the execution boundary; application
models remain independent of the backend. Async trait futures are local and
boxed at that boundary. This preserves compio's thread-local execution model
without imposing Send or Sync on sessions or V8 state.

Collection compilation returns `sql::compiler::CompiledQuery`. Its Debug output
includes SQL and native parameter types without parameter contents. Execution
borrows the bindings or consumes the output through `into_parts`.

Parameters and result records use native `Value` types. Dynamic dispatch does
not require JSON serialization. Strings, binary buffers and native text arrays
remain native; JSON encoding is reserved for JSON columns and explicit wire
contracts. The implementation still allocates records and futures and copies
some inputs.

The ORM refuses caller-supplied typed-ID assignments before insert, batch insert, or
upsert can mutate rows. Upsert conflict keys must be declared, supplied,
application-owned fields. The SQL compiler rejects malformed or repeated
conflict columns. A conflicting row keeps its identity; a new row gets the
descriptor-declared generated identity. The assignment pass remains idempotent
because input validation runs before it.

Collection descriptors carry assignment generators and explicit primary-key,
concurrency and soft-delete roles. The ORM resolves assignments per collection;
SQL compiles supplied expressions, and V8 forwards operations. Field names alone
never select a generator or lifecycle behavior. Both Rust macros and generated
TypeScript bindings expose declared fields and omit assigned fields from writes.

Native row decoding is fallible. Driver row adapters report `row_decode_failed`
with column context when they reject a result; they never substitute SQL NULL
for a decoding failure. PostgreSQL infinite dates and timestamps are
refused because the native timestamp contract represents finite instants.
An instant is carried as `Value::TimestampMicros`, Unix microseconds, and the
typed Rust API carries it as `UtcInstant` (`from_unix_micros`,
`from_unix_millis`, `unix_micros`, `floor_unix_millis`). A bare `i64` has no
`Timestamp` codec: the unit of an integer is invisible at the call site, and a
JSON or JavaScript number at the same position means milliseconds. Scalar
timestamps also accept a real ISO calendar timestamp; an omitted timezone means
UTC. Their UTC date must fit the positive `YYYY-MM-DD` calendar.

How much of that instant a backend keeps is its registered
`SqlSupport::timestamp_resolution`. PostgreSQL declares microseconds and stores
every one of them: a value bound, returned by `RETURNING`, produced by a
`min`/`max` aggregate or shifted by a `TimestampExpr` offset reads back as the
instant the server holds, so re-binding it matches its own row by equality.
SQLite declares milliseconds, and a finer value is refused with
`ValidationFailed{timestamp_precision_unsupported}` before any statement runs
rather than floored into a different instant. `TimestampExpr` offsets are whole
microseconds; a duration with a finer part is refused where it is written.

The shared temporal codec validates writes before protection transforms and
rejects malformed storage with column context. PostgreSQL receives native
timestamp binds; SQLite receives canonical UTC text. Neither path converts
caller timestamps through floating-point SQL: a PostgreSQL clock offset is bound
as exact decimal seconds, because the portable span in microseconds runs past
the range where a double holds every integer. Filters use the same binding
codec, keeping the indexed column bare.

The V8 adapter keeps JavaScript's millisecond contract in both directions: an
outbound instant floors toward negative infinity, and an inbound number is
scaled. That floor is lossy by design, so a JavaScript caller that reads a
PostgreSQL instant and filters by equality can still miss the row it read.

Temporal fields inside declared objects, union variants, and primitive arrays
follow the same logical contract. The shared codec normalizes them before
protection and JSON storage, and applies the descriptor when binding filters
and decoding results. Array-operation operands use the same normalization.
Ordinary JSON fields remain opaque: their strings and JSON-encoded Dates keep
their JSON representation. Invalid nested temporal writes fail before mutation;
invalid stored values report a column decoding error without exposing contents.

Vector and geographic-point fields return numeric
arrays and latitude/longitude objects on both backends. Their binary storage
encoding stays inside the backend and SQL codecs.

JSON read conversion is dialect-aware. PostgreSQL supplies decoded JSON values;
SQLite supplies encoded JSON text. The SQL codec parses that text once, preserving
JSON strings even when their contents resemble booleans, numbers, or objects.
Malformed stored JSON reports `row_decode_failed` with column context and without
including the stored contents.

Array mutations compile into an atomic SQL update. `$push` appends the operand
as a complete element; `$pull` removes every structurally equal element;
`$addToSet` appends only when no equal element exists. Objects compare without
key order, arrays retain order, and numbers compare by exact decimal value.
In JSON storage, JSON null is an element when used as an operand; native
arrays refuse a null operand. Null columns remain null. The operand is encoded
as one element of the column's storage: JSON for JSON arrays, the element type
for native arrays. The dialect renderer lives in `zeroship_data_orm::sql`.

The SQL module also owns the shared update grammar. It validates assignments
before declared generators and protection transforms, rejecting conflicting writes and
nonnumeric arithmetic operands. Normalization moves literal values under `$set`
so every assigned field passes through the same encryption and masking path.
Explicit `$set` values remain literal JSON even when they contain operator keys.
The compiler consumes parsed assignments instead of selecting an arbitrary
operator from an object. The SDK maps column names without flattening away
document operators or overwriting colliding assignments.

PostgreSQL uses native JSONB equality. SQLite connection setup registers the
deterministic `zeroship_json_equal` SQL function on ordinary and transaction
connections, including replacements after recovery. It uses the SQL module's
`json::comparison_key` and caches the bound operand during an element scan.
This helper receives only JSON text and knows no schema, policy, or application.
A custom SQLite backend using this renderer must install the same function;
the built-in backend handles that setup. Driver session contracts remain SQL
and native parameters.

Calendar dates stay `YYYY-MM-DD` strings through driver reads, Rust model codecs,
and V8. They use positive Gregorian years in that fixed-width form and do not
acquire a time or timezone. The shared calendar codec validates date writes
before protection transforms and rejects malformed stored dates with column
context. The SDK applies the same date rules, including early years and leap days.

Concrete driver access is available for host diagnostics and backend-specific
lifecycle extensions. Shared CRUD, transaction policy, and protection reads do
not downcast to concrete drivers. Registering another execution implementation
requires no new backend enum variant in those paths.

## Session ownership and transactions

Rust callers select isolation through typed transaction options:

```rust,ignore
use zeroship_data_orm::orm::{IsolationLevel, TransactionOptions};

db.transaction_with_options(
    TransactionOptions::default().isolation_level(IsolationLevel::Serializable),
    |tx| async move { save_changes(&tx).await },
).await?;
```

Omitted isolation uses the backend default. SQLite accepts explicit serializable
isolation and rejects the other levels. Nested callbacks inherit their parent's
isolation; passing an explicit level to a savepoint is refused. The callback is
not invoked when its options are refused, and the parent remains usable.

The callback owns its error type. `transaction` and `transaction_with_options`
are generic over it with `E: From<DbError>`, so a host refusal that must roll
back travels out of the transaction as itself rather than through a side
channel: `Err(refusal)` rolls back and returns the refusal, `Ok(Err(refusal))`
commits the work that preceded it. Settlement failures reach the caller through
the same conversion, and a commit whose outcome the protocol could not
establish still arrives as `commit_failed_indeterminate`. Nothing else fixes
`E`, so a callback that only ever fails with `DbError` says so at one of its
`Ok` arms.

```rust,ignore
enum Refusal { Rejected, Database(DbError) }
impl From<DbError> for Refusal { fn from(e: DbError) -> Self { Self::Database(e) } }

let outcome: Result<Result<(), Refusal>, Refusal> = db
    .transaction(|tx| async move {
        record_attempt(&tx).await?;          // DbError becomes Refusal
        Ok(Err(Refusal::Rejected))           // committed: the attempt is kept
    })
    .await;
```

```text
ordinary operation                 explicit transaction
       |                                   |
ScopedExecutor::{query,exec}        ScopedExecutor::open_tx_session
       |                                   |
driver acquires access              owned Session
executor applies authority          parked in transaction lane
       |                                   |
execute and settle                  operation claims session
       |                            executes and returns it
release safely                              |
                                    callback completes
                                            |
                                    settle actual outcome
                                            |
                                    safe reuse or discard
```

PostgreSQL sessions retain owned pool leases. SQLite transaction sessions retain
actor reservations; its ordinary handle obtains a reservation per command.
`LeaseKind::Transaction` requests connection affinity across commands and is
required before issuing transaction SQL. Acquisition itself does not issue BEGIN.
Owned drivers and sessions can outlive the host that opened them. Database file
lifetime is controlled by the host's storage, independently of connection lifetime.
The ORM routes by the dispatch's captured transaction context;
an overlapping ordinary request cannot borrow another callback's session.
Search and raw-column protection reads use the same pinned session as CRUD.
Each opened session is bound to its backend registration; replacing a backend
while a transaction is open cannot redirect its operations to another driver.

The session reports actual settlement as committed, rolled back, or
indeterminate. The transaction reducer decides the response and whether to
publish queued effects. An uncertain result never proves commit. Nested
callbacks use savepoints, and escaped callback handles expire.

### Concurrent lanes and re-entrant transactions

Top-level transactions serialize through one lane per app, and the lane set
belongs to the context. A second top-level transaction on the same handle waits
for the first; `Database::independent()` returns a handle over the same binding,
backend, installed schema, mask policy, protection floors and usage sink whose
lanes are its own, so its transaction is admitted while the original's is open.
Concurrency is then bounded by the backend's connection pool: a fork whose BEGIN
cannot get a connection fails with the pool's acquire timeout. Forks are
distinct handles, so a read source built on one is refused by the other, and a
backend that reserves one transaction connection per app refuses `independent()`
with `unsupported_backend_feature` rather than serializing invisibly. Committed
effects still reach the process broker, because the queue a fork drains on
commit is its lane's and the sink is not.

A root handle that opens a top-level transaction from inside a callback holding
that app's lane is refused with `nested_top_level_transaction`. Waiting there
cannot succeed - the claim is held by the poll that is asking for it - and the
database sees nothing wrong, so the wait used to end only at a caller's timeout.
Nesting through the handle the callback was given still opens a savepoint, and a
fork still opens a concurrent transaction; only the re-entrant root handle is
refused. Locks are not covered by any of this: a fork awaited from inside
another transaction's callback can still block on rows that transaction holds,
and the lock timeout is what ends that.

`Database::check_connection()` confirms the backend is reachable in one round
trip on an autocommit lease. It claims no lane, installs no session authority
and reads no table, so it answers while the handle's own transaction is open,
and its wait is the backend's connection wait.

Dropping a native callback starts supervised cancellation and retains admission
until cleanup settles or withdraws the session. Abandoning a nested callback
cancels its enclosing transaction; returning an error rolls back its savepoint.
An accepted commit remains owned by settlement even if its caller disappears.
During session startup, cancellation may be indeterminate because the backend
has not yet exposed an interrupt handle.

Cancellation authority is captured while the session is available and remains
bound to that lease. An acknowledgement either confirms delivery of an interrupt
or reports completed cleanup. PostgreSQL's cancellation barrier and lease
revocation remain in its driver. SQLite translates its actor's terminal outcome
before returning it to the ORM. The shared protocol sees no vendor outcome type.
Withdrawal consumes the session through `discard`; ordinary Drop must recover or
quarantine unfinished work before physical resources can be reused.

## PostgreSQL coordination

`Database::postgres()` returns PostgreSQL-specific coordination for native Rust
hosts: advisory locks, transaction-local settings and session leases. Other
backends refuse it with `unsupported_backend_feature`. The V8 adapter has no
route to it, and it must stay that way: advisory locks and settings are
server-wide, so creator code must not reach them.

```rust,ignore
use zeroship_data_orm::orm::{AdvisoryKey, TransactionSetting};

db.transaction(|tx| async move {
    let postgres = tx.postgres()?;
    postgres.advisory_xact_lock(AdvisoryKey::hashed_pair(NAMESPACE, user_id)).await?;
    postgres.set_local(&TransactionSetting::new("app_ns.retention")?, "on").await?;
    Ok(())
}).await?;
```

Both commands compile through the handle's SQL registration and run on its
captured transaction route, so they share the transaction's pinned session,
savepoint frames, budgets and supervised cancellation. They carry no data and
emit no usage metrics.

### Advisory transaction locks

`advisory_xact_lock` waits for a transaction-scoped advisory lock and holds it
until the transaction commits or rolls back, including the rollback a dropped
callback performs. Taking the same key again stacks and still releases once.
A lock taken inside a savepoint that rolls back is released with that
savepoint, so a lock that must outlive nested work belongs in the root frame.
The wait is bounded by the transaction's lock timeout; a timeout surfaces as
`lock_not_available` and aborts the transaction.

Keys are PostgreSQL's closed forms, and the database computes every hash:

| Constructor | Rendered key |
| --- | --- |
| `AdvisoryKey::single(i64)` | `$1::int8` |
| `AdvisoryKey::pair(i32, i32)` | `$1::int4, $2::int4` |
| `AdvisoryKey::hashed_pair(i32, text)` | `$1::int4, hashtext($2::text)` |
| `AdvisoryKey::hashed(text)` | `hashtext($1::text)::int8` |
| `AdvisoryKey::hashed_lowercase(text)` | `hashtext(lower($1::text))::int8` |

Hashing and case folding happen in the database, never in Rust, so a caller
that spells the same form in SQL contends on the identical lock. One-argument
and two-argument keys are separate key spaces and never contend with each
other. Transaction locks conflict with session locks other sessions hold on
the same key. A root handle is refused with `transaction_required`.

### Transaction-local settings

`set_local` runs `set_config(name, value, true)` with the name and the value
bound. PostgreSQL reverts the value when the transaction ends and when an
enclosing savepoint rolls back, so it can never reach a pooled session.
`TransactionSetting::new` accepts only two lowercase identifiers joined by a
dot; every built-in setting, including the role and the resource limits the
ORM applies to its sessions, is dotless and therefore unreachable. The host
declares which namespaces its connection may set:

```rust,ignore
ConnectOptions::new(url, keys)
    .connection_authority()
    .transaction_setting_namespace("app_ns")
```

An undeclared namespace, a malformed name, or a value containing NUL is
refused with `invalid_transaction_setting` before any SQL runs. The ORM owns
no setting names; hosts own theirs.

### Session leases

`try_session_lease` takes a session-scoped advisory lock for work that spans
several transactions, such as a fleet-wide sweep. It runs on a root handle
only; a transaction handle is refused with `session_lease_requires_root`.
Acquisition opens a pooled session with the backend's authority and limits,
tries the key once, and commits that short transaction; session locks survive
it. `Ok(None)` means another session holds the key.

The lease pins its session until `release`, which unlocks and returns the
session to the pool. Dropping an unreleased lease discards the session
instead, so the server frees the key when the connection closes and no pooled
session inherits it. A release whose unlock fails, or whose unlock reports the
key was not held, also discards the session and returns an error. A held lease
occupies one pooled connection, so the pool must cover the lease and the
transactions the caller runs beside it.

## SQL portability and extension points

The driver standardizes execution. The SQL compiler owns syntax differences.
A backend pairs execution with an immutable `SqlRegistration` containing its
compiler, storage codecs, effective support, SQL family, and opaque identity.
Adding another SQL language registers another implementation of the shared
statement contract; it does not require a central vendor enum or changes to
application models and transaction policy.

Portable behavior is established by tests, including native types, null/default
handling, projections, commits, rollback, and nested callbacks. SQLite vector SQL is compiled in `zeroship_data_orm::sql` with a native byte
parameter. Search strategies
retain their documented differences in `docs/reference/sqlite-divergences.md`.
Database versions and installed extensions can make a requested feature
unavailable; the backend must report that explicitly.

## Verification

Ordinary tests require real PostgreSQL and SQLite execution. Rust fixtures own
PostgreSQL testcontainers with the required extensions and logical WAL, and
SQLite fixtures own temporary database files. Docker is required.
There is no opt-in live-database feature on the ORM or V8 adapter.

The model contract runs through a host-defined registered backend on both
engines. Integration tests exercise V8 behavior, isolation, search and unmask
transaction routing, cancellation, poisoned transactions, and session cleanup.
Compiler tests validate generated schema and Rust model contracts.

`xtask/tests/data_architecture.rs` checks dependency and plain-driver boundaries,
concrete driver references, shared execution and SQL placement;
`cargo xtask test data` runs the required database tests; and
`cargo clippy --workspace --all-targets --all-features` lints workspace targets.

## Context ownership

`OrmContext` owns runtime descriptors, immutable mask policies, catalog
protection floors, and transaction lanes. A standalone `Database::from_schema`
or `Database::connect` creates its own context. Cloning a `Database` shares its
context; `Database::new` requires an explicit context when composing a handle
from installed metadata.

The context splits those owners in two. Descriptors, policies and floors are
held jointly by a context and every context forked from it, so
`Database::independent()` cannot resolve a different schema, install a second
mask policy, or execute under a weaker protection floor than the handle it came
from. Transaction lanes are the fork's alone, which is what makes admission
independent. Building a second `Database::from_schema` over the same backend
gets independent lanes too, but also a second copy of all three shared owners,
which is why it is not the way to run concurrent transactions.

The V8 host shares a context across dispatches on its worker thread. Metadata
and policy keys include the complete app/deployment/schema binding. Preparing
an operation captures its owner synchronously. Its future enters that context
on each poll and restores the prior context afterwards; cancellation drops its
work under the same owner. Detached transaction timers and recovery tasks
inherit their originating context. This prevents a concurrent database from
redirecting schema lookups, admission, or transaction cleanup.

The thread-local slot selects the currently executing context. It does not own
independent schema, policy, or transaction registries. The registry modules are
internal in shipped builds; the host installs descriptors through
`descriptor::install_collections` and checks transaction state through the
transaction API.

## Physical representations

`zeroship_data_orm::sql::codecs` owns schema-aware boolean lowering, SQLite vector
and geography encoding, and normalization of native result values. The shared
CRUD pipeline invokes these conversions at the appropriate points around
protection transforms without choosing a concrete backend. Namespace selection
belongs to the host executor.

The runtime uses `Catalog` for protection evidence and `Search` for ORM search
strategies. `Driver` and `DriverSession` own physical execution. Conformance
fixtures use a separate test-only `DatabaseFixture` helper with native values.
There is no production text-parameter execution trait or backend DDL type mapper;
the migration engine remains the authority for schema creation.

`xtask/tests/data_architecture.rs` checks source boundaries and dependency
closures, with negative controls for its rejection predicates.
`cargo xtask test data` exercises the
Rust and V8 paths against the required databases.
