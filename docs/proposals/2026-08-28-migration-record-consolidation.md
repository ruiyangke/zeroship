# Where migration records live

**Status:** designed, not implemented.

One record of what ran, per schema: the engine journal. Everything else is
removed.

## Placement

**Creator migrations belong to the creator.** Their journal lives in their own
schema, under the platform prefix:

```
<app_id>.__zeroship_schema_migrations
<app_id>.__zeroship_schema_migrations_supersedes
<app_id>.__zeroship_schema_migrations_inflight
<app_id>.__zeroship_schema_pending_contracts
<app_id>.__zeroship_schema_deploy_recovery
<app_id>.__zeroship_schema_backfills
```

**Platform migrations keep `zeroship_migrations.<table>`.**

The asymmetry is deliberate. The platform keeps a separate meta schema because
it has no tenant: nothing owns `zeroship` the way a migrator role owns an app
schema, so the engine's default derivation (`<project_schema>` + `_migrations`)
stays correct. The creator case moves in-schema precisely because a tenant DOES
own theirs.

## What is removed

| record | why |
| --- | --- |
| `zeroship.migrated_migrations` | its approval half is unreachable; its `descriptor_sha256` half moves to the journal |
| `zeroship.migrated_migration_audit` | one writer, zero readers; three of its four permitted actions belong to the unreachable approval flow |
| `zeroship.migrated_app_policies`, and `policy_store.rs` with it | policy is never persisted and never runtime-mutable |
| `zeroship_migrations.platform_migration_files` | filename + checksum, which `schema_migrations` already stores under an immutability trigger |
| `db/released_migrations.tsv` | a snapshot of the above |

Plus the three policy HTTP endpoints (`api.rs:211` write, `:245` get, `:262`
list) and `migration_store.rs`'s approval transitions.

**The approval workflow is unreachable.** `apply.rs:491` gates on
`!gated_versions.is_empty()`, driven by the `safety.require_approval`
obligation. `resolve_approval_level` starts at `ApprovalLevel::Never`
(`policy_approval.rs:123`) and only ever **loosens** on a matching rule
(`:127`); no policy artifact anywhere in the tree sets one, so the level cannot
leave `Never` and `migration_requires_approval` returns `false` for every op
(`:174`). A test pins the default (`policy.rs:744`).

Removing it removes a capability - operator approval of destructive creator
migrations - and that is intended, not incidental. A creator approves their own
destructive ops.

**Policy arrives in the artifact, not the database.**
`ApplyMigrationsRequest.policy` carries the draft with the request, and
`resolve_apply_policy` (`apply.rs:1208-1214`) already prefers it. This is the
mask policy's rule applied to migration policy: declared in the creator's
repository, folded at build time, immutable at runtime.

## Why the `__zeroship_` prefix is load-bearing

Without a fenced prefix, co-habitation is a live collision:

- the engine creates its journal with `CREATE TABLE IF NOT EXISTS`;
- its table names are **literals** - only `{meta}` is interpolated;
- `validate_collection` does **not** fence `schema_migrations`.

So a creator declaring a table named `schema_migrations` in their own schema
would have it silently **adopted as the journal**.

There are two production forks of `validate_collection` with no dependency edge
between them. Both now execute the same `__zero_migrate` and `__zeroship`
platform-prefix list; a test-only edge from `zeroship-schema` compares the lists
so they cannot silently diverge again:

| fork | reserves | governs |
| --- | --- | --- |
| `zeroship-schema/src/query.rs` | `__zero_migrate`, `__zeroship` | data-plane collection access |
| `zeroship-migrate-core/src/schema/query.rs` | `__zero_migrate`, `__zeroship` | schema-query DDL helpers |

A creator's declarative `createTable` reaches the engine fork through
`validate_ir_authorized`: the structural op gate explicitly calls
`validate_collection`, so `__zeroship_schema_migrations` is refused before
lowering emits SQL. This call is load-bearing; the matching constant alone would
not fence authoring.

**Sequencing remains the independent apply-time defence.** `ensure_journal`
bootstraps the journal before creator DDL runs, so even an unchecked artifact
cannot silently adopt `__zeroship_schema_migrations`. The normal creator path now
fails earlier with the actionable reserved-name error instead of relying on a
relation collision.

**A second refusal stands behind it, and it is conditional.** `dropTable` on the
journal is refused by the engine's own gate - *"plan requires approval
(destructive) but none was given"* - reached because the host passes
`Approval::None`. **If the host is ever made to assert approval on the
creator's behalf, the creator can drop their journal by name.** Both refusals
are asserted by exact message in the live apply API regression.

