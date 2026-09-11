# Data V8 adapter

`zeroship-data-v8` exposes the shared ORM to worker JavaScript through `env.db`.
It captures V8 arguments and request context, prepares ORM operations, and turns
native results into V8 values and promises. `DbPlugin` registers the primitive
with the runtime and installs deployment metadata before app code evaluates.

CRUD, protection, SQL compilation, database drivers, and transaction policy
belong to `zeroship-data-orm` and `zeroship-data-sql`. This adapter owns V8
classes, per-isolate integration, resource composition, and subscription wrappers. The ORM owns subscription
lifecycle; the PostgreSQL relay owns capture and slot cleanup.
Rust applications use `zeroship-data-orm` directly.

The adapter does not re-export ORM or SQL modules. Integration fixtures import
those crates directly and use this crate only for isolate and worker setup.
SQL compilation benchmarks live in `zeroship-data-sql`; native row-decoding
benchmarks live in `zeroship-data-orm`.

Run the required database conformance suite with
`tests/run_data_v8_live_suite.sh`. It requires the repository's live PostgreSQL
fixture and exercises SQLite using explicit temporary files.
