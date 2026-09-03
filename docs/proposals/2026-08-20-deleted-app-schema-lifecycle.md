# Who drops a deleted app's schemas, and when

**Date:** 2026-08-20
**Status:** SUPERSEDED 2026-08-31. The control-plane lifecycle is now archive,
not hard delete. This document remains as the historical teardown analysis;
teardown still requires a database-binding-keyed design in migrate-server.
**Scope:** the lifecycle of an app's Postgres schemas, per-app role, and
key-value/object residue after `DELETE /api/apps/{id}`. Explicitly NOT: the
account-erasure lifecycle in `crates/zeroship-auth/src/cron/account_reaper.rs`, which is
upstream of this and already has a decided shape.

---

## 0. Reading conventions

Every claim about current behaviour carries a `file:line` against this working
tree at `1c36fe3c6`. Claims are labelled:

- **VERIFIED** - I read the cited lines, or ran the cited query, myself.
- **MEASURED** - a number I produced by executing something, with the vehicle named.
- **INFERRED** - a conclusion drawn from verified facts, not itself read.
- **NOT CHECKED** - stated so the reader does not mistake silence for evidence.

---

## 1. What deletion does today

**VERIFIED.** `DELETE /api/apps/{id}` (`crates/zeroship-control/src/api.rs:451`) calls
`purge_app` (`crates/zeroship-control/src/api.rs:431`), which does exactly two things:

```rust
pub async fn purge_app(state: &AppState, app_id: &Uuid) -> Result<bool, PurgeError> {
    state
        .blob_store
        .delete_app_manifests(app_id)
        .await
        .map_err(PurgeError::Manifests)?;

    let deleted = state
        .registry
        .delete_app(app_id)
        .await
        .map_err(PurgeError::Registry)?;

    Ok(deleted)
}
```

Step 2 is a **hard delete with no tombstone** (`crates/zeroship-control/src/registry.rs:367`):

```rust
    let tx = conn.transaction().await?;
    let n = tx
        .execute("DELETE FROM zeroship.apps WHERE id = $1", &[id])
        .await?;
```

**VERIFIED.** `zeroship.apps` has no soft-delete column. Its full definition is
`db/migrations-ts/20260702000200_control_tables.ts:106-124`: `id`, `name`,
`plan_id`, `deploy_hash`, `api_key`, `api_key_hash`, `env_version`,
`workflows_enabled`, `manifest_json`, `created_at`, `updated_at`, `system`. No
`deleted_at`, no `archived_at`, no `status`.

That absence is a choice, not an oversight of the schema author. The same
migration corpus tombstones a different entity: `zeroship.sandboxes` carries
`deleted_at` and there is a `zeroship.deleted_sandboxes` table
(`db/migrations-ts/20260702000500_sandbox_tables.ts:10,76,121`). The shape was
available and was not applied to apps.

**VERIFIED.** The nearest thing to a reversible off-switch that apps ever had was
`suspended`, and it was **dropped three days ago**
(`db/migrations-ts/20260817000200_drop_app_freeze_flags.ts`), on the grounds
that it never reached the data plane:

> Neither ever reached the data plane: an app carrying either flag kept serving
> traffic and kept writing its own database, storage and KV, so what they froze
> was the creator's ability to change the app, not the app.

So at HEAD an app has exactly one terminal lifecycle operation and no reversible
one.

---

## 2. What survives a delete

**VERIFIED.** An app owns **three** Postgres schemas and one role. Two of the
three are not named by any teardown code in the tree, and the third is named
only by code with no production caller.

