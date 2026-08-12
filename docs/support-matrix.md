# Feature support matrix

> **GENERATED from `crates/zero-migrate/src/model/support.rs`. Do not edit by hand.**
>
> Regenerate with `ZERO_MIGRATE_UPDATE_SUPPORT_MATRIX=1 cargo test -p zero-migrate --lib model::support_matrix::committed_support_matrix_is_current -- --exact`.

`Yes` means the feature's capability decision is supported for that dialect; `No` means it is unsupported for the reason in the linked note.

## Create table

| Feature | PostgreSQL 18 | MySQL 8 | SQLite |
| --- | --- | --- | --- |
| `ifNotExists`/`ifExists` enforced at apply | Yes | No[^1] | Yes |
| Sequence-backed default | Yes | No[^2] | No[^2] |
| Table-level check constraint | Yes | No[^3] | No[^3] |
| Table-level foreign key | Yes | Yes | Yes |
| Foreign key with no local column | No[^4] | No[^4] | No[^4] |
| Composite foreign key | Yes | Yes | Yes |
| Foreign key referencing a non-`id` column | Yes | Yes | Yes |
| Table-level unique constraint | Yes | Yes | No[^5] |
| Exclusion constraint | Yes | No[^6] | No[^6] |
| Expression index | Yes | No[^7] | Yes |
| Partial index | Yes | No[^8] | Yes |
| Included index columns | Yes | No[^9] | No[^9] |
| Index storage parameters | Yes | No[^10] | No[^10] |
| Index on `ONLY` | Yes | No[^11] | No[^11] |
| Unique index with `NULLS NOT DISTINCT` | Yes | No[^12] | No[^12] |
| Index operator class | Yes | No[^13] | No[^13] |
| Index collation | Yes | No[^14] | No[^14] |
| Non-btree index method | Yes | No[^15] | No[^15] |

## Partition lifecycle

| Feature | PostgreSQL 18 | MySQL 8 | SQLite |
| --- | --- | --- | --- |
| `ifNotExists`/`ifExists` enforced at apply | Yes | No[^1] | Yes |
| Partition DDL | Yes | Yes | Yes |

## Add column

| Feature | PostgreSQL 18 | MySQL 8 | SQLite |
| --- | --- | --- | --- |
| `ifNotExists`/`ifExists` enforced at apply | Yes | No[^1] | Yes |
| Sequence-backed default | Yes | No[^2] | No[^2] |

## Create index

| Feature | PostgreSQL 18 | MySQL 8 | SQLite |
| --- | --- | --- | --- |
| `ifNotExists`/`ifExists` enforced at apply | Yes | No[^1] | Yes |
| Expression index | Yes | No[^7] | Yes |
| Partial index | Yes | No[^8] | Yes |
| Included index columns | Yes | No[^9] | No[^9] |
| Index storage parameters | Yes | No[^10] | No[^10] |
| Index on `ONLY` | Yes | No[^11] | No[^11] |
| Unique index with `NULLS NOT DISTINCT` | Yes | No[^12] | No[^12] |
| Index operator class | Yes | No[^13] | No[^13] |
| Index collation | Yes | No[^14] | No[^14] |
| Non-btree index method | Yes | No[^15] | No[^15] |

## Comment

| Feature | PostgreSQL 18 | MySQL 8 | SQLite |
| --- | --- | --- | --- |
| Comment | Yes | No[^16] | No[^16] |

## Set column type

| Feature | PostgreSQL 18 | MySQL 8 | SQLite |
| --- | --- | --- | --- |
| `ifNotExists`/`ifExists` enforced at apply | Yes | No[^1] | Yes |
| Custom `USING` expression | No[^17] | No[^17] | No[^17] |

## Set column default

| Feature | PostgreSQL 18 | MySQL 8 | SQLite |
| --- | --- | --- | --- |
| `ifNotExists`/`ifExists` enforced at apply | Yes | No[^1] | Yes |
| Sequence-backed default | Yes | No[^2] | No[^2] |

## Rename column

| Feature | PostgreSQL 18 | MySQL 8 | SQLite |
| --- | --- | --- | --- |
| Rename-column existence guard | No[^18] | No[^18] | No[^18] |

## Add constraint

