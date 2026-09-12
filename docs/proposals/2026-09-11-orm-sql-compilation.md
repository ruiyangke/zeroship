# Shared ORM SQL compilation

**Status: Implemented.**

Runtime SQL construction now has a single owner and a pluggable compiler
registration. Rust models and worker TypeScript share the same ORM behavior.
PostgreSQL is the production backend and file-backed SQLite is the local
development backend.

## Result

The standalone `zeroship-data-sql` crate was merged into
`zeroship-data-orm::sql`. Runtime CRUD, relational reads, aggregates, search,
protection queries, and identity allocation compile through the same resolved
statement grammar. The superseded planning and rendering paths were deleted.

The runtime no longer selects SQL with a closed vendor enum. A backend supplies
an immutable [`SqlRegistration`](../../crates/zeroship-data-orm/src/sql/registration.rs)
containing its compiler, storage codecs, feature support, SQL family, and opaque
identity. A downstream backend can register another implementation without
editing a central vendor list.

This work does not change migration DDL. Migration rendering and execution stay
in the migration crates.

## Architecture

```text
Rust typed API                         TypeScript
      |                                    |
      |                            zeroship-data-v8
      |                         decode/capture native input
      +--------------------+---------------+
                           |
                   zeroship-data-orm
             operation + descriptor resolution
          generators + protection + result layout
                           |
                  resolved Statement
                           |
                   SqlRegistration
             compiler + codecs + support
                  /                    \
       PostgreSQL compiler       SQLite compiler
                  \                    /
                    CompiledQuery
                SQL + native bindings
                           |
                    ScopedExecutor
                app authority + routing
                           |
                     DriverSession
              acquire / query / exec / settle
```

The responsibilities are deliberately asymmetric:

| Layer | Owns |
| --- | --- |
| Rust and V8 adapters | Typed construction or native argument decoding, then calls into the ORM. |
| ORM | Descriptors, access rules, generators, masking, encryption, operation strategy, transaction routing, and result decoding. |
| SQL registration | Pure statement validation, SQL compilation, storage encoding, feature support, and registration identity. |
| Scoped executor | Tenant namespace and authority on the session that executes the statement. |
| Driver | Physical acquisition, native parameter binding, execution, cancellation, settlement, cleanup, and pool diagnostics. |
| Migration engine | DDL, schema differencing, journals, and migration execution. |

The driver has no collection, search, policy, masking, encryption, or upsert
API. Search remains an ORM service because it includes strategy and backend
capability work beyond SQL syntax. The SQL module performs no I/O and receives
no policy objects or encryption keys.

## Crate shape

```text
zeroship-data-orm
  orm/                    Rust public API and models
  crud/                   operation preparation and execution strategy
  sql/statement.rs        resolved statement grammar
  sql/compiler/           shared writer and backend compilers
  sql/registration.rs     compiler, codecs, support, identity
  backend/                built-in ORM host integrations
  driver.rs               plain physical connection contract
  executor.rs             routed and authorized execution
  protection/             mask, encryption, unmask authorization
  cdc/                    ORM change contracts and broker

zeroship-data-macros      generated Rust metadata and model derives
zeroship-data-v8          V8 adapter
zeroship-data-cdc-wire    relay wire contract
zeroship-data-cdc-server  standalone PostgreSQL relay
compio-postgres           standalone PostgreSQL driver and pool
```

`zeroship-data-macros` does not depend on the ORM implementation. Generated
code refers to public ORM paths at the expansion site. `zeroship-data-v8` owns
V8 classes and promise delivery; it does not own query planning. CDC transport
remains separate so the relay does not acquire the ORM execution stack.

The shared physical schema identity lives in `zeroship-core`, allowing migration
and runtime code to use it without creating a migration-to-ORM dependency.

## Statement grammar

[`sql::statement`](../../crates/zeroship-data-orm/src/sql/statement.rs) represents
resolved reads and writes. It covers selects, joins, aggregates, inserts,
updates, deletes, upserts, vector search, spatial search, and identity allocation.
Expressions distinguish:

