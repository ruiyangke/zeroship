# Data V8 adapter

`zeroship-data-v8` exposes the shared ORM to worker JavaScript through `env.db`.
It captures V8 arguments and request context, prepares ORM operations, and turns
native results into V8 values and promises. `DbPlugin` registers the primitive
with the runtime and installs deployment metadata before app code evaluates.

CRUD, protection, SQL compilation, database drivers, and transaction policy
belong to `zeroship-data-orm` and `zeroship-data-sql`. This adapter owns V8
classes, per-isolate integration, and subscription wrappers. The ORM owns
subscription lifecycle; the PostgreSQL relay owns capture and slot cleanup.
Rust applications use `zeroship-data-orm` directly.

The host passes an ORM `ConnectionFactory` to `DbServiceConfig`. The adapter
binds that factory to its worker thread through the ORM's `LocalConnection`.
URL validation, backend selection, pool configuration, and concurrent lazy
opening all stay in the ORM. Hosts can inject a custom `BackendFactory` without
changing V8 bindings or application code.

Adapter tests live in `src/tests/postgres/` and `src/tests/sqlite/`, grouped by
behavior. Private setup and parity fixtures live in `src/tests/fixtures/`. Query
recording wraps the public backend interface; persisted SQLite values are
inspected independently with the SQLite driver. Engine contracts live in the
ORM crate. Concrete drivers and migration policy are dev dependencies only;
the boundary gate refuses them in adapter implementation code.
SQL compilation benchmarks live in `zeroship-data-sql`; native row-decoding
benchmarks live in `zeroship-data-orm`.

Run the required database conformance suite with `cargo xtask test data`.
The Rust runner invokes nextest against the data crates, including the real CDC
relay. Each PostgreSQL test owns its container; SQLite uses explicit temporary files.
See `xtask/README.md` for prerequisites and focused test runs.