| Object | Verdict | Deciding code |
| --- | --- | --- |
| `"<uuid>"` - the creator's tables, holding END USERS' rows | **survives** | `crates/zeroship-plugin-db/src/drop_namespace.rs:161` is the only production-shaped `DROP SCHEMA`, and the module is `#![allow(dead_code)]` with test-only callers |
| `"<uuid>_migrations"` - the migration engine journal | **survives** | named by nothing; created at `third_party/zero-migrate/crates/zeroship-migrate/src/conn.rs:188` |
| `"app_<uuid>"` - the workflow journal's 5 `__zeroship_workflow_*` tables | **survives** | named by nothing; `crates/zeroship-control/src/cron/workflow_engine.rs:306-311` states it outright |
| role `app_<uuid>_role`, its template membership, grants and default privileges | **survives** | `drop_per_app_role` (`crates/zeroship-data-engine/src/auth/bootstrap.rs:1671`) has exactly one caller, the dead `drop_namespace` |
| `env.kv` keys | **survives** | scoped `{<app_id>}:<key>` (`crates/zeroship-plugin-kv/src/backend/mod.rs:150`); the `KvBackend` trait has per-key ops only, no namespace drop |
| `env.storage` objects | **survives** | prefix `<app_id>/<bucket>/<key>` (`crates/zeroship-plugin-storage/src/backend/s3.rs:143`, `local.rs:104`); `crates/zeroship-plugin-storage/src/limits.rs:145`: "there is no runtime teardown hook to sweep them" |
| deploy blobs under `blobs/` | **survives permanently** | the `BlobStore` trait (`crates/zeroship-bundle/src/blob.rs:57-163`) has no `delete_blob` and no `list_blobs`. They are structurally unreclaimable, deleted app or not |
| `zeroship.workflow_scheduler_timers` / `_inflight` rows | **survives** | `app_id uuid NOT NULL` with **no FK** (`db/migrations-ts/20260811000100_workflow_scheduler_store.ts:44-66`) |
| `zeroship.app_audit` rows | **survives** | `app_id` with no FK, plus its own append-only trigger |
| `manifests/<app_id>/` | removed | `crates/zeroship-bundle/src/blob.rs:544-560` |
| the gateway route entry | removed | `get_routes` is `FROM zeroship.apps a` (`crates/zeroship-control/src/registry.rs:665`); the row is gone, so the route is |
| 26 `zeroship.*` tables with `ON DELETE CASCADE` to `apps` | removed | `db/migrations-ts/20260702000600_constraints_indexes_fks.ts` |
| auth's per-app rows (`app_user_identities`, `oauth_grants`, `app_session_anchors`) | removed | they cascade off the `oauth_clients` row `delete_app` deletes explicitly (`crates/zeroship-control/src/registry.rs:371-381`) |
| CDC replication slots | removed | worker-side, on the route feed disappearing (`crates/zeroship-worker/src/sync.rs:141-158`) - slots only, no schema, no role |

Note the residue is not only storage. The scheduler timer rows are **live
timers keyed to an app that no longer exists**, and the scheduler store only
deletes by `run_id` (`crates/zeroship-workflow-scheduler/src/store.rs:179`). That is a
correctness leak, not a housekeeping one, and it is not fixed by dropping a
schema.

One inverse case worth recording, because it is the only thing the fleet
reclaims and it does so for the wrong reason: a deleted app's workflow output
blobs under `wfblob/` DO get collected, ~72 h later, by
`workflow_blob_gc::tick_orphan_sweep`. Not because anything noticed the delete -
because the app left `journalled_apps`, so its own journal rows stopped counting
as references and the objects read as orphaned. The journal ROWS that referenced
them stay forever.

Note the second row of the dead-code column. `drop_namespace` is the only
teardown that exists, and **it drops one of the three schemas**: step 4 is
`DROP SCHEMA IF EXISTS {schema} CASCADE` where `schema = quote_ident(app_id)`
(`crates/zeroship-plugin-db/src/drop_namespace.rs:160-161`), the bare uuid. Wiring it as
written would still leave `"<uuid>_migrations"` and `"app_<uuid>"` behind. This
matters for section 7: "just call the existing teardown" is not a fix.

`crates/zeroship-plugin-db/src/drop_namespace.rs:44-55` says so itself:

> Until the control plane wires the call (cross-worker fan-out + lock), the
> orchestrator surface is unused in a default build