| Feature | PostgreSQL 18 | MySQL 8 | SQLite |
| --- | --- | --- | --- |
| `ifNotExists`/`ifExists` enforced at apply | Yes | No[^1] | Yes |
| Foreign key with no local column | No[^4] | No[^4] | No[^4] |
| Composite foreign key | Yes | Yes | Yes |
| Foreign key referencing a non-`id` column | Yes | Yes | Yes |
| `NOT VALID` constraint | Yes | No[^19] | No[^19] |
| Table-level check constraint | Yes | No[^3] | No[^3] |
| Exclusion constraint | Yes | No[^6] | No[^6] |

## Insert / DML

| Feature | PostgreSQL 18 | MySQL 8 | SQLite |
| --- | --- | --- | --- |
| Insert with `ON CONFLICT` | Yes | Yes | Yes |

## Create view / drop materialized view

| Feature | PostgreSQL 18 | MySQL 8 | SQLite |
| --- | --- | --- | --- |
| Raw view body | Yes | Yes | Yes |
| Materialized view | Yes | No[^20] | No[^20] |
| `CREATE OR REPLACE MATERIALIZED VIEW` | No[^21] | No[^21] | No[^21] |

## Sequence lifecycle

| Feature | PostgreSQL 18 | MySQL 8 | SQLite |
| --- | --- | --- | --- |
| Standalone sequence | Yes | No[^22] | No[^22] |

## Create trigger

| Feature | PostgreSQL 18 | MySQL 8 | SQLite |
| --- | --- | --- | --- |
| Execute a named trigger function | Yes | No[^23] | No[^23] |
| Structured trigger body | No[^24] | Yes | Yes |
| `TRUNCATE` trigger event | Yes | No[^25] | No[^25] |
| Statement-level trigger | Yes | No[^26] | No[^26] |
| Multiple trigger events | Yes | No[^27] | Yes |
| `INSTEAD OF` trigger timing | Yes | No[^28] | Yes |
| Trigger `WHEN` predicate | Yes | No[^29] | Yes |
| Trigger `RAISE IGNORE` | No[^30] | No[^31] | Yes |

## PostgreSQL raw SQL

| Feature | PostgreSQL 18 | MySQL 8 | SQLite |
| --- | --- | --- | --- |
| Raw SQL | Yes | No[^32] | No[^32] |

## Notes

[^1]: MySQL catalog probes enforce presence-only and non-column-type decisions, but any decision requiring column-type equality is refused until modifier-preserving equality is implemented
[^2]: nextval sequence defaults are PostgreSQL-only; SQLite/MySQL have no standalone sequences
[^3]: table-level CHECK expression rendering is PostgreSQL-only in the current engine
[^4]: foreign keys need at least one local column
[^5]: SQLite createTable table-level unique constraints are not threaded into the emitter
[^6]: exclusion constraints are PostgreSQL-only in the current engine
[^7]: createIndex expression elements are not supported on MySQL
[^8]: MySQL does not support partial indexes
[^9]: index INCLUDE columns are PostgreSQL-only
[^10]: index WITH storage parameters are PostgreSQL-only
[^11]: CREATE INDEX ON ONLY is PostgreSQL-only
[^12]: UNIQUE INDEX NULLS NOT DISTINCT is PostgreSQL-only (PG 15+)
[^13]: per-column index operator classes are PostgreSQL-only
[^14]: per-column index collations are PostgreSQL-only
[^15]: non-btree index methods are unsupported on SQLite/MySQL
[^16]: COMMENT ON is PostgreSQL-only in the current engine
[^17]: setColumnType.using expression rendering is deferred in the current engine
[^18]: renameColumn ifExists guards cannot be attributed to a single migration unit today
[^19]: NOT VALID online constraint adoption (addForeignKey/addCheck { notValid }) is PostgreSQL-only; SQLite/MySQL have no NOT VALID / VALIDATE CONSTRAINT
[^20]: materialized views are PostgreSQL-only in the current engine
[^21]: Postgres has no CREATE OR REPLACE MATERIALIZED VIEW and the other dialects have no materialized views
[^22]: standalone sequence objects are PostgreSQL-only in the current engine
[^23]: SQLite/MySQL have no CREATE TRIGGER EXECUTE FUNCTION form
[^24]: Postgres triggers must execute a named trigger function
[^25]: SQLite/MySQL have no TRUNCATE trigger event
[^26]: SQLite/MySQL triggers are row-level only
[^27]: MySQL CREATE TRIGGER accepts exactly one trigger event
[^28]: MySQL does not support INSTEAD OF triggers
[^29]: MySQL triggers do not support WHEN predicates
[^30]: Postgres trigger bodies are unsupported; named functions must be used
[^31]: MySQL cannot render RAISE IGNORE
[^32]: pgRaw statements are PostgreSQL-only
