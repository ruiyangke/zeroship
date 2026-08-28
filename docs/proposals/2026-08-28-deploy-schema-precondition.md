# Refusing a deploy whose migrations have not applied

**Date:** 2026-08-28
**Status:** designed, not implemented. Path 1 of operator decision 9
(`2026-08-26-runtime-db-binding-00-index.md`, "the descriptor must not get
wrong"). Paths 2 and 3 (mid-life drift, restore) are answered by the in-WAL
epoch marker in `2026-08-28-cdc-service.md` section 8 and are NOT in scope here.

## The hazard

Decisions 7 and 8 made the runtime descriptor the sole schema authority and
deleted all live introspection. If a deploy goes live before its migrations
apply, the descriptor says a column is masked while the database still holds
plaintext there, and **the runtime serves plaintext believing it is masked.
Nothing detects it.** After the SC-6 flip the same skew reverses: the database
holds the mask under the new DDL while an older descriptor calls the column the
plain value.

## The enforcement point

`Registry::set_deploy_with_manifest` (`crates/zeroship-control/src/registry.rs:402`),
specifically the `UPDATE` at `:410-416`. That statement **is** what "this deploy
becomes live" means: the gateway reads exactly `deploy_hash` and `manifest_json`
(`registry.rs:664-688` -> `:770-771` -> `internal.rs:171-185`), and the gateway
polls that (`zeroship-gateway/src/sync.rs:280-296`). The
`zeroship.app_deploys` insert at `registry.rs:423-434` is history, not liveness.

**The guard goes INSIDE the transaction, as a predicate on the UPDATE** - not in
`api.rs`. The precedent and the argument are already in this file: `set_plan`'s
doc comment (`registry.rs:462-471`) records that a separate validate-then-UPDATE
had a TOCTOU window and was replaced by an `EXISTS` subquery in the same
statement.

### Delete `set_deploy_hash` in the same change

`Registry::set_deploy_hash` (`registry.rs:387`) writes `deploy_hash` **without a
manifest**, so it carries no descriptor to check even in principle. Verified: a
repo-wide grep across `crates/`, `sdks/`, `tests/` returns **one line, its own
definition. Zero callers.** Guarding only the reachable writer leaves this as the
bypass the next person reaches for; guarding both writes the predicate twice for
a function nobody calls. Delete it.

*This is a finding that only falls out of asking whether a symbol has callers.
`grep "SET deploy_hash"` finds two writers and gives no hint that one is dead.*

## What the pipeline can know

**The `.zship` carries no migration identity, and it does not need to gain one.**
`Manifest` has exactly one schema-related field - `runtime_descriptor:
Option<RuntimeDescriptorEntry>` (`zeroship-bundle/src/manifest.rs:172`), whose
`hash` is the sha256 of the `schema.runtime.json` blob
(`sdks/vite-plugin/src/zship.ts:526-527`). Ingest validates hex format and blob
presence only (`unpack.rs:428-435`) and never parses the body.

`MigrationFileEntry` (`manifest.rs:194-200`) exists, is not a `Manifest` field,
and nothing constructs or reads it. **Do not revive it.** Adding a
`migrations: [{name, hash}]` array to the manifest makes the artifact assert its
own preconditions, which is the shape that cannot be checked.

**`manifest.runtime_descriptor.hash` IS the identity**, because the descriptor is
a deterministic DB-free fold of the migration set:
`genTypesFromMigrations` (`sdks/vite-plugin/src/gen-types/index.ts:283-336`)
calls `genArtifacts` **once** and writes `schema.runtime.json` and
`migrations.ir.json` from that single call; the CLI posts the latter verbatim as
the apply body (`zeroship-cli/src/migrate.rs:139-150`). One emit, two artifacts.

### The load-bearing discovery: control already has the grant

| | request ledger | engine journal |
| --- | --- | --- |
| table | `zeroship.migrated_migrations` | `"<app_uuid>_migrations".schema_migrations` |
| readable by control | **YES** | no |

`zeroship_control` holds `select, insert, update` on `migrated_migrations`
(`db/migrations-ts/20260702000900_grants.ts:25`) **and has never issued a single
query against it** - `grep -rn migrated_migrations crates/zeroship-control/src/`
returns nothing. Verified.

**So this is one column and one SQL predicate, not a new service call, a new
grant, or a new cross-service dependency.** It is also why the guard survives
`migrated` being down: the predicate reads control's own database on control's
own connection.

The engine journal is per-app, created on the superuser provisioning DSN with no
grant to any other role (`zeroship-migrated/src/provisioning.rs:162-176`). Its
reader is public (`journal_sql::applied`, `journal_sql.rs:506`) and **no platform
service calls it** - `migrated` links the crate and never reads the journal it
writes.

## The change

1. **New column** `descriptor_sha256 text` on `zeroship.migrated_migrations`, in
   a NEW migration file dated after `20260820000100_app_egress_rules.ts`, per the
   AGENTS.md ordering rule.
2. **M1, client-declared (minimum).** `ApplyMigrationsRequest`
   (`zeroship-migrated/src/apply.rs:50-55`) gains `descriptor_sha256: String`;
   the build stamps `sha256Hex(runtimeJson)` where both values are already in
   hand (`gen-types/index.ts:334`). **This proves ORDERING, not truth** - a
   creator who hand-edits both generated files can make them agree about a lie.
   State that limit; do not paper over it.
3. **M2, server-derived (end state, available today).** `migrated` re-renders the
   descriptor and requires the declared hash to match, 422 otherwise. The
   function is public Rust: `render_artifacts` (`migrate-core/src/render/gen_types.rs:508`),
   the same one the napi addon wraps. **Unverified:** whether a server-side
   render reproduces the build's bytes - the two call sites pass different
   `(project_schema, effective)` pairs. `gen_artifacts_byte_identical.rs` already
   pins byte-identity across two sources and a third can join it. That is the
   gate M2 owes.
4. **Nothing changes in the `.zship`.**

## The predicate: two arms, both required

Compare against the **LATEST applied** descriptor hash, not membership in the
set. Membership lets a rollback through - N-1's hash was applied once, so an
`IN (...)` test passes while the database sits at N.

**Descriptor absent must ALSO refuse**, when the app has any applied schema.
`runtime_descriptor` is `skip_serializing_if = "Option::is_none"`
(`manifest.rs:171`) on a creator-produced artifact, so a one-armed guard is
bypassed by deleting one JSON key, and the app boots with `env.db` uninstalled.
**A guard whose bypass is "omit the field" is not a guard.**

## The refusal

**HTTP 409**, `error: "schema_not_applied"` (and `"schema_descriptor_missing"`
for the second arm), with the exact remedy command in the body - the CLI prints
it raw (`zeroship-cli/src/main.rs:824-827`). Checked:
`should_resolve_or_create_after_deploy_failure` (`main.rs:910-912`) returns true
only on 404, so a 409 does not trigger the auto-create retry.

**No operator override.** Not because overrides get abused, but because **no
legitimate state requires one**: the remedy is a creator-reachable endpoint that
is already mandatory in the golden path. The case that feels like it needs one -
a code rollback across a migration boundary - is not made safe by an override,
it is made safe by a forward migration. An override there produces the exact
leak this guard exists to stop.

**Also move `print_migrate_reminder` (`main.rs:608-627`) to BEFORE the deploy.**
Today it prints "Deploy does NOT apply them" *after* a successful deploy, and its
own doc comment (`:596-607`) names this design as the fix.

## The acceptance test, and the fixture trap it must avoid

Home: `crates/zeroship-control/tests/deploy_http_test.rs`, which drives the full
handler and **refuses** rather than skips without a live DB
(`tests/common/mod.rs:89-104`).

Three arms: (1) no applied row + descriptor present -> 409; (2) the same bytes
plus one applied row with a matching hash -> 200; (3) applied row + descriptor
absent -> 409.

Arm 1 must assert **`get_routes()` yields no `deploy_hash` for the app**, not
merely the 409 - asserting status alone passes on a build that 409s *and*
commits.

**Two mutations, and the second is the one this repository's history demands:**

- **code:** delete the `AND (...)` clause; arm 1 must go red.
- **fixture:** set `runtime_descriptor: None` in arm 1; arm 1 must go red,
  because with no descriptor and no applied schema the two-armed guard correctly
  returns 200.

**The fixture mutation is not hypothetical.** `manifest_for`
(`deploy_http_test.rs:102-146`) hardcodes `runtime_descriptor: None` at `:145`,
and all ten existing deploy cases use it - correctly, they are schema-less apps.
A new test that reuses it unchanged is **guaranteed** to route around the guard
and print exactly what a working guard prints. That is verification-record
class 7 (the fixture whose subject is not the claim) waiting to happen, in the
exact file where the test must live.

**Do not lift a descriptor from `examples/*/generated/`.** All nine committed
`schema.runtime.json` files are `"version": 1` with no `storage` key, while the
packer hard-rejects anything but v2 (`zship.ts:937-943`). They are stale
artifacts and would fail for a reason unrelated to the guard.

## LANDED 2026-08-28 in `5d27e71c5`, and the implementation found a HOLE IN THIS SPEC

**Two requirements this document states as independent are in direct tension,
and together they re-open the rollback the "newest applied, not membership"
rule was written to close.**

- Section "The predicate" requires comparison against the **newest applied**
  descriptor, because membership lets a rollback through.
- Section "What this costs" requires the ledger row to be written **per
  request**, not per applied migration, because otherwise an engine upgrade that
  changes descriptor bytes halts every app's next deploy forever.

Both are correct in isolation. Together they let a creator move the ledger head
**backwards**:

1. Re-run `zeroship migrate` with an **old** `migrations.ir.json` while the
   database sits at N.
2. `insert_auto_approved` records the old descriptor D1 (`apply.rs:539`).
3. The engine skips every document - already journaled - so nothing applies.
4. **`mark_applied` fires anyway.** Verified: it is on the `Ok(outcome)` arm at
   `apply.rs:781` with no guard on whether `outcome.applied` is non-empty.
5. The newest applied row now names D1 with `applied_at = now()`.
6. The old build deploys and the guard admits it.

**M2 does not close this.** A server-derived descriptor renders from the
*submitted* documents, which are self-consistently old.

**What does close it** is the end state this document's last bullet already
names for a different reason: derive the descriptor from **the engine journal's
applied set** rather than from anything the request declares - i.e. stop
shipping the descriptor in the `.zship` at all and let the worker consume the
one `migrated` derived. That is now the second independent argument for it.

The shipped test
`deploy_rolling_back_to_a_previously_applied_descriptor_is_refused` covers only
the case where the creator does **not** re-migrate.

### The landing cost is 8 test scripts, not the 2 this document named

Measured by the implementer by scanning every harness that builds a `.zship`
from an example carrying a `migrations/` directory. Six of them
(`e2e_account_status_enforcement`, `e2e_lago_billing`,
`e2e_multi_app_attribution`, `e2e_multi_metric_billing`, `e2e_openmeter_export`,
`e2e_spend_state_transitions`) ship `examples/metering-probe`, which carries a
descriptor, and **run no migration service at all** - so fixing them means
adding a whole service to six harnesses.

**And the two reorderable ones are worse than a reorder**, which is the finding
this document should have had:
`tests/e2e_db_app_end_to_end.sh:322-347` is a section headed **"THE PRE-MIGRATE
CONTROL"** whose own prose says *"the app is deployed and serving, and its
database schema does not exist yet. That is the state a creator reaches by
following the documented chain up to `zeroship deploy`"*, and
`tests/e2e_app_primitives.sh:330-372` asserts that a worker dispatch in that
state returns `SCHEMA_NOT_PROVISIONED` carrying a `zeroship migrate` remedy.

**Arm 1 abolishes that state for every descriptor-carrying app.** The runtime's
carefully built loud-failure path becomes unreachable except for schema-less
deploys. That is a real capability removed, it was not weighed here, and the two
harnesses that documented the state are the evidence it was deliberate.

**One point in arm 1's favour that this document also omitted:** a migration
that fails mid-apply leaves the row `approved`/`rejected` while schema objects
may already exist, because `migrated` provisions the schema before the engine
runs. Under a weaker "allow when there is no applied row" rule, any descriptor
could then go live over that partial schema. Arm 1 closes that, which is why the
weaker rule is not the answer to the eight harnesses.

## What this costs, argued against itself

- **It breaks the first deploy of every new database app.** `zeroship migrate`
  refuses to create a missing app; `zeroship deploy` is what creates it. The
  sequence becomes deploy -> 409 -> migrate -> deploy. Mitigated by moving the
  reminder before the upload and putting the command in the 409 body, but a
  first-run experience whose middle step is a red error is how a guard acquires
  a reputation.
- **It blocks `dev_provision`.** `bin/dev_provision.rs:129-142` calls
  `set_deploy_with_manifest` directly and never applies migrations
  (`grep -n migrat dev_provision.rs` returns nothing). `tests/golden_path.sh`
  runs a dev-provision arm, so that arm must gain a migrate step or it goes red
  on landing. Measured, not inferred.
- **An engine upgrade that changes descriptor bytes halts every app's next
  deploy** until its owner runs a migrate that applies nothing. Survivable ONLY
  because `migrated` records the row per *request* rather than per applied
  migration (`apply.rs:485-495`). **Write that down as a requirement** - "skip
  the ledger write when nothing applied" is a natural optimisation that would
  silently brick every future deploy. This is the most likely reason someone
  disables the guard.
- **It refuses code rollbacks across a migration boundary, permanently**, because
  the journal is append-only. Correct - rolling code back across a schema change
  is unsafe everywhere, and the flip makes that failure silent - but it is a real
  capability removed, and it is the argument for the override refused above.
- **M1 proves ordering, not truth.** If M2's byte-identity does not hold, the
  honest end state is larger and stronger: **stop shipping the descriptor in the
  `.zship` at all** and have the worker consume the descriptor `migrated` derived
  from the migrations it applied. Then path 1 needs no guard, because there is
  only one descriptor and it is the database's.

## One invariant that exists only as prose

`sdks/vite-plugin/src/gen-types/confined-ceiling.ts:20-22` states: *"the emitted
`schema.runtime.json` cannot describe a different table from the one the
migration apply produces."* It rests on `confined-system-shape.inject.toml` being
shared between `migrated` (`policy.rs:55-58`, `include_str!`) and TypeScript
(`policies/codegen.mjs`). **Sharing the input is not checking the output**, and
nothing computes both descriptors and compares them. M2 is that check.

## Do not reuse `approved_checksum`

`PreflightReport::content_checksum()` (`apply.rs:1204-1224`) folds sorted
`version=checksum` pairs to close the approve/apply TOCTOU. It is tempting and it
is wrong for this: it is derived **after** server-side lowering under the app's
per-app effective policy (`apply.rs:1283-1292`), which the build cannot
reproduce. A reviewer who finds the column and stops will specify a comparison
the build can never satisfy.
