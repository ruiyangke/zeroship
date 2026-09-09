# Runtime SQL

Native values and records, validated identifiers, runtime schema metadata,
query plans, predicates, and dialect-aware SQL compilation for the ORM.

This crate has no database driver, pool, actor, V8, or runtime in its normal
dependency graph. Compilation produces SQL and native parameters. Backend
adapters bind those parameters and return native records through the ORM's
execution contracts.

`compile` owns operation builders; `filter`, `plan`, and `render` own typed
query grammar and rendering; `internal` builds protection statements. Catalog
metadata records database evidence without performing I/O. Migration engines
own DDL and schema changes.

Architecture: `docs/architecture/data-orm.md`.