**MEASURED.** Vehicle: `docker exec zs-auth-pg-5440 psql`, read-only, against
the pre-existing test databases on 127.0.0.1:5440. A workflow journal schema
holding **zero runs** costs 376 kB:

```
__zeroship_workflow_blobs         | 24 kB
__zeroship_workflow_runs          | 64 kB
__zeroship_workflow_signals       | 40 kB
__zeroship_workflow_steps         | 48 kB
__zeroship_workflow_subscriptions | 32 kB
TOTAL                             | 376 kB     (runs_rows = 0)
```

Across 17 journal schemas in `zeroship_billing_test_s78` the range is 376-520 kB.
That is the **floor** per abandoned app, before a single workflow has run and
before the creator's own data schema is counted. The data schema is unbounded:
it is whatever the app's end users wrote.

**MEASURED.** Per-app roles are cluster-scoped and outlive even a
`DROP DATABASE`. That cluster currently holds **517** roles matching
`^app_[0-9a-f]{8}`.

**NOT CHECKED:** the byte size of the KV and storage residue. That it survives
is verified above; how much of it there is per app was not measured.

---

## 3. Deletion does not currently complete for any app that has a schema

This inverts the framing of the whole question, so it comes before the options.

**VERIFIED.** Two append-only triggers sit on `ON DELETE CASCADE` edges out of
`zeroship.apps`, and neither honours the `zeroship.audit_retention` GUC escape
that three other platform audit tables have:

- `migrated_migration_audit_append_only`
  (`db/migrations-ts/20260702000700_functions_triggers_comments.ts:21,40`), whose
  function body is `BEGIN RAISE EXCEPTION 'migrated_migration_audit is
  append-only'; END;` with no `TG_OP` test. The FK is
  `onDelete: "cascade"` (`db/migrations-ts/20260702000600_constraints_indexes_fks.ts:169`).
- `plan_change_events_immutable_trg`
  (`db/migrations-ts/20260702000700_functions_triggers_comments.ts:18`), same
  unconditional shape.

**VERIFIED.** Nothing later in the corpus replaces either function: grep for
`reject_migrated_migration_audit_mutation|plan_change_events_immutable` across
`db/migrations-ts/*.ts` returns hits in exactly one file, the one that creates
them.

**VERIFIED.** Every migration-apply outcome writes a
`migrated_migration_audit` row - `crates/zeroship-migrate-server/src/apply.rs` calls
`record_audit` at ten sites, and `crates/zeroship-migrate-server/src/migration_store.rs:301` is
the single `INSERT`. So the trigger fires for every app that has ever applied a
migration, which is every app that has a schema to drop. **The set of apps whose
schemas need dropping and the set of apps whose delete cannot complete are the
same set.**

**MEASURED**, on a scratch database `s126_orphan_demo` I created for this and
nothing else, reproducing both triggers from the migration source, **with a
control**:

```
### an app that has applied ONE migration
ERROR:  migrated_migration_audit is append-only
CONTEXT: ... "DELETE FROM ONLY "zeroship"."migrated_migration_audit" WHERE $1 = "app_id""
 still_present
             1

### the SECOND blocker, invisible until the first clears
ERROR:  plan_change_events is append-only (no UPDATE/DELETE) - the proration timeline is frozen

### control: an app with neither row
DELETE 1
```

The control is the load-bearing part: an app carrying neither row deletes
cleanly, so the failure is attributable to the trigger and not to the vehicle.

