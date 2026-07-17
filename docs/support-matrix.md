# Feature support matrix

> **GENERATED from `crates/zero-migrate/src/model/support.rs`. Do not edit by hand.**
>
> Regenerate with `ZERO_MIGRATE_UPDATE_SUPPORT_MATRIX=1 cargo test -p zero-migrate --lib model::support_matrix::committed_support_matrix_is_current -- --exact`.

`Yes` means the feature's capability decision is supported for that dialect; `No` means it is unsupported for the reason in the linked note.

## Create table

| Feature | PostgreSQL 18 | MySQL 8 | SQLite |
| --- | --- | --- | --- |
| Sequence-backed default | Yes | No[^1] | No[^1] |
| Table-level check constraint | Yes | No[^2] | No[^2] |
| Table-level foreign key | Yes | Yes | Yes |
| Foreign key with no local column | No[^3] | No[^3] | No[^3] |
| Composite foreign key | Yes | Yes | Yes |
| Foreign key referencing a non-`id` column | Yes | Yes | Yes |
| Table-level unique constraint | Yes | Yes | No[^4] |
| Exclusion constraint | Yes | No[^5] | No[^5] |
| Expression index | Yes | No[^6] | Yes |
| Partial index | Yes | No[^7] | Yes |
| Included index columns | Yes | No[^8] | No[^8] |
| Index storage parameters | Yes | No[^9] | No[^9] |
| Index on `ONLY` | Yes | No[^10] | No[^10] |
| Unique index with `NULLS NOT DISTINCT` | Yes | No[^11] | No[^11] |
| Index operator class | Yes | No[^12] | No[^12] |
| Index collation | Yes | No[^13] | No[^13] |
| Non-btree index method | Yes | No[^14] | No[^14] |

## Partition lifecycle

| Feature | PostgreSQL 18 | MySQL 8 | SQLite |
| --- | --- | --- | --- |
| Partition DDL | Yes | Yes | Yes |

## Add column

| Feature | PostgreSQL 18 | MySQL 8 | SQLite |
| --- | --- | --- | --- |
| Sequence-backed default | Yes | No[^1] | No[^1] |

## Create index

| Feature | PostgreSQL 18 | MySQL 8 | SQLite |
| --- | --- | --- | --- |
| Expression index | Yes | No[^6] | Yes |
| Partial index | Yes | No[^7] | Yes |
| Included index columns | Yes | No[^8] | No[^8] |
| Index storage parameters | Yes | No[^9] | No[^9] |
| Index on `ONLY` | Yes | No[^10] | No[^10] |
| Unique index with `NULLS NOT DISTINCT` | Yes | No[^11] | No[^11] |
| Index operator class | Yes | No[^12] | No[^12] |
| Index collation | Yes | No[^13] | No[^13] |
| Non-btree index method | Yes | No[^14] | No[^14] |

## Comment

| Feature | PostgreSQL 18 | MySQL 8 | SQLite |
| --- | --- | --- | --- |
| Comment | Yes | No[^15] | No[^15] |

## Set column type

| Feature | PostgreSQL 18 | MySQL 8 | SQLite |
| --- | --- | --- | --- |
| Custom `USING` expression | No[^16] | No[^16] | No[^16] |

## Set column default

| Feature | PostgreSQL 18 | MySQL 8 | SQLite |
| --- | --- | --- | --- |
| Sequence-backed default | Yes | No[^1] | No[^1] |

## Rename column

| Feature | PostgreSQL 18 | MySQL 8 | SQLite |
| --- | --- | --- | --- |
| Rename-column existence guard | No[^17] | No[^17] | No[^17] |

## Add constraint

| Feature | PostgreSQL 18 | MySQL 8 | SQLite |
| --- | --- | --- | --- |
| Foreign key with no local column | No[^3] | No[^3] | No[^3] |
| Composite foreign key | Yes | Yes | Yes |
| Foreign key referencing a non-`id` column | Yes | Yes | Yes |
| `NOT VALID` constraint | Yes | No[^18] | No[^18] |
| Table-level check constraint | Yes | No[^2] | No[^2] |
| Exclusion constraint | Yes | No[^5] | No[^5] |

