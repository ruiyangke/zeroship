# Ordered upgrades to the workflow journal

Version 1 is `../schema.ts`. Every later version is one module here, named
`NNNN_what_it_does.ts` and exporting `up(namespace)`, which records its changes
through the same `@zeroship/migrate` DSL `schema.ts` uses and returns the same
`{ identifiers, columns }` shape.

Versions must be contiguous, starting at `0002`. The generator refuses a gap.

`../generate.mjs` folds the whole series into the snapshot artifacts
(`../postgres.sql`, `../sqlite.sql`) and emits each version's own DDL under
`../versions/`. Nothing here is authored twice: the snapshot is the fold, never a
hand-edited file, so it cannot describe a different journal from the series.

Adding a version changes the journal's fingerprint. That is the intended signal:
a host compiled against the new fingerprint refuses a journal still at the old
one, the manager sends the bundle, and the service applies exactly the versions
between the installed stamp and the bundle's target.

A version's DDL runs inside the same transaction as every other version the
upgrade applies, so a failure part way leaves the stamp where it was. Author each
one so that is true of the database as well: avoid a statement PostgreSQL cannot
run inside a transaction.
