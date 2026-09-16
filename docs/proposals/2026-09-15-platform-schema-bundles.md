# Platform schema bundles: provisioning per-app system schemas without domain leaks

Status: proposed, 2026-09-15.

The workflow journal is a platform-owned schema that lives inside a creator's
database. Installing it today makes the migration service know what a workflow
is, and leaves no way to change the schema once an app has one. This proposal
separates the two responsibilities and gives the schema an ordered history.

## The two problems

### 1. The migration service knows about workflows

`crates/zeroship-migrate-server` carries workflow-specific code:
`provisioning.rs` holds the journal template, its schema-name substitution, an
installed check and `provision_workflow_app`; `api.rs` publishes a route named
for the domain and a handler that calls it. The template arrives through
`include_str!` reaching OUT of the crate directory into
`crates/zeroship-workflow/schema/postgres.sql`, and `Cargo.toml` declares no
dependency on that crate at all.

The comment defending it says the service "keeps no dependency on the creator
engine". The dependency exists; it is simply undeclared. That costs three
things:

- Cargo's dependency graph does not show it, so `cargo xtask test repository`
  cannot reason about it and moving the workflow crate breaks the build as a
  missing FILE rather than a missing dependency.
- The schema-name substitution is implemented twice, in the service's
  `workflow_journal_tables_sql` and in the engine's `schema::postgres_sql`. Two
  copies that must agree exactly, with nothing holding them together.
- The service's domain is now "creator migrations, plus workflows". A second
  platform-owned schema would add a third.

### 2. The journal cannot change

The stamp table records a fingerprint and nothing else. The generator emits only
the full create-everything form. `journal_installed` gates on the stamp table
EXISTING, so re-provisioning leaves an old journal untouched, and the host
refuses any journal whose fingerprint differs from the one compiled into it
(`crates/zeroship-workflow/src/service/schema.rs`).

Change the schema and every existing journal mismatches, every host refuses it,
and nothing in the system can bring it forward. That is change DETECTION with no
repair path. Pre-launch it is survivable by dropping databases; that is not a
property to launch with, and schema shapes are exactly what launch freezes.

## What each party actually owns

- **The migration service** owns privileged DDL against a creator database: the
  app schema, the migrator and runtime roles, and applying recorded operations
  under a policy ceiling. It is PostgreSQL-only and refuses anything that is not
  pure DDL. None of that is workflow knowledge.
- **The workflow domain** owns what its journal looks like, which version it is
  at, and how one version becomes the next.
- **The workflow manager** owns when an app's journal must exist, because it is
  the platform-side service that learns an app registered.

The current design gives the first party the second party's knowledge. The fix
is to move the knowledge, not to tidy the include.

## Design

### A leaf crate for the artifacts

`crates/zeroship-workflow-schema` owns the generated artifacts and nothing else:
the per-dialect SQL, the fingerprints, the runtime descriptor, the current
VERSION, the stamp descriptor, and the ordered upgrades. It depends on nothing
in the workspace, so both the engine and any caller may depend on it without
pulling the engine in.

The schema-name substitution moves here too, so it exists once. `generate.mjs`
keeps `schema.ts` as the single authored source and writes into this crate.

### A domain-neutral endpoint

The migration service gains one generic capability and loses all workflow
knowledge. A request carries a SCHEMA BUNDLE: recorded operations, the policy
the bundle declares, a stamp descriptor (table, row id, target version), and a
fingerprint. The service authenticates the caller as a platform service,
composes the effective policy the way `resolve_apply_policy` already does for
creator migrations, reads the declared stamp, and then:

- no stamp: install at the bundle's version
- stamp equals the bundle version: verify the fingerprint, refuse a mismatch as
  a corrupted journal rather than silently repairing it
- stamp below: apply the ordered upgrades between the two versions in ONE
  transaction, then re-stamp
- stamp above: REFUSE. An app provisioned by a newer platform must never be
  silently downgraded by an older one.

The service knows about bundles, stamps and versions. It never learns what a
journal is. A second platform-owned per-app schema reuses this unchanged, which
is the test of whether the boundary is real.

This does not widen the trust surface. The service already accepts creator
drafted migrations narrowed by an operator ceiling; a bundle from an
authenticated platform service is strictly more constrained, and it arrives as
recorded operations rather than free SQL. Policy still "arrives in the artifact,
not the database", exactly as the apply path documents.

### The manager is the caller