## Insert / DML

| Feature | PostgreSQL 18 | MySQL 8 | SQLite |
| --- | --- | --- | --- |
| Insert with `ON CONFLICT` | Yes | Yes | Yes |

## Create view / drop materialized view

| Feature | PostgreSQL 18 | MySQL 8 | SQLite |
| --- | --- | --- | --- |
| Raw view body | Yes | Yes | Yes |
| Materialized view | Yes | No[^19] | No[^19] |
| `CREATE OR REPLACE MATERIALIZED VIEW` | No[^20] | No[^20] | No[^20] |

## Sequence lifecycle

| Feature | PostgreSQL 18 | MySQL 8 | SQLite |
| --- | --- | --- | --- |
| Standalone sequence | Yes | No[^21] | No[^21] |

## Create trigger

| Feature | PostgreSQL 18 | MySQL 8 | SQLite |
| --- | --- | --- | --- |
| Execute a named trigger function | Yes | No[^22] | No[^22] |
| Structured trigger body | No[^23] | Yes | Yes |
| `TRUNCATE` trigger event | Yes | No[^24] | No[^24] |
| Statement-level trigger | Yes | No[^25] | No[^25] |
| Multiple trigger events | Yes | No[^26] | Yes |
| `INSTEAD OF` trigger timing | Yes | No[^27] | Yes |
| Trigger `WHEN` predicate | Yes | No[^28] | Yes |
| Trigger `RAISE IGNORE` | No[^29] | No[^30] | Yes |

## PostgreSQL raw SQL

| Feature | PostgreSQL 18 | MySQL 8 | SQLite |
| --- | --- | --- | --- |
| Raw SQL | Yes | No[^31] | No[^31] |

## Notes

[^1]: nextval sequence defaults are PostgreSQL-only; SQLite/MySQL have no standalone sequences
[^2]: table-level CHECK expression rendering is PostgreSQL-only in the current engine
[^3]: foreign keys need at least one local column
[^4]: SQLite createTable table-level unique constraints are not threaded into the emitter
[^5]: exclusion constraints are PostgreSQL-only in the current engine
[^6]: createIndex expression elements are not supported on MySQL
[^7]: MySQL does not support partial indexes
[^8]: index INCLUDE columns are PostgreSQL-only
[^9]: index WITH storage parameters are PostgreSQL-only
[^10]: CREATE INDEX ON ONLY is PostgreSQL-only
[^11]: UNIQUE INDEX NULLS NOT DISTINCT is PostgreSQL-only (PG 15+)
[^12]: per-column index operator classes are PostgreSQL-only
[^13]: per-column index collations are PostgreSQL-only
[^14]: non-btree index methods are unsupported on SQLite/MySQL
[^15]: COMMENT ON is PostgreSQL-only in the current engine
[^16]: setColumnType.using expression rendering is deferred in the current engine
[^17]: renameColumn ifExists guards cannot be attributed to a single migration unit today
[^18]: NOT VALID online constraint adoption (addForeignKey/addCheck { notValid }) is PostgreSQL-only; SQLite/MySQL have no NOT VALID / VALIDATE CONSTRAINT
[^19]: materialized views are PostgreSQL-only in the current engine
[^20]: Postgres has no CREATE OR REPLACE MATERIALIZED VIEW and the other dialects have no materialized views
[^21]: standalone sequence objects are PostgreSQL-only in the current engine
[^22]: SQLite/MySQL have no CREATE TRIGGER EXECUTE FUNCTION form
[^23]: Postgres triggers must execute a named trigger function
[^24]: SQLite/MySQL have no TRUNCATE trigger event
[^25]: SQLite/MySQL triggers are row-level only
[^26]: MySQL CREATE TRIGGER accepts exactly one trigger event
[^27]: MySQL does not support INSTEAD OF triggers
[^28]: MySQL triggers do not support WHEN predicates
[^29]: Postgres trigger bodies are unsupported; named functions must be used
[^30]: MySQL cannot render RAISE IGNORE
[^31]: pgRaw statements are PostgreSQL-only