- validated source-bound columns;
- native bind values with explicit storage types;
- SQL null and database default;
- arithmetic and array assignments;
- supported aggregates and predicates;
- current and incoming values in an upsert conflict action.

There is no caller-supplied raw SQL node. Trusted infrastructure SQL such as
transaction control, role setup, catalog inspection, provisioning, and
replication remains explicit backend or service code.

Statement constructors validate source membership, output-name uniqueness,
clause placement, aggregate portability, conflict targets, storage types, and
operation budgets. SQL null is a predicate or expression node; a JSON null is a
native JSON value. Missing input, SQL null, and database default remain distinct.

Unordered maps and commutative predicates are normalized only where it preserves
meaning. Projection, join, and requested sort order remain caller ordered.
Structural ordering does not depend on bound values, which keeps equivalent
queries stable without changing their semantics.

## Compilation and registration

[`SqlCompiler`](../../crates/zeroship-data-orm/src/sql/compiler/shared.rs) accepts
an owned resolved statement and returns
[`CompiledQuery`](../../crates/zeroship-data-orm/src/sql/compiler/query.rs): SQL
text plus ordered native parameters. Debug output includes SQL and parameter
types while omitting parameter contents. The execution path can borrow bindings
or consume the query to transfer owned buffers.

The compiler-internal writer owns identifier quoting, placeholder allocation,
and statement-wide bind limits. Values never become SQL fragments. The
PostgreSQL and SQLite compiler files own all runtime statement families for
their backend; they are not upsert-only modules.

Each registration contains:

- a `SqlCompiler`;
- `SqlStorageCodecs`;
- compiler-implemented support;
- support available from the connected database;
- an open `SqlFamily` identifier;
- an opaque identity derived from the complete configuration.

Registration construction rejects support that the compiler does not implement.
Preflight checks statement requirements before identity allocation or protected
write reservation. Compilation revalidates the statement, and registration
validates the produced parameter count, including internal identity plans. A
downstream compiler cannot bypass the advertised bind limit by returning an
oversized output.

Captured routes retain the SQL registration identity and connection identity.
Binding refuses a replacement backend, and transaction sessions remain bound to
the driver that opened them. Async initialization cannot silently redirect an
already prepared operation.

## Values and storage codecs

Both public entries use `zeroship_data_orm::value::Value`. Rust typed builders
construct predicates directly; V8 decodes JavaScript values into the same native
representation. Ordinary strings, numbers, timestamps, decimals, records, and
byte buffers do not pass through JSON text serialization.

Storage codecs are part of the SQL registration. They map descriptor-declared
storage types to backend-native values for booleans, JSON, temporal values,
decimals, vectors, geography, arrays, and encrypted bytes. Driver adapters then
perform the final PostgreSQL wire or SQLite binding.

The write path is:

```text
validate logical input and apply declared generators
                         |
               preserve mask source value
                         |
                 mask and/or encrypt
                         |
          resolve descriptor-selected storage inputs
                         |
                registered storage codec
                         |
                 resolved Statement
                         |
                    SQL compiler
```

The read path reverses storage encoding before decrypt, mask wrapping, unmask
authorization, and final projection. Array mutation operands use the same
storage codec path as ordinary writes. JSON null remains an array element while
an explicit field assignment to null remains SQL null.

## Generated identity and upsert

Every collection descriptor requires `id` as its sole primary key. The ORM does
not infer lifecycle or identity behavior from field names. Generators and roles
come from the migration-produced descriptor and generated Rust or TypeScript
bindings.

A database-generated identity is handled by an ORM backend service above the
plain driver. The SQL compiler owns reservation and allocation syntax; the
service owns statement order, returned-value checks, overflow handling, and
session affinity. A normal database-generated insert does not require explicit
ID allocation. Protected writes preallocate only when encryption requires the
final row identity before binding ciphertext.

