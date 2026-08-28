# Refusing a deploy whose migrations have not applied

**Status:** implemented in `5d27e71c5`. Path 1 of operator decision 9
(`2026-08-26-runtime-db-binding-00-index.md`, "the descriptor must not get
wrong"). Paths 2 and 3 (mid-life drift, restore) are answered by the in-WAL
epoch marker in `2026-08-28-cdc-service.md` section 8 and are not in scope here.

It carries one known hole - the ledger head can be moved backwards - and three
outstanding debts. Both are below, and neither is closed.

## The hazard

Decisions 7 and 8 made the runtime descriptor the sole schema authority and
deleted all live introspection. If a deploy goes live before its migrations
apply, the descriptor says a column is masked while the database still holds
plaintext there, and **the runtime serves plaintext believing it is masked.
Nothing detects it.** After the SC-6 flip the same skew reverses: the database
holds the mask under the new DDL while an older descriptor calls the column the
plain value.

## The enforcement point

`Registry::set_deploy_with_manifest`
(`crates/zeroship-control/src/registry.rs:455`). Its `UPDATE` **is** what "this
deploy becomes live" means: the gateway reads exactly `deploy_hash` and
`manifest_json` via `get_gateway_snapshot` (`registry.rs:859`) and polls them
(`crates/zeroship-gateway/src/sync.rs`). The `zeroship.app_deploys` insert
below the UPDATE is history, not liveness.

**The guard is a predicate on that UPDATE, inside the transaction** - not a
check in `api.rs`. `set_plan` (`registry.rs:561`) is the precedent: a separate
validate-then-UPDATE there had a TOCTOU window and was replaced by an `EXISTS`
subquery in the same statement. The window here is worse, because what races is
a concurrent migration.

When the UPDATE matches zero rows the handler re-reads to tell "no such app"
from "the predicate refused" - **for the message only**. The decision was
already taken atomically; that read cannot re-open it.

`Registry::set_deploy_hash` wrote `deploy_hash` without a manifest, so it
carried no descriptor to check even in principle, and it had zero callers. It is
deleted rather than guarded twice: guarding only the reachable writer would have
left it as the bypass the next person reaches for. *That finding only falls out
of asking whether a symbol has callers -* `grep "SET deploy_hash"` *found two
writers and gave no hint that one was dead.*

## What identifies the schema

**The `.zship` carries no migration identity and does not gain one.** `Manifest`
has exactly one schema-related field, `runtime_descriptor:
Option<RuntimeDescriptorEntry>` (`crates/zeroship-bundle/src/manifest.rs:172`),
whose `hash` is the sha256 of the `schema.runtime.json` blob
(`sdks/vite-plugin/src/zship.ts:526-527`). Ingest validates hex format and blob
presence only (`crates/zeroship-bundle/src/unpack.rs:428-435`) and never parses
the body.

`MigrationFileEntry` (`manifest.rs:194`) still exists, is not a `Manifest`
field, and nothing constructs or reads it. **Do not revive it.** Adding a
`migrations: [{name, hash}]` array to the manifest makes the artifact assert its
own preconditions, which is the shape that cannot be checked.

`manifest.runtime_descriptor.hash` is the identity because the descriptor is a
deterministic DB-free fold of the migration set: `genTypesFromMigrations`
(`sdks/vite-plugin/src/gen-types/index.ts`) calls `genArtifacts` **once** and
writes `schema.runtime.json` and `migrations.ir.json` from that single reply;
the CLI posts the latter verbatim as the apply body
(`crates/zeroship-cli/src/migrate.rs:139-150`). One emit, two artifacts.

The hash the build stamps into `migrations.ir.json` must be **the hash of the
bytes that reach the packer**, not of a re-serialisation of the same value.
`emit` writes `runtimeJson` verbatim as utf8 and `zship.ts` hashes the file it
reads back, so hashing the same string as utf8 agrees. A pretty-print, a
re-`JSON.stringify`, or a trailing newline added on either side produces two
hashes that can never agree, and the failure looks like the guard misfiring.

### Control already had the grant

| | request ledger | engine journal |
| --- | --- | --- |
| table | `zeroship.app_schema_applies` | `"<app_id>".__zeroship_schema_migrations` |
| readable by control | **yes** | no |

`zeroship_control` holds `select, insert, update` on `app_schema_applies`
(`db/migrations-ts/20260702000900_grants.ts:25`) and had never issued a query
against it. So this is one column and one SQL predicate - not a new service
call, a new grant, or a new cross-service dependency. It is also why the guard
survives the migration service being down: the predicate reads control's own
database on control's own connection.

The engine journal is per-app and now lives **in the app's own schema**, which
the migrator role owns. Owner privileges are implicit and cannot be revoked, so
a creator can destroy their own journal
(`crates/zeroship-migrate-server/src/provisioning.rs:164-171`, where the old
meta-schema `REVOKE` is deleted and the reasoning recorded). That is not a gap
this guard should close - it is the reason the guard reads control's ledger
instead of the journal.

## The ledger column

`descriptor_sha256 text NOT NULL` on `zeroship.app_schema_applies`
(`db/migrations-ts/20260702000200_control_tables.ts:94`), declared **with the
table** rather than added by a later migration. The corpus is rewritten
pre-release rather than appended to, so there is no era of rows lacking the
column and no nullable-column arm to reason about.

**The NULL the predicate tests is the MANIFEST's, not the row's.** `$4` is the
hash the deploy carries. A manifest with no descriptor must find no applied row
at all; a manifest with one must equal the newest applied row. Because the
column is `NOT NULL`, `=` is total and `IS NOT DISTINCT FROM` would only
weaken it - it would let a manifest with no descriptor past an applied row,
which is the bypass the first arm exists to close.

**One row per apply request, not per applied migration.** `insert_pending` and
`insert_auto_approved` both stamp `request.descriptor_sha256`
(`crates/zeroship-migrate-server/src/apply.rs:517` and `:573`). See the cost
section: this is a requirement, not an accident.

**`applied_versions json NOT NULL DEFAULT []`** (`:96`) carries what the engine
reported as applied for that request. It is what makes the per-request row safe:
an engine upgrade that changes descriptor bytes without changing any schema
writes a row with `[]`, so "nothing advanced" is distinguishable from "the
schema moved" instead of halting every app's next deploy forever.

`ApplyMigrationsRequest.descriptor_sha256` (`apply.rs:69`) is required and
validated as lowercase sha256 hex at the door (`is_sha256_hex`, `apply.rs:1570`,
400 `invalid_migration_request`). The comparison is a plain SQL `=` on `text`,
so an uppercase or whitespace-padded hash of the *same* bytes would be recorded,
would look right in the row, and would refuse every deploy of the app that
submitted it. Refusing the spelling at the door is the difference between a 400
naming the field and a 409 nobody can explain.

## The predicate: two arms, both required

```
AND CASE WHEN $4::text IS NULL
         THEN NOT EXISTS (SELECT 1 FROM zeroship.app_schema_applies
                           WHERE app_id = $3 AND status = 'applied')
         ELSE $4::text = (SELECT m.descriptor_sha256
                            FROM zeroship.app_schema_applies m
                           WHERE m.app_id = $3 AND m.status = 'applied'
                           ORDER BY m.applied_at DESC NULLS LAST,
                                    m.submitted_at DESC, m.migration_id DESC
                           LIMIT 1)
    END
```

**Descriptor present: it must equal the NEWEST applied row's hash, not be a
member of the set.** Membership lets a rollback through - N-1's hash was applied
once, so an `IN (...)` test passes while the database sits at N.

**Descriptor absent: allowed only when the app has no applied schema.**
`runtime_descriptor` is `skip_serializing_if = "Option::is_none"` on a
creator-produced artifact, so a one-armed guard is bypassed by deleting one JSON
key, and the app boots with `env.db` uninstalled over a live database. A guard
whose bypass is "omit the field" is not a guard.

The absent arm also covers a partial apply: the migration service provisions the
schema before the engine runs, so a migration that fails mid-apply leaves the
row `approved`/`rejected` while schema objects may already exist. Under a weaker
"allow when there is no *applied* row" rule any descriptor could then go live
over that partial schema.

## The refusal

HTTP **409**, `error: "schema_not_applied"` when the descriptor is present and
`"schema_descriptor_missing"` when it is absent, with both hashes and
`remedy: "zeroship migrate --app=<id>"` in the body
(`schema_precondition_response`, `crates/zeroship-control/src/api.rs:167`). The
CLI prints the body raw, so `remedy` is a command, not a sentence.

409 is load-bearing beyond readability:
`should_resolve_or_create_after_deploy_failure`
(`crates/zeroship-cli/src/main.rs:917`) returns true only on 404, so a 409 does
not trigger the auto-create-and-retry path and the creator sees this body rather
than a second failure against a freshly created app.

`print_migrate_reminder` runs **before** the upload (`main.rs:439`). Printed
after a 200, as it used to be, it named a step the deploy had already made it
too late to take in order.

**No operator override**, and not because overrides get abused: no legitimate
state requires one. The remedy is a creator-reachable endpoint already mandatory
in the golden path. The case that feels like it needs an override - a code
rollback across a migration boundary - is not made safe by one; it is made safe
by a forward migration. An override there produces the exact leak this guard
exists to stop.

## THE OPEN HOLE: the ledger head can be moved backwards

Two requirements this design needs independently are in tension, and together
they re-open the rollback that "newest applied, not membership" was written to
close.

- The predicate must compare against the **newest applied** row, or a rollback
  passes.
- The ledger row must be written **per request**, or an engine upgrade that
  changes descriptor bytes halts every app's next deploy forever.

Both are correct in isolation. Together they let a creator walk the head back:

1. Re-run `zeroship migrate` with an **old** `migrations.ir.json` while the
   database sits at N.
2. `insert_auto_approved` records the old descriptor D1 (`apply.rs:573`).
3. The engine skips every document - already journaled - so nothing applies.
4. **`mark_applied` fires anyway.** It is on the `Ok(outcome)` arm
   (`apply.rs:781`) with no guard on whether `outcome.applied` is non-empty.
5. The newest applied row now names D1 with `applied_at = now()`.
6. The old build deploys and the guard admits it.

**A server-derived descriptor does not close this.** Re-rendering from the
*submitted* documents reproduces D1, because those documents are
self-consistently old.

**What closes it** is to derive the descriptor from the engine journal's applied
set rather than from anything the request declares - i.e. stop shipping the
descriptor in the `.zship` at all and let the worker consume the one the
migration service derived. Then there is one descriptor and it is the database's.

The shipped test
`deploy_rolling_back_to_a_previously_applied_descriptor_is_refused`
(`crates/zeroship-control/tests/deploy_http_test.rs:1437`) covers only the case
where the creator does **not** re-migrate.

## What this still owes

**A server-derived descriptor.** Today the hash is client-declared: a creator
who hand-edits both generated files can make them agree about a lie. **This
proves ORDERING, not truth**, and `ApplyMigrationsRequest`'s doc comment says so
(`apply.rs:64-68`). The end state is that the migration service re-renders the
descriptor from the documents it just applied and answers 422 on a mismatch.
`render_artifacts` (`crates/zeroship-migrate-core/src/render/gen_types.rs:508`)
is the public function, the same one the napi addon wraps. **Unverified:**
whether a server-side render reproduces the build's bytes - the two call sites
pass different `(project_schema, effective)` pairs.
`crates/zeroship-migrate/tests/gen_types/gen_artifacts_byte_identical.rs`
already pins byte-identity across two sources and a third can join it. That gate
is owed before the check lands.

**Six harnesses that deploy a descriptor-carrying app with no migration service
at all.** `e2e_account_status_enforcement`, `e2e_lago_billing`,
`e2e_multi_app_attribution`, `e2e_multi_metric_billing`, `e2e_openmeter_export`
and `e2e_spend_state_transitions` all ship `examples/metering-probe`, which
carries a descriptor, and none of them mentions the migration service. Fixing
them means adding a whole service to six harnesses.
`tests/e2e_metering_billing.sh`, `tests/golden_path.sh` and
`tests/e2e_dev_vs_deployed_db.sh` were converted in the landing commit and are
the worked examples.

**Two harnesses whose subject the guard abolished.**
`tests/e2e_db_app_end_to_end.sh:322` is a section headed "THE PRE-MIGRATE
CONTROL" whose own prose says *"the app is deployed and serving, and its
database schema does not exist yet. That is the state a creator reaches by
following the documented chain up to `zeroship deploy`"*, and
`tests/e2e_app_primitives.sh:330-372` asserts that a worker dispatch in that
state returns `SCHEMA_NOT_PROVISIONED` carrying a `zeroship migrate` remedy.
**That state no longer exists for any descriptor-carrying app.** The runtime's
loud-failure path is now reachable only for schema-less deploys. That is a real
capability removed; the two harnesses are the evidence it was deliberate, and
neither has been reconciled.

## Costs accepted deliberately

Do not "fix" these without reading the argument.

- **The first deploy of every new database app takes two passes.**
  `zeroship migrate` refuses to create a missing app; `zeroship deploy` is what
  creates it. The creator sequence is deploy -> 409 -> migrate -> deploy.
  Mitigated by printing the reminder before the upload and putting the command
  in the 409 body, but a first-run experience whose middle step is a red error
  is how a guard acquires a reputation. `dev_provision` gets `--defer-deploy`
  for the same reason: create the app and ingest the blobs, apply, then re-run
  the same command to activate.
- **The ledger row must be written per apply request, even when nothing
  applies.** An engine upgrade that changes descriptor bytes without changing
  any schema halts every app's next deploy until its owner runs a migrate that
  applies nothing - and that migrate is only a remedy because it still writes a
  row. "Skip the ledger write when nothing applied" is a natural optimisation
  that would silently brick every future deploy. This is also the most likely
  reason someone reaches to disable the guard.
- **Code rollbacks across a migration boundary are refused permanently**,
  because the journal is append-only. Correct - rolling code back across a
  schema change is unsafe everywhere and the flip makes that failure silent -
  but it is a real capability removed, and it is the argument that keeps
  reappearing for the override refused above.

## Traps for anyone extending this

**The fixture trap, in the file where the test lives.** `manifest_for`
(`deploy_http_test.rs:102-146`) hardcodes `runtime_descriptor: None`, and the
ten pre-existing deploy cases use it correctly - they are schema-less apps. A
new case that reuses it unchanged routes around the guard and prints exactly
what a working guard prints. That is why every schema case builds its bundle
through `zship_with_descriptor` (`deploy_http_test.rs:1182`), and why each one
declares **two** mutations it must survive: delete the `AND CASE ... END` clause
(code), and build the bundle with `zship_with_descriptor(None)` (fixture). The
fixture mutation is the one this repository's history demands - with no
descriptor and no applied schema the guard correctly answers 200, so a case that
cannot tell those two apart is not measuring the guard.

Arm 1 also asserts that `get_routes()` yields no `deploy_hash` for the app
(`live_deploy_hash`, `deploy_http_test.rs:1234`), not merely the 409: asserting
status alone passes on a build that 409s *and* commits.

**Do not lift a descriptor from `examples/*/generated/`.** The committed
`schema.runtime.json` files are `"version": 1` with no `storage` key, while the
packer hard-rejects anything but v2 (`zship.ts`). They would fail for a reason
unrelated to the guard. `descriptor_blob` (`deploy_http_test.rs:1166`)
synthesises a minimal v2 body instead, which is enough because ingest never
parses the descriptor.

**Do not reuse `approved_checksum`.** `PreflightReport::content_checksum()`
folds sorted `version=checksum` pairs to close the approve/apply TOCTOU. It is
tempting and wrong here: it is derived **after** server-side lowering under the
app's per-app effective policy, which the build cannot reproduce. A reviewer who
finds the column and stops will specify a comparison the build can never
satisfy.

## One invariant that exists only as prose

`sdks/vite-plugin/src/gen-types/confined-ceiling.ts:20-22` states: *"the emitted
`schema.runtime.json` cannot describe a different table from the one the
migration apply produces."* It rests on `confined-system-shape.inject.toml`
being shared between the migration service
(`crates/zeroship-migrate-server/src/policy.rs:57-58`, `include_str!`) and
TypeScript (`policies/codegen.mjs`). **Sharing the input is not checking the
output**, and nothing computes both descriptors and compares them. The
server-derived descriptor owed above is that check.