This reproduces `docs/pilot/2026-08-12-pending-tasks.md:1677` (#331) and
`tests/golden_path.sh` step 12, which already carries the finding and is
EXPECTED RED at HEAD.

**And the ordering makes a failure worse than a no-op.** `purge_app` deletes the
manifest keyspace FIRST and cascades SECOND. When the cascade aborts, the app row
is still there, the gateway still serves it, its schemas are still there, and
`manifests/<app_id>/` is empty. `tests/golden_path.sh:3886-3894` records this
reproduced identically in three consecutive runs. A retry cannot restore what
step 1 removed.

**VERIFIED.** A third refusal exists and is deliberate: an ever-invoiced app is
pre-checked and refused with a typed 409 (`crates/zeroship-control/src/registry.rs:344-365`),
because `invoice_lines.app_id -> apps` is `ON DELETE RESTRICT`. The comment
names the posture - "consistent with the anonymize-don't-delete financial-history
posture" - but the anonymize path it points at exists for `users`, not for apps.

---

## 4. The residue is invisible to every instrument the platform has

**VERIFIED.** Every fleet sweep in `crates/zeroship-control/src/cron/` takes its app list
from `journalled_apps` (`crates/zeroship-control/src/cron/workflow_engine.rs:344`), which
starts from the apps table:

```sql
   FROM zeroship.apps a
   JOIN pg_catalog.pg_namespace n
     ON n.nspname = 'app_' || a.id::text
```

**MEASURED** on the scratch database: before the delete that census returns the
app with `tables_present = 5`; after `DELETE FROM zeroship.apps`, it returns
**zero rows**, while `pg_namespace` still lists both schemas and both still hold
their rows, including a `__zeroship_workflow_runs` row whose `input` jsonb I had
seeded with end-user PII.

So an orphaned schema is not merely unswept. It cannot be *found* by any query
keyed on `app_id`, because the key is gone. Deploy retention, workflow
retention, blob GC, signal fan-out and the coverage census all inherit this: an
orphan is not counted in `apps_swept`, not counted in `apps_skipped`, and not
counted in `apps_unvisited`. It is outside the denominator.

This is directly relevant to tasks #76 (`e98b7751a`, one unreadable journal
stalling the fleet fan-out) and #97 (`7a81e6d49`, a census keyed on one table's
existence dropping apps from every coverage number). Both were fixed by making
the *enumeration* honest. **A reaper that walks `zeroship.apps` inherits the
fixes and still cannot see a single orphan**, because the orphans are exactly
the rows that are not there. Any reaper for this problem has to be keyed on
something else - see section 7.

**Field evidence that the state is reachable**, though not via a delete:
`docs/pilot/2026-08-12-pending-tasks.md:735` records a real database left with
"at least 8 live per-app schemas (`app_<uuid>`)" whose `apps` rows were gone,
and the consequence: "the app id -> owner/name mapping is lost." The cause there
was a dropped table, not a delete. The end state is identical.

---

## 5. Retention: what the tree claims that the residue contradicts

The abandoned schemas hold the app's END USERS' data. Four claims in the tree are
inconsistent with keeping it.

