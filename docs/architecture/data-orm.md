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
| `zeroship-data-macros` | Migration-derived collection metadata and Rust model derives. It performs no database I/O. |
| `zeroship-data-v8` | V8 capture and result encoding, isolate composition, and worker lifecycle integration. |

```text
crates/
  zeroship-data-orm/
    src/orm/                 Rust models and codecs
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
source, and the deployment's runtime collection descriptors:

```rust,ignore
use zeroship_data_orm::{ConnectOptions, Database};

let options = ConnectOptions::new(database_url, key_source);
let db = Database::connect(binding, options, collections).await?;
```

Changing the configured PostgreSQL or SQLite URL does not change application
functions taking `&Database`. `Database::from_schema` also accepts an explicitly
registered backend, allowing a host-defined implementation or instrumentation
wrapper. Rust models continue using `schema!`, `FromRow`, `Insertable`, and
`Changeset`. TypeScript continues using `env.db`.

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
descriptor revision, and its schema names the qualified SQL namespace. Control
owns access to platform tables. Workers retain the default per-app role path and
reach Control-owned metadata through authenticated service APIs.

Connection configuration contains credentials and is excluded from Debug output.
Connection setup does not create application tables.
Migration artifacts supply the descriptor and the physical schema.

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

`first` returns an optional model. `count` counts matching visible rows without
applying ordering or page bounds. `include_deleted` opts into rows hidden by the
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

## Explicit joins

`Database::from` builds source-qualified reads from generated entity aliases.
Model projections reuse `FromRow`; a left-joined model uses `Option`:

```rust,ignore
let o = db.entity::<schema::orders::Entity>()?.alias("o")?;
let c = db.entity::<schema::customers::Entity>()?.alias("c")?;
let rows: Vec<(OrderSummary, Option<CustomerSummary>)> = db
    .from(&o)
    .left_join(&c, o.column(schema::orders::customerId)
        .eq_column(c.column(schema::customers::id))?)?
    .select((o.row::<OrderSummary>(), c.optional_row::<CustomerSummary>()))?
    .order_by(o.column(schema::orders::id).asc())
    .limit(page_size)?
    .all().await?;
```

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
Explicit joins preserve row multiplication and paginate joined rows. The SDK's
existing `with` relation loader remains a separate operation.

## Driver contract

`driver::Driver` exposes a configured physical connection source: acquisition
and pool diagnostics. It accepts no application binding or keys and
requires no search, catalog, masking, or change-publication implementation.
`DriverSession` executes SQL with native parameters, returns rows or affected-row
counts, and handles settlement, cancellation, cleanup, and discard.

`executor::ScopedExecutor` resolves the app's physical SQL namespace and applies its authority
on the connection that executes its statements. PostgreSQL uses a transaction
with local role and timeout settings. SQLite attaches the database and selects
its transaction lane before exposing a physical connection source.

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

Mask policy comes from the app's `defineMaskPolicy()` declaration. Bootstrap
installs it in memory for the app-at-deploy binding, including the empty default.
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
not require JSON serialization. Strings and binary buffers remain native;
JSON encoding is reserved for JSON columns and explicit wire contracts. The
implementation still allocates records and futures and copies some inputs.

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
Timestamp precision is reduced to the containing Unix millisecond, including
instants before the epoch. Scalar timestamps accept integral Unix milliseconds
or real ISO calendar timestamps; an omitted timezone means UTC. Their UTC date
must fit the positive `YYYY-MM-DD` calendar. The shared temporal codec validates
writes before protection transforms and rejects malformed storage with column
context. PostgreSQL receives native timestamp binds; SQLite receives canonical
UTC text. Neither path converts caller timestamps through floating-point SQL.
Filters use the same binding codec, keeping the indexed column bare.

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
JSON null is an element when used as an operand; null columns remain null.
The dialect renderer lives in `zeroship_data_orm::sql`.

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