`zeroship-workflow-server` depends on the schema crate and sends the bundle. It
is the right caller because it already learns that an app registered, and
because routing this through Control would move the same leak into Control -
Control would then need the workflow artifacts.

Two triggers:

- **Registration.** When an app registers, ensure its journal is at the current
  version. Idempotent, so it is safe on every deploy, and a schema change rolls
  out as apps redeploy.
- **Refusal.** When a host refuses a journal on a fingerprint or version
  mismatch, that refusal becomes a repair trigger rather than a dead end. The
  host reports it, the manager provisions, the host retries.

The second trigger is what closes problem 2. Today a refusal is terminal.

### SQLite

`migrate-server` is PostgreSQL-only by design and refuses the SQLite rebuild
step. The local development journal is SQLite, so the CLI applies the SAME
ordered artifacts in process through the engine, which is multi-dialect.

This is a real seam: two appliers, one source. It is acceptable because the
artifacts and the version sequence are shared and generated together, so the two
paths cannot drift in WHAT they apply, only in WHO applies it. The alternative -
teaching the PostgreSQL-only service to drive SQLite - is the larger and worse
change.

## What this replaces, concretely

Deleted from `zeroship-migrate-server`: the workflows route and its handler,
`provision_workflow_app`, the journal template constant, the substitution helper,
the installed check, and the cross-crate `include_str!`. What remains there is
the generic bundle path and the creator apply path.

## Alternatives considered

**Put the journal in the creator's own migration stream.** Rejected. The journal
is platform-owned and a creator must not be able to alter or skip it, and
workflow-only apps never author a migration at all.

**Put the journal in the platform migration corpus (`db/migrations-ts/`).**
Rejected, and already argued in the tree: a static migration cannot name a
per-app schema for an app that does not exist yet. The deleted
`control_workflow_journal_access` migration says exactly this.

**Keep the include and only add versioning.** Rejected. It fixes the symptom the
operator sees and leaves the domain boundary broken, and the duplicated
substitution with it.

**Have the worker provision its own journal.** Rejected. Privilege follows the
process: the worker executes creator code and must not hold DDL authority.

## What proves it

- A bundle applied twice leaves the journal and its ROWS unchanged.
- A bundle one version ahead upgrades an installed journal and re-stamps it.
- A bundle BEHIND an installed journal is refused, not applied.
- A fingerprint mismatch at the same version is refused as corruption.
- An upgrade that fails part way leaves the stamp at the old version, because
  the whole upgrade is one transaction.
- A host that refuses a journal causes the manager to provision it, after which
  the host proceeds. This is the end-to-end repair path, and it is the one the
  examples would have caught.
- `zeroship-migrate-server` contains no occurrence of "workflow" outside the
  creator apply path's own tests. This is the boundary assertion, and it is
  cheap to keep.

## Open questions

1. ~~Does the bundle carry recorded operations or compiled per-dialect SQL?~~
   **SETTLED 2026-09-15: compiled per-dialect SQL, and operations were never
   available.** `validate_collection` in
   `crates/zeroship-migrate-core/src/schema/query.rs` refuses any collection
   whose name carries the reserved `__zeroship` prefix. It is unconditional and
   fail-closed, no capability or policy layer lifts it, and it is the same
   function CRUD dispatch uses. That rule is WHY `names.mjs` exists: `schema.ts`
   authors unprefixed names and `bindOwnedNames` rewrites the compiled SQL
   afterwards. So an operation-carrying bundle cannot express this journal, and
   the only op form that could - `raw` - is compiled SQL wearing an operation's
   clothes, which the ceiling polices no more finely than the text.

   The service still polices the bundle, at the layer where schema confinement
   actually lives: `MigrationGuard::check` bound to the target schema. Measured:
   the journal artifact bound to `customer` passes, and the same artifact bound
   to `someone_else` is refused `CrossSchema`. Two consequences the
   implementation must carry: the guard FLAGS rather than denies a destructive
   bundle, so the service gates `outcome.destructive` against the composed
   `safety.destructive_ops` itself (the creator path gets that from the engine's
   plan gate, which a direct-SQL applier never runs); and the endpoint is
   addressed by SCHEMA, not by app, because a project-level creator database
   makes the schema underivable from an app id.
2. Should the stamp live in the app schema (where it is today) or in a platform
   registry? In the app schema it travels with the thing it describes and
   survives a platform database restore; in a registry the manager can answer
   "which apps are behind" without touching creator databases.
3. Pre-launch, the current snapshot becomes version 1 and the ordered series
   starts there, with no back-migration. Confirm that rather than assume it.