1. **A user-facing promise, in an email we actually send.**
   `crates/zeroship-mailer/src/templates/account_deletion_requested.txt:3-7`:
   "We received a request to permanently delete your zeroship account... After
   that date the deletion is irreversible." The erasure chain behind that promise
   is `crates/zeroship-auth/src/cron/account_reaper.rs` (headed "Account-erasure reaper
   (ISS-12 / GDPR Art. 17)") -> the owner-less apps it leaves ->
   `crates/zeroship-control/src/cron/orphaned_app_reaper.rs` -> `purge_app`. **VERIFIED**
   at each hop. The chain terminates in the two-step function quoted in section 1,
   which does not name a per-app schema. An erased user's apps' data survives the
   erasure that was promised to be irreversible.

2. **`docs/reference/db.md:1096` states something about app deletion that is
   false.** "App deletion stops local consumers, drops every slot with that app's
   exact prefix, and then drops the publication; workers retry this idempotent
   teardown when a control-plane removal poll fails." That describes
   `drop_namespace`, which has no production caller. This is corrected in the
   same commit as this document.

3. **`docs/reference/db.md:1285` sells `purge()` as the GDPR-erase lever** for
   end-user rows. It operates row-by-row inside the per-app schema, through the
   app's runtime. Once the app is deleted the runtime is gone and the schema is
   not, so the erasure lever becomes unreachable while the data remains.

4. **The auth audit sweep puts a 90-day ceiling on PII**
   (`crates/zeroship-auth/src/cron/audit_retention.rs:49-51`), and control's puts 12
   months on its audit trails. A per-app schema holding end-user PII with no
   horizon at all is not consistent with that posture.

This is therefore a data-protection question, not a disk question. The disk cost
is real but small (section 2); the liability is the point.

---

## 6. Is app deletion meant to be recoverable? THE EVIDENCE DOES NOT SAY

I looked for evidence in the schema, the handler, the SDK, the CLI, the MCP
tools, and the docs. Summary of what is there:

- **No tombstone, no grace, no restore endpoint, no `archiveApp`.** The one
  `archived_at`-on-apps mention in the tree is an unbuilt line in an archived
  status doc (`docs/archive/superpowers/zeroship-builder-status.md:78`).
- **No warning that deletion is permanent, anywhere a caller would see it.**
  `sdks/control/src/index.ts:311` is a bare two-line method with no doc comment.
  `sdks/mcp/src/index.ts:170` describes it as "Delete a zeroship app by UUID or
  name." There is no console UI in this tree to warn in, and no CLI `app delete`.
- **`docs/architecture/control-plane.md` never mentions `DELETE /apps/{id}` at
  all**, and `docs/reference/control.md` never mentions `apps.delete`. Grep for
  "delete" in the latter returns zero hits.
- The only place deletion semantics are stated as an expectation is
  `tests/golden_path.sh:3904`, "Teardown: the creator deletes the app, and its
  state goes with it" - one-way, never recovery - and that file is explicit that
  it "asserts what a creator is entitled to assume, not what the code currently
  does."

So the honest reading is: **nobody decided.** The current behaviour is not
option (c) "we deliberately never delete", because there is no recovery path
either. It is the third thing, and it is worse than either:

> **The handle is destroyed and the data is kept.** After a successful delete
> nobody - not the creator, not an operator, not a support request - can reach
> the app's data through any product surface, because the id-to-owner mapping is
> the row that was deleted. The data itself persists with no owner, no horizon,
> and no query that can find it.

That is not a policy. It is the residue of two half-decisions.

**The question I cannot answer from the tree, and that decides section 7:**

> If a creator deletes an app and asks for it back an hour later, is that
> supposed to work?

Two answers, both defensible, with different consequences:

- **(A) No - delete is final.** Then a grace window is an operational safety
  margin (retryable teardown, a window in which an operator can abort a bad
  reaping), never a product promise, and it must not be documented as one. The
  console/SDK acquires a "this cannot be undone" warning. Section 7 as written
  assumes this answer.
- **(B) Yes - delete is reversible for N days.** Then the tombstone is a
  *feature*, `DELETE` becomes a state transition rather than a destruction, a
  `POST /apps/{id}/restore` has to exist and has to restore the route registry
  and the OAuth client, and the reaper's grace is the product's promise. This is
  strictly more work and more surface, and it is the answer that makes today's
  behaviour a *near-miss* rather than a defect: the data is all still there.

I am not choosing between these. The rest of section 7 is written for (A) and is
marked where (B) would change it.

---

## 7. Recommendation (assuming answer A)

**Tombstone in the handler, reap in a cron, keyed on the tombstone and never on
absence.** That is option (b) from the framing, with four prerequisites, in this
order. The order is not negotiable: steps 1-2 are what make step 4 mean anything.

### 7.1 Make the delete able to complete at all

Nothing else matters until this lands. Recommend changing the two blocking FKs
from `ON DELETE CASCADE` to `ON DELETE SET NULL` in a NEW migration dated after
`20260820000100`.

Rationale over the alternatives filed at
`docs/pilot/2026-08-12-pending-tasks.md:1725`:

- **not (a') "give both triggers the `audit_retention` GUC hatch"** - it makes
  two more audit trails deletable to solve a problem that is not about deleting
  audit rows. `plan_change_events` is described in its own exception text as a
  frozen proration timeline; that is a billing-integrity claim and should not be
  weakened for a housekeeping reason.
- **not (c) "refuse the delete with a typed 409"** - it makes the impossibility
  honest without making deletion possible, and deletion has to become possible.
- **(b) `SET NULL`** keeps every audit row, keeps append-only literally true (no
  row is deleted), and matches the anonymize-don't-delete posture
  `crates/zeroship-control/src/registry.rs:344-365` already applies to billed apps. The
  audit row loses its app pointer, which is exactly what "the app is gone" means.

**MUST CHECK before landing:** `billing_reconcile.rs:1357` reads
`plan_change_events`; a nullable `app_id` changes that query's shape.

### 7.2 Reverse `purge_app`'s ordering

Registry state first, artifacts second. A failed teardown must leave a live app,
never an app whose manifests are gone and whose row is not. Today it is the
other way round and reproduces every run.

### 7.3 Tombstone

Add `deleted_at timestamptz` to `zeroship.apps` in a new migration.
`DELETE /api/apps/{id}` sets it, drops the route from the gateway feed, revokes
the OAuth client, and returns. The API call becomes fast and bounded, which is
the whole reason not to do option (a) synchronously: a `DROP SCHEMA CASCADE` over
a creator's real data is unbounded work behind an HTTP timeout, and a partial
failure orphans state with no retry - the exact defect 7.2 exists to remove.

Every existing reader of `zeroship.apps` has to grow `AND deleted_at IS NULL`,
including `get_gateway_snapshot`, the fleet censuses, and the orphaned-app
reaper's detection query. That is the real cost of this step and it should be
counted honestly.

### 7.4 Reap on the tombstone

A cron reaper walks `deleted_at IS NOT NULL AND deleted_at < NOW() - grace` and,
per app, tears down: three schemas, the role, the KV prefix, the storage prefix.
Per-app failures isolated and retried next tick, exactly like
`orphaned_app_reaper::tick`.

**The detection query must be a POSITIVE assertion about a row that exists, not
an anti-join against absence.** This is the single most important sentence in
this document. `orphaned_app_reaper` already learned the lesson and records it
(`crates/zeroship-control/src/cron/orphaned_app_reaper.rs:17-23`): a naive "delete
owner-less apps" sweep would have deleted the platform's own console, and the
guard is that the console asserts `system = true`. A reaper keyed on
`NOT EXISTS (SELECT 1 FROM zeroship.apps ...)` over `pg_namespace` has the same
shape as that naive sweep and a worse blast radius: pointed at the wrong
database, or at one whose `apps` table is empty for any reason, **every tenant
schema in the cluster qualifies**. `docs/pilot/2026-08-12-pending-tasks.md:735`
is a record of `zeroship.apps` being absent from a live database for an
unrelated reason; a reaper of that shape running that day would have destroyed
eight tenants' data.

Consequence: **the orphans that already exist cannot be reaped by this cron.**
They have no row to carry a tombstone. Clearing them is a separate, one-time,
operator-run command that prints its plan and requires confirmation - not a
cron, not a fleet sweep, and not part of this design.

Under answer (B) this section changes: the grace becomes a documented product
window, and 7.3 needs a restore endpoint.

### 7.5 What to fix now, before any of it

Two things land with this document, because both are wrong independent of which
option is chosen.

**The false claim at `docs/reference/db.md:1096`.** It describes
`drop_namespace`, which has no production caller. Corrected to say what actually
happens (slots are dropped worker-side; the publication, schema and role are
not).

**The residue instrument was undercounting.** `tests/golden_path.sh` step 12
counted the residue with `table_schema like '<app_id>%'`, which matches
`<app_id>` and `<app_id>_migrations` and **not** `app_<app_id>` - the workflow
journal is PREFIXED, so a suffix-open LIKE cannot see it. **MEASURED** on the
scratch database, same data, two patterns:

```
old pattern (like '<uuid>%')                  -> 1 schema,  1 table
new pattern (like '<uuid>%' OR = 'app_<uuid>') -> 2 schemas, 6 tables
```

The step is EXPECTED RED either way, so nothing changed colour - which is the
point. It was reporting a smaller residue than the one it exists to report, and
a future reader comparing "7 tables" against a real deployment would have
concluded the fix was working. Both the BEFORE and AFTER counts are widened, and
the comment now states what the count still cannot see (the per-app role, KV
keys, storage prefix, scheduler timers - none of which are schemas).

---

## 8. The case against this recommendation

Required by the brief, and I think it is genuinely strong on two of its three
legs.

**8.1 A fleet-walking cron that deletes tenant data is the most dangerous shape
of code in this repository.** Every other destructive sweep here deletes ROWS
inside a bounded scope; this one issues `DROP SCHEMA CASCADE` against a whole
tenant. A bug in the retention sweep costs some rows. A bug in this costs a
tenant, and it is unrecoverable in a way that leaked disk never is - there is no
backup in this tree (`docs/pilot/2026-08-12-pending-tasks.md:751`: "I searched
for a dump and found none"). The repository has already destroyed a shared
database once this month, and the mechanism was not a wrong `DROP` but a *stale
identifier* being helpfully corrected (#208). This design hands that same class
of accident a bigger weapon. Leaked disk has never woken anyone; a wrongly reaped
tenant would end the platform.

**8.2 It is 376 kB per abandoned app, and I measured that myself.** At any
plausible pre-launch churn the residue is noise. Spending the riskiest code in
the repo on it fails a cost-benefit test that nobody has run, because nobody has
measured the actual delete rate - there are no production users to have one.

**8.3 And the honest counter to 8.2, which is why I still recommend building
it.** The cost is not disk, it is that the data has no owner and no horizon
(section 5). "We never delete anything" is exactly how a platform accumulates an
unbounded liability that nobody owns: every abandoned schema is an end user's
rows sitting in a live database that the platform has promised, in an email it
already sends, to have permanently erased. That liability compounds with signups
and is not visible in any dashboard, because section 4 established that no query
in the tree can even enumerate it. The disk argument and the liability argument
point opposite ways, and the liability argument is the one that gets worse with
time.

Where 8.1 does land: it is an argument about the *shape* of the reaper, not
about whether to have one, and 7.4 is written to answer it. If the operator is
not willing to accept a cron that drops schemas, the fallback is not "leave it" -
it is 7.1 through 7.3 plus an operator-run teardown command, which gets the
liability bounded and the accident surface manual.

---

## 9. What this does not settle

- **The deploy `blobs/` keyspace is a separate, larger problem this proposal does
  not solve.** `BlobStore` has no `delete_blob` and no `list_blobs`
  (`crates/zeroship-bundle/src/blob.rs:57-163`), so no deploy blob is reclaimable by any
  code path, whether or not an app is deleted. A reaper for deleted apps cannot
  fix that; it needs a refcount or a mark-and-sweep the trait cannot express.
- **The orphaned scheduler timers need their own fix.** `workflow_scheduler_timers`
  and `_inflight` carry `app_id` with no FK, so they survive a delete as live
  work items. Adding the FK is the obvious answer and is a schema change with its
  own blast radius; it is not folded into 7.1.
- **The `oauth_clients` delete is keyed on a derived value, not on a read.**
  `crates/zeroship-control/src/registry.rs:371-381` deletes
  `WHERE client_id = client_id_for_app(id)`. Any app whose OAuth client was ever
  provisioned under a different client_id would keep that row and its
  `app_user_identities` / `oauth_grants` children. Whether such an app can exist
  was **NOT CHECKED**.
- The `invoice_lines` `ON DELETE RESTRICT` refusal is untouched by this proposal.
  An ever-invoiced app still cannot be deleted at all, and the "anonymize
  instead" path its error message promises does not exist for apps. That is a
  separate decision with the same shape as this one.
- The exact grace window is not chosen here. It is meaningless to choose before
  section 6 is answered: under (A) it is an operational number, under (B) it is a
  product promise.
