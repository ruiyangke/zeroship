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

The adapter does not re-export ORM or SQL modules. Integration fixtures import
those crates directly and use the feature-gated `testing` module for isolate
and worker setup. Concrete drivers and migration policy are dev dependencies
only; the boundary gate refuses them in adapter implementation code.
SQL compilation benchmarks live in `zeroship-data-sql`; native row-decoding
benchmarks live in `zeroship-data-orm`.

Run the required database conformance suite with
`tests/run_data_v8_live_suite.sh`. It requires the repository's live PostgreSQL
fixture and exercises SQLite using explicit temporary files.
