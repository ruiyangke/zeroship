# ORM and driver architecture

Rust applications and worker TypeScript use the same ORM behavior. Application
models, collection operations, and transaction callbacks do not carry a backend
type parameter. The host chooses the database during setup.

```text
Rust models                         Worker TypeScript
     |                                     |
     |                              zeroship-plugin-db
     |                                native V8 capture
     +------------------+------------------+
                        |
                zeroship-data-orm
                Database / Collection
                PreparedOperation
                protection and transaction policy
                        |
                zeroship-data-sql
                query compilation + dialect
                        |
                SQL + native parameters
                        |
                registered BackendHandle
                  Driver / owned Session
                   /                \
          PostgreSQL adapter      SQLite adapter
                   |                |
            compio-postgres    rusqlite + session actor
                   |                |
              PostgreSQL          SQLite
```

## Crate responsibilities

| Crate | Responsibility |
| --- | --- |
| `zeroship-data-orm` | Public database API, model codecs, protection, transaction protocol, runtime state, driver contracts, and built-in backend adapters. |
| `zeroship-data-sql` | Native values and records, identifiers, runtime schema metadata, query plans, predicates, and SQL compilation. Its normal dependencies contain no database driver or runtime. |
| `zeroship-data-macros` | Migration-derived collection metadata and Rust model derives. It performs no database I/O. |
| `zeroship-plugin-db` | V8 capture and result encoding, isolate composition, and worker lifecycle integration. |

```text
crates/
  zeroship-data-orm/
    src/orm/                 Rust models and codecs
    src/driver.rs            shared driver and session contracts
    src/backend/postgres/    PostgreSQL adapter
    src/backend/sqlite/      SQLite adapter
    src/crud/                shared protection and CRUD passes
    src/transaction/         shared transaction protocol
  zeroship-data-sql/          plans, native values, SQL dialects
  zeroship-data-macros/       schema and mapping derives
  zeroship-plugin-db/         V8 adapter
libs/
  compio-postgres/            standalone transport and pool
```

The former engine, domain, and vendor crates are consolidated into the ORM.
The query-builder crate is replaced by the SQL crate. PostgreSQL and SQLite
implementations live under the ORM's `backend` module. `compio-postgres` remains
a standalone library with no dependency on the ORM.

Migration services and the CDC relay retain their process boundaries. The ORM
registration contract grants no DDL, backup, replication, or provisioning power.
Existing worker CDC integration remains adapter-owned; this reorganization does
not implement the deferred relay transport or datastore placement system.

## Setup and application code

The host supplies a validated `DbBinding`, database configuration, a column-key
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

Connection configuration contains credentials and is excluded from Debug output.
A connection opens an existing database; it does not create application tables.
Migration artifacts supply the descriptor and the physical schema.

## Driver contract

`driver::Backend` combines focused interfaces:

- `Driver` selects the SQL dialect, prepares access, executes autocommit queries,
  opens transaction sessions, and explicitly declares its change-event source.
- `Catalog` supplies live protection metadata. Catalog failures remain failures;
  an unavailable catalog cannot be treated as an empty protection floor.
- `PolicyStore` stores host-installed policy where the deployment requires it.
- `Search` supplies optional vector and spatial operations. An unsupported
  operation returns an explicit error.

`BackendHandle` owns an `Rc<dyn Backend>`. `driver::Session` owns a
`Box<dyn DriverSession>`. Erasure happens at the execution boundary; application
models remain independent of the backend. Async trait futures are local and
boxed at that boundary. This preserves compio's thread-local execution model
without imposing Send or Sync on sessions or V8 state.

Parameters and result records use native `Value` types. Dynamic dispatch does
not require JSON serialization. Strings and binary buffers remain native;
JSON encoding is reserved for JSON columns and explicit wire contracts. The
implementation still allocates records and futures and copies some inputs.

Concrete driver access is available for host diagnostics and backend-specific
lifecycle extensions. Shared CRUD, transaction policy, and protection reads do
not downcast to concrete drivers. Registering another execution implementation
requires no new backend enum variant in those paths.

## Session ownership and transactions

```text
ordinary operation                 explicit transaction
       |                                   |
Driver::query                      Driver::open_tx_session
       |                                   |
backend acquires access             owned Session
and applies authority                parked in transaction lane
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

PostgreSQL sessions retain owned pool leases. SQLite sessions retain actor
reservations. The ORM routes by the dispatch's captured transaction context;
an overlapping ordinary request cannot borrow another callback's session.
Search and raw-column protection reads use the same pinned session as CRUD.
Each opened session is bound to its backend registration; replacing a backend
while a transaction is open cannot redirect its operations to another driver.

The session reports actual settlement as committed, rolled back, or
indeterminate. The transaction reducer decides the response and whether to
publish queued effects. An uncertain result never proves commit. Nested
callbacks use savepoints, and escaped callback handles expire.

Cancellation authority is captured while the session is available and remains
bound to that lease. An acknowledgement either confirms delivery of an interrupt
or reports completed cleanup. PostgreSQL's cancellation barrier and lease
revocation remain in its driver. SQLite translates its actor's terminal outcome
before returning it to the ORM. The shared protocol sees no vendor outcome type.
Withdrawal consumes the session through `discard`; ordinary Drop must recover or
quarantine unfinished work before physical resources can be reused.

## SQL portability and extension points

The driver standardizes execution. The SQL compiler owns syntax differences.
A backend pairs its execution implementation with a supported `SqlDialect`.
Adding another SQL language extends the SQL compiler; it does not require
rewriting application models or shared transaction policy.

Portable behavior is established by tests, including native types, null/default
handling, projections, commits, rollback, and nested callbacks. Search extensions
retain their documented differences in `docs/reference/sqlite-divergences.md`.
Database versions and installed extensions can make a requested feature
unavailable; the backend must report that explicitly.

## Verification

Ordinary tests require real PostgreSQL and SQLite execution. PostgreSQL must
provide the extensions and WAL configuration required by the database suite.
There is no opt-in live-database feature on the ORM or V8 adapter.

The model contract runs through a host-defined registered backend on both
engines. Integration tests exercise V8 behavior, isolation, search and unmask
transaction routing, cancellation, poisoned transactions, and session cleanup.
Compiler tests validate generated schema and Rust model contracts.

`tests/data_crate_closure_gate.sh` checks dependency boundaries;
`tests/vendor_embedding_gate.sh` checks concrete driver references;
`tests/decision_four_gate.sh` fences shared execution and SQL placement;
`tests/run_plugin_db_live_suite.sh` runs the required database tests; and
`tests/clippy_gate.sh` validates the workspace and its declared feature surface.