`crates/zeroship-data-plan/src/ident.rs` is a third copy because its
zero-dependency boundary forbids importing either validator. Its collection
role now consumes the same platform-prefix slice, and the data-plane parity
test compares all three copies. Its additional backend and runtime-plan
reservations remain separate.

The prefix is still worth having: it means one thing everywhere, and the pattern
is already in use - `__zeroship_workflow_{runs,steps,signals,blobs,subscriptions}`
live in the `app_<uuid>` workflow schema, which is a different service's journal
and is not folded in here.

## What this costs, accepted deliberately

The migrator role **owns** the app's schema (`provisioning.rs:140-160`:
`ALTER SCHEMA {proj} OWNER TO {role}`, `GRANT CREATE, USAGE`, default privileges
granting ALL on tables). A schema owner can `DROP` and `TRUNCATE` anything in
it, and owner privileges are **implicit - they cannot be REVOKE'd away.**

So a creator can destroy their own journal. That is the point: it is their
database and their app, and corrupting it breaks only them.

Two consequences follow and must not be "fixed" later:

1. **The platform cannot treat the creator journal as a trust anchor.** The
   deploy precondition keeps its own record.
2. **The rollback hole closes platform-side.** Record what the engine reported
   as applied (`outcome.applied`) alongside the declared descriptor; a
   re-submitted old IR yields `applied: []`, so the row shows nothing advanced
   while the per-request write the engine-upgrade case needs is preserved.
   Detail: `2026-08-28-deploy-schema-precondition.md`.

## The work

1. **Rewrite the corpus rather than appending to it.** Pre-release, the goal for
   `db/migrations-ts/` is clean and correct. Edit
   `20260702000200_control_tables.ts` so the three `migrated_*` tables are never
   created, drop their grants from `20260702000900_grants.ts`, and drop their
   trigger and comments from `20260702000700_functions_triggers_comments.ts`.
   The `migrated_*` naming fossil disappears rather than being inherited.
2. **Recreate the deployment's database.** `baseline`'s
   `assert_corpus_output_is_live` (`verbs.rs:1261`) refuses to adopt a database
   that is not what the corpus produces, so a schema-changing rewrite cannot be
   re-baselined onto the existing one. Recreating is clean pre-release and
   discharges every stale checksum in one step.
3. **Prefix the six engine journal tables `__zeroship_`.** They live in **two**
   files: five in `migrate-postgres/src/backend/journal_sql.rs` (`:128`, `:178`,
   `:224`, `:327`, `:342`) and `schema_backfills` in
   `migrate-postgres/src/backend/backfill_sql.rs:1527`. Grep `{meta}\.` across
   the crate rather than trusting that list.
4. **Point `meta_schema` at the app's own schema.** `conn.rs` exposes
   `meta_schema` and no table-name knob, so step 3 is what makes step 4 safe.
5. **Delete `provisioning.rs` step 5's `REVOKE ALL ON ... SCHEMA {meta}`.** With
   no separate meta schema it names nothing, and a revoke that names nothing
   reads as protection.
6. **Move the freeze check to `zeroship_migrations.schema_migrations`**, which
   is append-only by trigger and cannot lose its writer.
   `released_ledger_misordered` currently reads `released_migrations.tsv`, which
   step 1 deletes - move it in the same change or it silently stops guarding.
7. **Prove the corpus applies.** Apply it to a fresh PostgreSQL through the real
   `zero-migrate` CLI and report files applied, table/role/function counts, and
   an idempotent second run.

## What a reviewer must check

**Step 7 is not ceremony.** The corpus was rewritten once into the `schema()`
form and shipped without being applied end to end by anything; two of its files
turned out to be unauthorable - a BRIN option flattened to an unrepresentable
vendor attribute, and a `language:` value the IR had renamed. A rewrite is not
correct because it compiles.

**If a future change grants access to a creator journal, check the grantee, not
just the privilege.** A tenant owning its own journal is accepted here; a third
party gaining write access to someone else's is not.

**SQLite may need nothing.** Its journal is an attached `_mig` database
(`migrate-sqlite/src/backend/backfill_sql.rs:996`) and does not read
`meta_schema`. If the dev tier is unaffected, say so in
`docs/reference/sqlite-divergences.md` rather than leaving it silent.

## A constraint on any future consolidation

The engine's journal has **no tenant column** - `event_seq`, `event_kind`,
`version`, `name`, `checksum`, `at`, `by`, `exec_ms`, `down`, `phase`,
`outcome`, `kind`. Its per-app isolation is carried entirely by the schema name.
Any move to a single shared journal table must first add an owner column
upstream in the standalone engine, which changes a published contract.