Upsert is a native statement with an explicit conflict target, insert values,
conflict assignments, an optional conflict condition, and a returning layout.
The existing row keeps its `id` and insert-only fields. Update generators come
from the descriptor. An empty application update uses an explicit self-assignment
so the row-returning contract is preserved.

Encrypted upsert retains the ORM-owned guarded strategy:

```text
preflight support and validate conflict lookup
                         |
                 atomic write frame
                         |
        resolve or allocate candidate identity
                         |
              encrypt for that identity
                         |
       conditional native upsert identity guard
                /                    \
          row returned          guard skipped
               |                      |
       decode and settle     resolve winning identity
                                      |
                         retry from retained plaintext
```

The retry is confined to the guarded protected-write strategy. Nullable conflict
values are refused when the identity probe cannot match database uniqueness
semantics. PostgreSQL locking and SQLite writer reservation remain backend
strategy concerns rather than compiler feature claims.

## Portability

Portable application code never carries a backend type parameter. Portability
comes from the bounded statement grammar, explicit support checks, registered
storage codecs, and database conformance tests. Adding a backend requires a
compiler and codecs for the shared grammar plus an ORM host integration that can
provide routing, authority, catalog protection evidence, search strategy, and
committed-change behavior.

The compiler can refuse a feature it cannot preserve. Adding a driver alone does
not make an ORM backend. Migration support for another vendor does not imply
runtime ORM support for that vendor.

This follows the useful split in established ORMs:

| Reference | Applied lesson |
| --- | --- |
| [Diesel query fragments](https://docs.rs/diesel/latest/diesel/query_builder/trait.QueryFragment.html) | Structured nodes walk a backend compiler; identifier and bind output use separate writer operations. |
| [SQLAlchemy compilation](https://docs.sqlalchemy.org/en/20/core/compiler.html) | Dialect compilation is explicit and unsupported constructs fail at compilation. |
| [Prisma upserts](https://www.prisma.io/docs/orm/v7/reference/prisma-client-reference#database-upserts) | Logical upsert semantics are distinct from connector syntax and fallback strategy. |

Zeroship adapts these ideas to a runtime statement value shared by Rust and V8.
Application APIs therefore stay backend-neutral without erasing backend feature
checks at the compiler boundary.

## Verification

The implementation is protected by:

- statement-constructor and compiler tests for PostgreSQL, SQLite, and a
  downstream registration;
- hostile compiler and codec regressions that exercise support, bind, and value
  boundaries;
- Rust ORM tests against real PostgreSQL and explicit file-backed SQLite;
- V8 adapter tests against the same ORM;
- SDK and bootstrap tests and type checks;
- the data architecture gate and the data xtask suite;
- the production query-build benchmark.

Live PostgreSQL races observe the relevant lock or conflict before releasing a
competitor. File-backed SQLite tests exercise writer reservation, transactions,
and recovery. There is no live-database feature flag or missing-service skip.

## Completed cutover

- [x] Merge SQL and native values into `zeroship-data-orm` and delete the old crate.
- [x] Remove incomplete runtime MySQL compilation while retaining migration support.
- [x] Consolidate compiler output into redacted `CompiledQuery` with native bindings.
- [x] Replace the closed runtime dialect switch with open SQL registration.
- [x] Register compilers, codecs, support, family, and immutable identity together.
- [x] Compile insert, update, delete, lifecycle, upsert, read, join, aggregate, search, protection, and identity statements through registration.
- [x] Move generated identity allocation behind the scoped ORM backend service.
- [x] Enforce capability and bind requirements before protected mutation work.
- [x] Delete the replaced plan, render, and runtime enum paths.
- [x] Exercise downstream registration and built-in backends through conformance tests.
- [x] Point the production benchmark and stable architecture documentation at the final path.

No ORM compilation cache was added. The PostgreSQL driver retains ownership of
its prepared-statement cache. The benchmark measures production compilation so a
future cache can be proposed with an explicit cacheability and invalidation
contract.
