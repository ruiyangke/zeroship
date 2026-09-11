# Declared columns and assignment generators

Status: superseded by the implemented descriptor contract. The current behavior
is documented in [Column assignments](../reference/db.md#column-assignments)
and the [ORM architecture](../architecture/data-orm.md).

Fields supplied by migration policy are ordinary declared columns. Their names
have no intrinsic behavior: `created_at`, `deleted_at`, `version` and `id` only
acquire behavior through the same metadata any other column can carry. The
runtime does not load a separate field charter or inject a field-name list.

The migration renderer carries effective assignments into `schema.runtime.json`.
The ORM reads `assign: { by, on }` to compute values and `primaryKey`,
`concurrency` and `softDelete` to resolve operation roles. For example, a key
named `record_key`, a counter named `revision` and a deletion marker named
`retired_on` work through those declarations. An assignment controls the value;
an ordinary default allows the caller to supply a value instead.

Generated fields remain part of the declared read surface. Generated Rust and
TypeScript write types exclude assigned fields. At runtime, the input boundary
rejects caller-supplied generated identifiers; update preparation rejects
insert-fixed and delete-assigned fields and removes caller values for fields
reassigned on writes.

The ORM generates typed identifiers and supplies request actors. Database
defaults initialize timestamps, counters and identity columns. Write expressions
use the database clock and the declared increment step. Anonymous writes supply
a null actor. A typed identifier uses its own field's `idPrefix` when present,
otherwise a prefix derived from the collection name; both paths are validated.
The scalar type alone does not request a generator.

Delete assignments fire on soft deletion. Restore clears delete assignments and
runs write assignments. Without a declared soft-delete role, delete physically
removes the row. Optimistic concurrency uses the declared concurrency column
when the filter supplies its expected value.

Implementation:

- [Assignment resolution](../../crates/zeroship-data-orm/src/assignments.rs),
  `AssignmentPlan::from_schema` and `write_assignments`.
- [CRUD assignment preparation](../../crates/zeroship-data-orm/src/crud/assignment_pass.rs),
  including prefix validation and assignment events.
- [Write input validation](../../crates/zeroship-data-orm/src/crud/write_pipeline.rs).
- [SQL column roles](../../crates/zeroship-data-sql/src/lifecycle.rs), which resolve
  roles from metadata rather than column names.

The earlier proposal's runtime charter, implicit field lists and generator
workarounds no longer describe the implementation.
