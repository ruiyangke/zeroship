# Moving the workflow journal out of creator databases

**Status.** PROPOSED, with Plan steps 1 and 2 in place and step 3 landed for every run call but
`restart`. `db/migrations-ts/20260919000000_workflow_journal.ts` installs the journal into
`workflow_manager` and grants it to the role already serving that schema, and
`journal_is_installed_and_served_by_one_role` in
`crates/zeroship-workflow-server/tests/platform_schema.rs` holds that posture. The service links
the engine and answers `status`, `signal` and `transition` over its own journal, establishing
its own ingress epoch; `restart` refuses, naming the prerequisite it still needs. Nothing in
production writes this journal yet - there is no `start` endpoint - so step 5 is what gives the
endpoints rows to operate on.

Open 3 names three pieces behind a store of its own: splitting the corpus, severing the
service's control-plane reads, and giving the service a deployment site. The first and third
have landed, and the second is nearly done. The two tables the service owns have moved into
`workflow_manager`, which deletes a cross-database write rather than transporting it, and the
policy inputs now arrive over `POST /v1/app-facts` rather than a binding on Control's schema.
The transport fence that blocked reaching Control is decided and built - plaintext now reaches
only peers an operator named. Two reaches into `zeroship` remain: placement eligibility in
`crates/zeroship-workflow-server/src/coordinator.rs`, which stays deliberately because its two
reads exist so they can disagree, and the worker registry lookup that authenticates every
call, which is deferred by decision.

The journal is installed into the creator's own schema today and written by the worker over the
creator's own connection; this moves storage and the durable fold into the workflow service,
leaving execution where it is.

This is a sibling of `docs/proposals/2026-08-28-app-database-decoupling.md` and should land
BEFORE it. See Sequencing: that design breaks journal appends if the journal is still where it
is.

---

## What is true today

**The engine is embedded in the worker.** `crates/zeroship-workflow/src/service/mod.rs` opens
with "Workflow engine embedded in customer workers and local development", and
`crates/zeroship-workflow/src/service/store.rs` with "Customer-bound journal execution through
the shared Rust ORM". The store holds a `Database`, a `BackendHandle`, a `DbBinding` and a
`ProjectKeySource`, so journal rows are written through the same ORM, the same binding and the
same pooled connection as creator data.

**The split is deliberate and documented.**
`crates/zeroship-workflow-server/src/lib.rs`: "Workflow metadata coordination. Customer workers
own execution and storage."

**The journal lives inside the creator's schema.**
`crates/zeroship-worker/src/workflow_creator.rs` resolves a binding and takes
`binding.schema()`; the control-side caller
(`crates/zeroship-control/src/publication/journal.rs`) derives the same name. The tables are
`__zeroship_workflow_*` inside that schema, and `crates/zeroship-workflow-schema/schema/schema.ts`
declares around twenty of them - `app_state`, `payloads`, `broadcasts`, `schedules`,
`occurrences`, `tasks`, the publication and page tables, the receipt tables - nearly all keyed
with an `app_id` column.

**One journal serves many apps already.**
`crates/zeroship-workflow-schema/src/lib.rs`, under a heading called "The schema is not an app":

> Nothing here takes an app id, and no name here should suggest one. A journal belongs to a
> creator database, and one schema holds the journals of every app in it - the `app_id` COLUMNS
> inside the journal are the tenant discriminator, and `STAMP_ROW_ID` is one row per journal,
> not one per app.

**Provisioning is per schema, through the migration service.**
`crates/zeroship-workflow-server/src/journal.rs` builds a `SchemaBundle` with
`SCHEMA_PLACEHOLDER` substituted, and `ensure_journal`
(`crates/zeroship-workflow-server/src/api.rs`) applies it. It has two callers: control when an
app registers, and a worker whose host refused the journal it found.

**The service already has its own storage and a stated boundary.**
`crates/zeroship-workflow-server/src/config.rs` declares `workflow.database_url` with the
comment "Platform coordination metadata login; no customer database credentials", and
`db/migrations-ts/20260911000000_workflow_coordination.ts` creates the `workflow_manager`
schema, a `zeroship_workflow_migrator` role and a `zeroship_workflow` login whose search path is
`["workflow_manager", "pg_catalog"]`.

**The creator-facing seam is small.** `crates/zeroship-workflow/src/backend.rs` defines
`WorkflowBackend` as `start`, `status`, `signal`, `transition`, `restart`, `read_step_output`
and `read_output`. `crates/zeroship-workflow-v8/src/lib.rs` already composes it through a
`WorkflowBackendFactory` with `Service` and `Ready` variants.

**Durability does not rest on transactions.** `crates/zeroship-workflow/src/execution.rs`:
executors receive replay, and "step idempotency keys must carry it". Nothing requires a step's
data write and its journal record to commit atomically.

---

## Four defects, three of which exist at 1:1

**1. A creator can drop the platform's journal.** The migration service does
`ALTER SCHEMA ... OWNER TO` the migrator role, and a schema owner's privileges are implicit and
cannot be revoked. So the schema the platform is actively driving runs against is owned by the
tenant. `docs/proposals/2026-08-28-migration-record-consolidation.md` accepts exactly this for
the *migration* journal, on the ground that "it is their database and corrupting it breaks only
them" - which is true there and false here, because the platform is mid-execution against this
one.

**2. Dropping a database destroys the journal.** Harmless while one app owns one database.
Under sharing it destroys the workflow state of every app bound to that database, not just the
one doing the dropping.

**3. Column-level grants break journal appends.** This one is live and dated.
`docs/proposals/2026-08-28-app-database-decoupling.md` deletes the blanket
`GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA` and the prospective
`ALTER DEFAULT PRIVILEGES` from `runtime_role_provisioning_sql`
(`crates/zeroship-migrate-server/src/apply.rs`), and regenerates explicit per-column grants
**from the creator's IR**. The journal tables are not in that IR - they arrive in a separate
bundle - so they receive no grants and the worker loses the ability to write them. The current
arrangement works only because the blanket grant covers everything in the schema, and that
grant is what goes.

The tripwire for this is already in the tree, and nothing names it.
`runtime_provisioning_does_not_narrow_table_access_by_name`, in that same file, asserts the
provisioning still contains `ON ALL TABLES IN SCHEMA`. Deleting the blanket grant turns that
test red, and the tempting repair is to relax its assertion - which lands this defect rather
than catching it. Whoever deletes the grant has to add the journal's tables to the generated
set in the same change.

**4. Under N:M the journal's home is not a function.** `crates/zeroship-worker/src/workflow_creator.rs` takes the schema
off whichever binding `provider.resolve(scope)` returned. With one binding that is
deterministic. With several it is not, and an app's runs could split across two schemas with
each half invisible to the other and nothing raising.

The fourth is dormant, and what holds it dormant is narrower than it looks. The journal's
schema is not resolved from a creator binding at all: `app_schema` in
`crates/zeroship-worker/src/workflow_host.rs` composes it as
`app_derivation::schema_name(app)`, which returns the app id, and hands it to the binding
that `workflow_creator.rs` later reads back. So plurality alone does not reach it - an app
gaining several databases leaves the journal where it was, because nothing consulted
`SuppliedAppBindings` to place it.

What makes it live is `app_derivation::schema_name` ceasing to return the app id. That
function is deliberately narrower than what is built around it, and when it widens, the
journal's home becomes a choice with no owner: "the app's primary database" holds only
until a creator changes which database is primary, at which point every existing run sits
in a schema the app no longer points at - invisible, not deleted, and nothing raising.

Relocating removes the question rather than answering it. A journal in `workflow_manager`
has no creator schema to derive.

The first three are true now.

---

## The design

**Execution stays in the worker. Orchestration and storage move to the service.**

```
  TODAY
    worker   V8 task executor + durable fold + journal store (SQL, creator schema)
    service  metadata coordination, placement, journal provisioning

  TARGET
    worker   V8 task executor + WorkflowBackend RPC client (creator seam)
    service  durable fold + journal store (SQL, workflow_manager) + coordination
```

The seam already exists and is narrow. `WorkflowBackend` is the whole creator-facing surface,
`crates/zeroship-workflow/src/engine.rs` is "Pure durable-workflow fold and DTO contracts" with
no storage in it, and the V8 binding is already built to take a backend rather than a database.

**The journal becomes ordinary service schema.** It is installed into `workflow_manager` by a
platform migration, with one stamp row for the whole installation. That is what `STAMP_ROW_ID`
already describes, now over a set of apps scoped by the service rather than by a creator
database.

Not at service boot. The service has no installer - `run` in
`crates/zeroship-workflow-server/src/server.rs` connects, verifies and serves - and its login is
forbidden the DDL it would need: `Coordinator::verify` in
`crates/zeroship-workflow-server/src/coordinator.rs` refuses to start when
`has_schema_privilege(current_user,'workflow_manager','CREATE')` is true, and
`crates/zeroship-workflow-server/tests/platform_schema.rs` asserts that granting CREATE makes
`verify` fail. Installing at boot would mean deleting a shipped security contract. A platform
migration is also the route `workflow_manager` itself arrives by, in
`db/migrations-ts/20260911000000_workflow_coordination.ts`; the journal joins it there.

The other installer this workspace owns cannot target this schema either. `apply_schema_bundle`
in `crates/zeroship-migrate-server/src/bundle.rs` calls `provision_database` unconditionally,
and `provision_migrator` reassigns schema ownership before `provision_runtime_app_role` mints a
login holding DML on every table in the schema. Pointed at `workflow_manager` that would take
the schema from `zeroship_workflow_migrator` and give a runtime login full reach over the
manager's queue. It stays the installer for a journal in a creator schema, which is a different
target with different owners.

**The worker holds no journal credential**, which is the property that makes the whole thing
safe without building anything. See Why it is this way.

### What is deleted

```
  ensure_journal + its route            crates/zeroship-workflow-server/src/api.rs
  Journal / journal_bundle / bundle_for crates/zeroship-workflow-server/src/journal.rs
  JournalManager, JournalError          crates/zeroship-control/src/publication/journal.rs
  the journal ensure at app registration control's deploy path
  the worker's journal repair path      crates/zeroship-worker/src/workflow_creator.rs
  zeroship-data-orm from the engine      crates/zeroship-workflow/Cargo.toml
```

`SCHEMA_PLACEHOLDER` stays. A fixed target schema does not remove the need for it: the generated
PostgreSQL artifact carries the placeholder quoted, and the platform migration substitutes
`"workflow_manager"` into it exactly as a creator bundle substitutes a creator schema. The
quoting matters, because substituting the bare word would also rewrite the stamp table's name.

The first four entries are not available yet. Step 1 left `ensure_journal` and
`Journal`/`bundle_for`/`journal_bundle` byte-identical on purpose, and they stay until a reader
exists in `workflow_manager` to replace what they serve. Read this list as the end state, not as
work unlocked by the installation.

That last one is a dependency-boundary improvement the AGENTS.md invariant already gestures at:
`zeroship-workflow-schema` is a leaf so a service can install the journal without depending on
the engine. Read it as the last step of the move rather than a deletion available on its own,
because the ORM is not confined to the store: every module under
`crates/zeroship-workflow/src/service/` reaches it - activation, signals, continuations,
collection, app, models, control, frontier, delivery - while
`crates/zeroship-workflow/src/engine.rs` names it nowhere. That asymmetry is exactly what makes
the split clean, and it is also why what moves is the whole `service/` tree, not one file.

### What it does NOT decide

Whether `workflow_manager` should be promoted from a schema in the control database to a
database of its own. It has its own migrator role, its own login and its own search path
already, so the promotion is contained and can be made on capacity grounds later. This design
only requires that the journal live in the workflow service's storage, not which physical
database that is. See Open 3.

---

## Why it is this way

**The worker must not hold a credential to a shared journal.** This is the load-bearing reason
for RPC rather than a second DSN. The journal's only tenant separation is its `app_id` columns:
no roles, no RLS, one stamp row covering every app in it. That is safe today only because the
journal sits inside the app's own schema and is reached through the app's own binding, so the
data plane's role fence covers it for free. Put the same tables in a shared platform database
that the worker reaches by SQL, and every app's workflow state sits behind a column comparison
inside the one process that executes creator code.

Note that `db_posture` (`crates/zeroship-worker/src/db_posture.rs`) would not catch it: it
refuses a login that can resolve the `zeroship` schema, and a workflow database has no such
schema. The check would pass while the principle behind it was violated. RPC removes the
question rather than answering it.

**Why not give the worker the ORM and keep one path?** This is the first question a reviewer
asks, and it deserves an answer in the document rather than in a thread. The appeal is real:
production would exercise the same store the dev tier does, and the divergence recorded under
SQLite dev tier would not exist.

It fails on where the fence would have to live. The journal's tenant separation is the `app_id`
column on its tables - `crates/zeroship-workflow-schema/schema/schema.ts` declares no role, no
row-level security and no grant of any kind. That is sound today only because the journal sits
inside the creator's schema and is reached through the creator's binding, so the data plane's
role fence covers it without the journal owning one. Move the tables into shared storage and
keep SQL in the worker, and the only thing standing between one app's runs and another's is a
column comparison evaluated inside the process that executes creator code. AGENTS.md settles
that case directly: privilege follows the process, and a privileged database function the worker
can invoke is not a security boundary.

Giving the journal a fence of its own is the honest version of the idea, and it is a larger
project than this one. Every table would need a policy, the worker would need a per-app
principal rather than a shared login, and whatever scopes the connection cannot be something the
executing process can choose for itself. It also does not deliver the single path that motivated
it: SQLite has no row-level security, so the dev tier diverges again - the same tax, moved from
"RPC against SQL" to "fenced against unfenced", and now sitting on the tenant boundary instead
of beside it.

So the arrangement below is not "RPC because RPC is nicer". It is the only one where the fence
is not inside the process running untrusted code. If the journal ever is reached by SQL from
outside the service, this paragraph is the thing that has to be answered first.


**Nothing needed a shared transaction.** Replay plus idempotency keys is the durability model,
stated in `crates/zeroship-workflow/src/execution.rs`. A step's data write and its journal record have never been atomic in
any sense a caller could rely on, so moving the journal to another process removes a property
nothing was using. Under N:M it would have been lost anyway, since a step writing to database B
cannot share a transaction with a journal in database A.

**The boundary is already declared.** `workflow.database_url` is documented as a "Platform
coordination metadata login; no customer database credentials". The service has been keeping
platform state out of customer databases since it was written; the journal is the piece that
did not move.

**The creator seam is seven methods; the execution seam is its own.** It would be a much larger
proposal if the storage seam were the RPC boundary, because the store spans roughly twenty
tables. It is not: the engine's fold is pure and the creator-facing backend is narrow, so the
whole engine moves server-side and the wire carries `start`, `status`, `signal`, `transition`,
`restart`, `read_step_output` and `read_output`.

Say plainly that those are the creator-facing surface and not the whole wire. A worker also
has to be given work and report it, and that path is not a trait at all. `DeliverySlot` in
`crates/zeroship-workflow-runner/src/delivery.rs` is what production runs, and it calls
`AppWorkflows::accept_job`, `heartbeat_job` and `complete_job` directly, in process. There is a
`TaskTransport` trait beside it, but its only consumer, `RunnerSlot`, appears solely in tests -
do not plan against it, and do not read `WorkerTasks` implementing it as evidence that the
protocol is already abstracted. `WorkerTasks` is production, in its other role as `TaskPayloads`.

Those three direct calls are what has to cross, and they are the half carrying the durability
properties: `renewal` in `crates/zeroship-workflow-manager/src/queue.rs` is what advances the
manager's evidence that an execution began, `complete_job` carries the outcome batch the fold
consumes, and `settle_attempt` in `crates/zeroship-workflow/src/service/journal.rs` counts a
reported execution of a run body. A reader who takes the creator seam as the whole surface will
under-plan the cutover.

---

## SQLite dev tier

The embedded store stays. `crates/zeroship-workflow/src/service/mod.rs` already says the engine is embedded "in customer
workers **and local development**", and `WorkflowBackendFactory` already carries both a
`Service` and a `Ready` variant, so both paths exist by construction rather than by a flag.

Two consequences to record in `docs/reference/sqlite-divergences.md` rather than leave silent:
the dev tier keeps the journal in the local file and therefore exercises the SQL store that
production no longer uses, so a dev-tier pass is not evidence about the production write path;
and `SCHEMA_PLACEHOLDER` survives for that tier alone.

---

## Sequencing

**This should land before the app-database decoupling**, for defect 3. If column grants land
first, journal appends break, and the only repairs are an interim grant path in the journal
installer that would be deleted immediately afterwards, or shipping both projects as one change.

```
  1. this proposal       journal leaves the creator schema
  2. the decoupling      column grants land with nothing in the creator schema
                         that they must cover beyond the creator's own IR
```

Neither ordering is forced by anything else: defects 1 and 2 are worth fixing on their own, and
the decoupling needs nothing from the journal except that it not be in the way.

---

## Plan

Steps 1 to 3 each landed on their own and are verifiable on their own. Step 4 does not: its
wire and client halves would land green but unwired, and its server half shares step 5's flag
day, because the job endpoints it changes are the live claim path rather than a new one nobody
calls. Land the halves together rather than the client half early. Unwired it changes no
existing test and has to survive step 5's churn, and the timeout budget recorded under the
heartbeat has to be settled by whoever writes the server side in any case.

**What is easy, and what is not.** Neither seam is the one to start from. The creator seam
looks narrow - `WorkflowBackend` has seven methods and the factory in
`crates/zeroship-workflow-v8/src/lib.rs` takes a third arm cleanly - but those seven divide
into wiring, a service capability, one blocked on the payload seam, and two whose `Vec<u8>`
shape may be wrong for a remote seam at all; step 5 counts them. And the execution seam is not
`TaskTransport`: the type that dispatches through it is built only by tests, so a remote
implementation of it would serve nobody. Step 4 names what actually crosses, which is the three
journal calls beside the manager calls the worker already makes. Steps 1 to 3 are preparation,
and steps 4 and 5 land together at the cutover.

The machinery to carry it exists. The worker already reaches the manager over HTTPS through
`zeroship-workflow-client` with a validated `Transport`, so job delivery is already a remote
protocol. This extends a working client rather than inventing one.

1. **Install the journal into `workflow_manager` as a platform migration.** Nothing reads it
   yet. Verify that the schema installs, that one stamp row covers the installation, and that
   the creator-schema path is untouched.

2. **DONE. `AppBackend` binds the store of the journal it is given.**
   `into_backend` in `crates/zeroship-workflow/src/service/backend.rs` takes the journal, checks
   the handle's binding against that journal's policy registry, and swaps the store.
   `a_backend_reads_and_writes_the_journal_it_was_built_over` in
   `crates/zeroship-workflow/src/service/tests/backend_journal.rs` builds two stores, binds a
   handle to the second, and reads a run only the first holds; its neighbour
   `into_backend_refuses_a_journal_from_another_policy_registry` binds a foreign store under the
   handle's own registry and refuses one under another. What the
   worker passes it is a service opened over the creator database
   (`crates/zeroship-worker/src/workflow_creator.rs`). The work is to pass a service opened over
   the service's store instead.

   Two things bound how far this reaches. `same_binding` compares registry identity, app and
   policy generation, so both services must share the host's `HostPolicies`; that holds for any
   store, since `WorkflowService` keeps store and registry as separate fields. And the worker has
   no database handle to the service's schema at all - it reaches the manager over HTTP - so it
   waits for step 5. Two places hold a handle to bind: the test fixtures in
   `crates/zeroship-workflow/src/service/tests/backend_journal.rs`, which build two stores and
   read through a handle bound to the second, and the workflow server, where step 3 put
   `WorkflowService` and `AppWorkflows` behind `RunService` in
   `crates/zeroship-workflow-server/src/runs.rs`.

3. **PARTLY DONE - `status`, `signal` and `transition` are served; `restart` refuses.** The
   endpoints, their `svc/worker` grants, the request envelopes, the `RunFailure` refusal
   envelope and the client methods are in; `RunService` in
   `crates/zeroship-workflow-server/src/runs.rs` binds an app per request from the server's own
   policy registry, and `bind` in `crates/zeroship-workflow-server/src/api/runs.rs` takes the
   worker from the credential that verified the call rather than from the body, so a request
   cannot name a placement its caller does not hold.
   `status_answers_from_the_service_journal` in
   `crates/zeroship-workflow-server/tests/http_runs.rs` reads a run's state back through the
   endpoint and compares it against the row, and
   `a_signal_served_over_the_wire_is_in_the_journal` does the same for a write.

   **The ingress epoch is established, and survives the reinstall.** `RunService::app`
   reinstalls the policy snapshot on every request and `PolicySnapshot::lease` starts every
   snapshot without an epoch, so the epoch is carried forward at that call site rather than
   inside `install`: a `None` arriving on a manager lease is the manager saying no
   responsibility is open, and the two install paths must not be unified.
   `an_established_epoch_survives_the_next_requests_policy_reinstall` binds the carry-forward by
   abandoning the recovery scope, so a second served request can only have been served on the
   epoch the first one established, and closes that epoch in a third arm as its control.
   `closing_the_journals_epoch_retires_the_carried_forward_one` binds what licenses carrying it
   at all: `require_open_epoch` reads the journal's closed epoch inside the caller's own
   transaction on every mutation, so a carried value is a claim rechecked at use time rather
   than a grant that keeps admitting.

   **`restart` is the one still fenced, and for its own reason.** It resolves a deployment
   before it reaches the epoch. Under `RestartOptions::default()` the effective policy is
   `Latest`, so `active_deploy` and `exact_target` in
   `crates/zeroship-workflow/src/service/control/restart.rs` run first; `retained_source` is the
   `Started` path rather than the default one, and both end at `require_journal_hold`. That
   check is a READ - `admission_generation` in
   `crates/zeroship-workflow/src/service/deployment_retention.rs` is one `find` - so what
   restart lacks is not authority to create a hold, but a hold and a `deploys` row that
   something else created.
   `ingress_serves_signal_and_transition_while_restart_still_lacks_its_source` pins all of it:
   the two that serve, the one that refuses, and a `status` control that needs neither.

   **Nothing in production writes this journal, which is what step 5 is for.**
   `WorkflowBackend` (`crates/zeroship-workflow/src/backend.rs`) declares `start`, `status`,
   `signal`, `transition`, `restart`, `read_step_output` and `read_output`; `start`,
   `read_step_output` and `read_output` have no `ServiceEndpoint` in
   `crates/zeroship-core/src/service_identity.rs`. `record_verified` in
   `crates/zeroship-workflow/src/service/deploys.rs` is the only writer of the deploys table and
   every path to it needs an `AppDeployments` this service never installs. So the endpoints
   above operate on rows nothing here produces, which is why
   `crates/zeroship-workflow-server/tests/http_runs.rs` seeds by raw SQL. That is a sequencing
   constraint on steps 4 and 5 rather than a gap in this step.

4. **Carry the three direct calls across, merged into the claims that already cross.** This is
   the step that earns its own review, and it is not "add a remote `TaskTransport`".
   `WorkerTasks` implements that trait in production, but the type that dispatches through it,
   `RunnerSlot` (`crates/zeroship-workflow-runner/src/lib.rs`), is constructed only by tests;
   the production job path is `DeliverySlot` over `JobTransport`
   (`crates/zeroship-workflow-runner/src/delivery.rs`). Building against `TaskTransport` would
   ship a remote implementation of something the worker never calls.

   What crosses is `AppWorkflows::accept_job`, `AppWorkflows::heartbeat_job` and
   `AppWorkflows::complete_job`, all in `crates/zeroship-workflow/src/service/delivery.rs`, each
   with exactly one production call site in `crates/zeroship-workflow-runner/src/delivery.rs`.
   Merge them into the manager calls beside them - the assignment rides the claim reply, one
   renewal carries both leases, the frontier rides the settlement. **No new endpoint is needed:**
   `WORKFLOW_JOB_CLAIM`, `WORKFLOW_JOB_HEARTBEAT` and `WORKFLOW_JOB_SETTLE` already exist in
   `crates/zeroship-core/src/service_identity.rs` with their `svc/worker` grants, `DeliveryGrant`
   in `crates/zeroship-workflow-manager/src/queue.rs` already implements `JobLease`, and
   `WorkflowHttpState` already holds both the coordinator and `RunService`. That, rather than the
   round-trip count, is why merging beats bolting on separate endpoints: the merged handler is
   the smaller one.

   **Only the heartbeat sits immediately beside its neighbour.** `renew` calls
   `JobTransport::heartbeat` and `AppWorkflows::heartbeat_job` in one bounded block. The claim
   does not: `DeliverySlot::run` dispatches `Activate`, `Reconcile`, `Cron`, `Management`,
   `ReleaseHold`, `Collect`, `Close`, `Fanout` and `Propagate` before `accept_job`, which is the
   fall-through for `Advance` alone, so a merged claim reply carries an assignment for one
   operation kind among ten. `Queue::claim_authorized` does not filter by operation - the
   restriction is on the publish side, in `worker_operation`
   (`crates/zeroship-workflow-manager/src/coordinator/jobs.rs`) - so all ten really are
   deliverable. And completion is a pipeline rather than a pair: `complete_job` runs in
   `execute`, and `acknowledge` settles the receipt it returned.

   **Take the heartbeat first; its server half waits.** Production journals per app
   into the creator's own schema - `JournalLocation::CreatorSchema` in
   `crates/zeroship-worker/src/main.rs` - while `RunService` holds one platform
   `workflow_manager` schema, so a merged server handler would have no rows to touch. The
   client, wire and DTO halves land green; leave
   `crates/zeroship-workflow-server/src/api/jobs.rs` alone until the cutover animates it.

   **Do not add a test module to `service/delivery.rs` to reach `TaskRenewal`.** Its fields are
   private, and that guard holds by ABSENCE: the file declares no `#[cfg(test)]` and no
   `mod tests`, so no test is a descendant of the module and the only constructions of the type
   are the ones inside `heartbeat_job` itself. `DeliveryGrant`
   (`crates/zeroship-workflow-manager/src/queue.rs`) has equally private fields and IS built
   field by field by a test, because that file does declare an inline test module. A merged
   heartbeat needs a second constructor; put the tests that exercise it in
   `crates/zeroship-workflow/src/service/tests/delivery.rs`, where they still cannot reach the
   fields.

   **Two measurement traps here.** `impl JobTransport for` misses an implementor -
   `crates/zeroship-workflow-v8/tests/runner.rs` spells it fully qualified - so a sweep for the
   test doubles to update has to match `JobTransport for` instead. And the merged body adds a
   journal object to the delivery envelope while the end-to-end fixture
   (`crates/zeroship-workflow-server/tests/support/server_process.rs`) caps request bodies far
   below the production default, with
   `queue_routes_authenticate_before_body_and_reject_open_metadata` probing that boundary on
   purpose. Re-measure there rather than assuming headroom.

   **The timeout budget does not survive the merge, and this step has to decide what moves.**
   Today the two calls spend their budgets in sequence under one `OPERATION_TIMEOUT`
   (`crates/zeroship-worker/src/workflow_host.rs`): the wire half answers to the client's own
   default timeout and the journal half to `ATTEMPT_IO_CEILING`
   (`crates/zeroship-workflow/src/service/delivery.rs`), and the two fit inside it exactly.
   Merged, both run inside ONE client-bounded request, while the server must fit
   `authenticate` (`crates/zeroship-workflow-server/src/api/jobs.rs`), the manager's queue
   transaction, AND a journal attempt carrying that same ceiling - so the server's worst case
   exceeds what the client will wait for. That is arithmetic over four constants rather than a
   measurement, and none of the three candidates is chosen: the client's timeout for this
   endpoint, the attempt ceiling on this path, or a tighter bound inside the server. The other
   two pairs do not have it. `accept_job` runs under no bounded wrapper, so there is no shared
   budget to overrun, and settle already retries, which absorbs a longer server call where a
   single `?` cannot.

   **One guard has no merged equivalent.** `renew` re-reads the grant's remaining time BETWEEN
   the two calls, so a manager reply arriving after the old grant lapsed cannot be followed by
   a journal write. Merged, the journal half has already committed on the server before the
   runner evaluates anything, which admits a case the split path refuses. Name it where it
   lands rather than leaving it to be found.

   **The partial commit moves rather than goes.** The journal call takes the manager call's
   output as its grant, so the server commits the manager half first and the journal half
   second, across two stores with no shared transaction. Merging hides which half succeeded
   from the runner; it does not make them atomic.

   **Do not write the server handler ahead of the cutover.** Not because it would sit unwired,
   but because it would be a LIVE endpoint reading a merged body whose journal half it cannot
   serve, and a client speaking the merged contract would break against it at once.

   **The port trait is not what this step is for.** The client validates nothing about the
   journal half - every check it makes is on `Delivery` fields it already owns - so carrying
   that half as an opaque value loses no guarantee the client exercises, and moving to
   associated types later touches one method signature and one implementation. Take the trait
   only if the server half is commissioned in the same breath; otherwise it is a bet on a
   schedule rather than an argument from the code.

   **What must survive, and where each is enforced.** The manager's evidence that an execution
   began is `renewal` in `crates/zeroship-workflow-manager/src/queue.rs`, NOT any method named
   `heartbeat_job` - that name resolves to three different methods in three crates, and a
   mutation applied to the wrong one proves nothing. `renewal` counts on the first renewal of an
   attempt, so a deferred claim spends no budget; it also means the manager half must keep
   counting before the journal half runs, or an attempt that reached creator code goes uncounted
   and redelivery unbounded. The outcome batch the fold consumes reaches `fold_outcomes`
   (`crates/zeroship-workflow/src/engine.rs`) through `tasks::complete_in` and `frontier::apply`;
   note that `crates/zeroship-workflow/tests/execution.rs` calls `fold_outcomes` directly and
   bypasses `complete_job`, so that target alone is a false green for this step. Counting a
   reported execution once is `settle_attempt` in
   `crates/zeroship-workflow/src/service/journal.rs`, which reads the held count inside the same
   transaction and so survives relocation unchanged. Verify by mutation rather than by suite.

   **An assignment does not always arrive with a claim, and the reply must say so.**
   `tasks::assign` answers `Busy` at `max_running` or with admission or dispatch off, and
   `Unavailable` with no available deploy; `accept_captured` answers `Deferred` when the run is
   not due and `Settled` on a stale frontier or a replayed receipt. The merged reply carries all
   three `JobAcceptance` arms or it is lossy.

   **Three things no step owns yet, and this one cannot land without the first.** `RunService`
   installs no deployments source - `crates/zeroship-workflow-server/src/runs.rs` opens the
   journal without the `.with_deployments(...)` that `crates/zeroship-worker/src/workflow_creator.rs`
   and `crates/zeroship-cli/src/workflow/host.rs` both pass - and `tasks::assign` opens with a
   `deploys` lookup, so a merged claim on today's server can never return a task. The other two
   belong to step 5: an HTTP `WorkflowBackend`, and the `TaskPayloads` seam that
   `crates/zeroship-worker/src/workflow_creator.rs` constructs on `WorkerTasks` - where the
   half that cannot merge is `read` rather than `stage`, as step 5 records.

   **That first one is not a line of wiring; it is a credential question.** `AppDeployments`
   (`crates/zeroship-workflow/src/service/deployments.rs`) holds a `BlobStore`, a byte budget
   and a map of hold clients - nothing the server could not have - so installing one compiles.
   It would also do nothing. None of the four run calls the server serves reads
   `self.deployments`, and no endpoint it serves reaches `record_verified`: every path to that
   writer runs through the runner's delivery loop, which
   `workflow_process_dependencies_follow_crate_ownership` forbids the server to reach. The
   table therefore stays empty, and `__zeroship_workflow_run_deploy` is an `ON DELETE RESTRICT`
   foreign key, so the journal cannot hold a run at all until something writes a deploy. That
   is why `crates/zeroship-workflow-server/tests/http_runs.rs` seeds both by raw SQL.

   **And the hold `restart` waits on belongs to another principal.** The journal holder is
   `HoldScope::for_app`, while the server's `ControlHolds` takes `QueueDeploymentHolds`, whose
   scope is the queue's. `crates/zeroship-core/src/service_identity.rs` grants `svc/workflow`
   the queue hold pair and `svc/worker` the direct one, and
   `crates/zeroship-workflow/src/deployment_holds/remote.rs` refuses any signer that is not an
   enrolled worker instance. Letting the service take a journal hold means granting it an
   endpoint only a worker may call today - the same credential question Open 3 carries, not a
   `.with_deployments(...)` line.

   **Where the merged client lives: a port trait in the client, implemented in the runner.**
   The gate is not what decides this, and reading it as the constraint understates the problem.
   `crates/zeroship-workflow/Cargo.toml` declares `zeroship-workflow-client` in
   `[dependencies]`, so an edge back the other way is a Cargo cycle rather than a rule violation;
   `workflow_process_dependencies_follow_crate_ownership` in
   `xtask/tests/workflow_architecture.rs` catches the dev-kind and transitive spellings Cargo
   would tolerate. Moving `TaskAssignment`, `WorkflowExecution`, `JobReceipt` and their
   neighbours into `zeroship-core` is therefore compulsory for a client that names them, not
   stylistic - and the closure is not three types. It reaches `WorkflowServiceError`, the DTO
   half of `crates/zeroship-workflow/src/engine.rs`, and the outcome decoder with its helpers.
   It would also publish `DeliveredTask` and `TaskRenewal`, whose fields are private precisely so
   that `renew` is the only way to advance a deadline and every host linking core cannot mint
   delivery authority.

   **Core's own headers refuse the cargo.** `crates/zeroship-core/src/workflow_jobs.rs` opens
   "Customer inputs, history and outputs stay in creator storage" and
   `crates/zeroship-core/src/workflow_coordination.rs` says what crosses is "at most the
   descriptor that locates one". `WorkflowTrigger.input`, `WorkflowInvocation.journal` and
   `StepOutcome::StepCompleted.output` are the inputs, history and output bytes those two
   sentences exclude. A move would contradict the module it moves into.

   **None of that is necessary, because the wire boundary is already core.**
   `JobReceipt::settlement` (`crates/zeroship-workflow/src/service/delivery.rs`) returns a core
   `Settlement`, and `acknowledge` in `crates/zeroship-workflow-runner/src/delivery.rs` performs
   that conversion before it calls `transport.settle`. So the client can own the merged exchange
   without naming a journal type: declare the port in `zeroship-workflow-client` with associated
   types for the journal payloads, bounded by serde rather than by identity, and implement it in
   `zeroship-workflow-runner` over `AppWorkflows`. The precedent is in the tree and points the
   other way across the same seam: `JobTransport` in
   `crates/zeroship-workflow-runner/src/delivery.rs` declares `type Lease: JobLease + Clone;` so
   the runner need not name the client's `LeasedJob`. This is that technique inverted.

   Bounding the body stays with the client, which is where the bound below belongs anyway: it
   measures serialized bytes and needs no view inside them.

   **The gate this turns on can currently pass over nothing.** Its two siblings in that file each
   assert the walk was nonempty - "the walk visited only the leaf itself, so an empty result is
   not evidence" - and `workflow_process_dependencies_follow_crate_ownership` has no such
   control, so every arm would pass vacuously if the resolve graph came back without edges. Fix
   that before leaning on it.

   **One bound is already wrong for the merged settle.** `Options::default` in
   `crates/zeroship-workflow-client/src/lib.rs` takes `max_request_bytes` from
   `MAX_INPUT_BYTES_CEILING` while a settle body carrying the outcome batch answers to
   `max_journal_bytes`, which `append` in `crates/zeroship-workflow/src/service/journal.rs`
   enforces against the journal ceiling. Open 1 counts three numbers on the request path and
   does not notice that the first derives from the wrong one for this endpoint.

   **The concurrency the merge introduces.** Today every accept for an app runs in the one worker
   process holding that app's binding. Served, accepts run in `RunService`, which is one per HTTP
   worker thread because its store is `!Send`, across every replica. `lock_app`
   (`crates/zeroship-workflow/src/service/app.rs`) takes `lock_app_state`, and `assign` then takes
   `lock_run` on the run itself; both are filtered row updates and do serialize across
   connections, so the single-assignment invariant holds - but the compare-and-swap in
   `tasks::assign` stops being unreachable by construction and becomes what stands between a lock
   bug and two live tasks on one run.

   **Its refusal is already bound. Its ORDERING is not, and that is the half this step owns.**
   `sqlite_a_run_holding_a_dispatch_is_not_assigned_another` and its postgres twin in
   `crates/zeroship-workflow/src/service/tests/task_models.rs` drive `assign` against a run
   holding the live dispatch its own `poll` handed out, and take the same call again after
   `release` as the control. No production caller reaches that refusal - `reclaim`
   (`crates/zeroship-workflow/src/service/delivery.rs`) defers the whole delivery unless it can
   expire the held task and null the column first, and `poll` expires it in place - so the test
   reaches it directly through `claim_again`. Both arms run inside ONE transaction under the app
   lock, so neither says whether two claimants on two connections are ordered.

   **The two-connection test exists and does not force its race.**
   `postgres_independent_orm_hosts_serialize_admission_and_claims`
   (`crates/zeroship-workflow/src/service/tests/orm.rs`) opens two stores and runs
   `futures::join!(first.poll(..), second.poll(..))`, then asserts
   `assert_ne!(a.is_some(), b.is_some())`. On a single-threaded executor the first poll may
   finish before the second begins, and that assertion passes identically either way - so its
   name claims a serialization it does not establish. Fixing it is the step's real work, not
   writing a third test beside it.

   **What such a test must carry, or it measures nothing.** Force the contention with a held
   blocker rather than with `join!`: a third service takes the app-state row and two claimants
   pile up behind it, as `postgres_concurrent_app_revocations_each_advance_the_signal_epoch`
   (`crates/zeroship-workflow/src/service/tests/ingress_models.rs`) already does. Then observe
   the wait positively through `pg_stat_activity`, with the term naming
   `__zeroship_workflow_app_state` that
   `postgres_collection_rechecks_references_after_waiting_for_completion`
   (`crates/zeroship-workflow/src/service/tests/payloads.rs`) uses, because that term is what
   separates a claimant blocked at the app lock from one blocked anywhere else. Without the
   probe the test stays green under the one mutation that matters: turning `lock_app_state`'s
   `$inc` update into a read leaves `assign` correct and simply stops ordering the claimants.

   **Assert the production answer, which is not an error.** `poll_inner`
   (`crates/zeroship-workflow/src/service/tasks.rs`) re-reads the run after `lock_run` and
   commits out when `due_at` is still ahead of now, and the winner's `assign` set `due_at` to
   its lease expiry - so the loser gets `Ok(None)`, and the candidate filter excludes the run on
   the next sweep. The arm worth writing is the one that would go red if that gate were deleted:
   the loser would reach the compare-and-swap and `poll` would hand a worker `Err(Conflict)`
   where it owed it "no work".

   Two facts bound how it is built. Two `begin()` calls on ONE `OrmStore` do not error, they
   hang - admission is per lane set on the `OrmContext` the store owns - so a second claimant
   needs a second store, which `crates/zeroship-workflow/src/service/tests/orm.rs` already
   spells. And it is postgres-only: SQLite reserves its writer as the transaction opens, so its
   loser blocks inside `begin` and a sqlite arm would silently measure a different thing.

   **And most of that filter cannot miss.** Under `lock_run` the `generation` and `lease_epoch`
   terms are read back from the row the lock pins, inside the same call, so only a non-null
   `task_id` can drop the match to zero. A mutation deleting either of the other two prints
   green, and a test that races epochs against this filter exercises nothing.

   Collection does not widen this step. Open 6 records why: the journal and the object store
   are already decoupled by a commit on that path, and the manager already owns when it runs.
   It is separate, smaller work carrying its own contract.

5. **Cut the worker over to the remote variants, and own what step 4 does not.** This is the
   flag day, and step 4's server half is inside it rather than before it.

   **An HTTP `WorkflowBackend`, which is not the easy half.** Only `AppBackend`
   (`crates/zeroship-workflow/src/service/backend.rs`) and `ReadyBackend`
   (`crates/zeroship-workflow-runner/src/ready.rs`) implement that trait, and the factory in
   `crates/zeroship-workflow-v8/src/lib.rs` takes a third arm cleanly. Counting methods says
   mechanical; counting decisions says otherwise, and the seven do not divide evenly.

   **Three are wiring, and the wire already exists on both ends.**
   `WorkerCoordinator::run_status`, `signal_run` and `transition_run` in
   `crates/zeroship-workflow-client/src/lib.rs` are written, their endpoints are served, and
   NOTHING calls them - no production caller and no test, in either crate. That is the built
   half of this step already sitting in the tree unwired, and wiring it is the small part.

   **One is a service capability.** `restart` has its endpoint and handler and refuses
   unconditionally, because `RunService` builds its service without deployments; see the
   credential question under step 4.

   **One waits on the payload seam, which already has its answer.** `start` needs no deployment
   input - `start_captured` in `crates/zeroship-workflow/src/service/app.rs` resolves it with
   `active_deploy` against the journal's own rows - but it must mint a `RequestId` on the wire
   rather than internally, and it stages the creator's input into a payload store the server may
   not reach. That last part is the same split `stage` has: `stage_input`
   (`crates/zeroship-workflow-runner/src/payloads/objects.rs`) computes the descriptor from the
   bytes itself and records ownership through the journal handle it is given, so the worker
   writes the bytes and only the ownership record crosses. `start` therefore needs no new
   mechanism, only the one the reserve already defines. It does need `StartOptions`,
   `StartedRun`, `ConflictPolicy` and `WorkflowOutputRef` moved into `zeroship-core` before the
   client can spell the call, and a request type that is a creator-safe subset of
   `StartOptions`, since `input_ref` is a descriptor a creator must never supply.

   **And two may be the wrong shape to build at all.** `read_step_output` and `read_output`
   return `Vec<u8>`. A single payload answers to `MAX_PAYLOAD_BYTES_CEILING` while the client's
   reply answers to `MAX_JOURNAL_BYTES_CEILING`, which is smaller, and the transport is
   buffered JSON with no byte-stream path. `crates/zeroship-core/src/workflow_coordination.rs`
   already states the rule - what crosses is "at most the descriptor that locates one" - and
   `AppWorkflows::read_step_output` already splits `Inline` from `Object`. So the journal
   lookup is what should cross, with the host resolving bytes from its own store. That changes
   the TRAIT rather than the count of implementations, and it is the thing to settle before
   anyone writes a third one.

   **The journal calls that are not the three.** `DeliverySlot::run` in
   `crates/zeroship-workflow-runner/src/delivery.rs` reaches `activate_job`, `reconcile_job`,
   `cron_job`, `management_job`, `release_hold_job`, `close_job`, `fanout_job`,
   `propagation_job`, `release_job` and `job_receipt` beside the three step 4 merges. Open 6
   concludes the sweeps move WITH the fold rather than crossing, so most of those become deleted
   branches rather than new remote calls - but a reader of step 4 alone would write them as
   remote calls, so the conclusion belongs here where the cutover happens.

   **The `TaskPayloads` seam, which is smaller than it looks.** `stage` does NOT run
   mid-execution. `V8Execution::wait` (`crates/zeroship-workflow-v8/src/executor.rs`) calls
   `self.stop().await` first - "app code has finished its frontier" - and
   `crates/zeroship-workflow-v8/tests/runner.rs` asserts every isolate probe is disposed before
   a stage arrives. Nor is it without a neighbour: between `stage` returning and `complete_job`
   in `crates/zeroship-workflow-runner/src/delivery.rs` there is no I/O at all, and
   `complete_job` already reads and writes the same payload rows through `promote` and `attach`.

   **So it is half-mergeable, and the bytes never cross.** `stage_inner`
   (`crates/zeroship-workflow/src/service/payloads.rs`) opens two transactions: the first
   reserves an id under the quota and the payload ceiling, the second holds the app and run
   locks ACROSS the object write and then confirms. Only the reserve must precede the write,
   because it mints the id the object is keyed by; the confirm folds into `complete_job`. The
   reserve batches per dispatch, since `PreparedExecution::from_runtime_json` decodes and
   deduplicates every descriptor before any upload starts. What crosses is `WorkflowOutputRef`
   and ids, never payload bytes -
   `the_workflow_admission_crate_declares_no_payload_storage_dependency` holds that split.

   **One obligation is genuinely new.** The confirm update filters on the primary key alone and
   discards its row count, which is safe only because the lock is held across the write. Split
   across a request boundary it has to carry `state` and the reserved `expires_at` as
   predicates and check that exactly one row changed, the way `collect_payload_checked` in
   `crates/zeroship-workflow/src/service/payloads/collection.rs` already does.

   **And `read` crosses once per dispatch, not once per call.** It is the only one of the three
   that fires while an isolate is live, but the journal contributes exactly one thing the worker
   cannot supply itself: the payload id. `WorkflowOutputRef` carries hash, size and content type
   and no key; the key is minted by `typed_id::generate` in `stage_payload`, and the object store
   is addressed by it. Name to descriptor is already local - the assignment carries the whole
   journal, and `TaskPayloadReader` resolves names against it with no I/O. And the mapping is
   immutable for the dispatch: once a step output is `referenced`, the matching arm of
   `owned_reference` carries no expiry predicate and collection skips that state entirely. So
   the descriptor-to-id map for a dispatch is a constant, and one prefetch replaces N reads.

   **Two things constrain how that map travels.** It must NOT ride inside `WorkflowInvocation`:
   `V8Execution::wait` serializes that into the envelope creator code receives, and the
   executor's own contract says only `assignment.invocation` may enter app code. It belongs on
   `TaskAssignment` beside the token. And prefetch the MAPPING, not the bytes - the JS bridge
   already memoises byte reads per dispatch by `(run, name, occurrence, hash)` in
   `crates/zeroship-workflow-v8/js/dispatch.js`, deliberately lazily, so eagerly fetching bytes
   would pay for outputs the body never touches.

   **Moving this read remote shrinks a lock rather than widening one.** Today
   `authorized_task` takes `lock_app_state` - app-wide, not run-scoped - and `lock_run`, and the
   object open happens inside that transaction, so an S3 header round trip runs with both held
   and every replay read serializes against every journal mutation for the app. Served, the
   service answers the mapping and commits; the worker opens the object afterwards.

   **One guard is currently unreachable and becomes live on the move.** `read_verified` refuses
   when the returned descriptor differs from the one asked for, but today the row is selected by
   equality on hash, size and content type, so it cannot differ. Nothing asserts that refusal -
   the message appears once in the tree, at its own definition. It is the one check positioned
   for the relocation, and it needs a test before the transport changes under it.

   **What would make a prefetch silently wrong.** It drops the per-read lease recheck, and
   nothing in the tree drives a read after the lease is lost while an isolate is live - the
   outage test fakes an unavailable transport, which is a different mechanism. So the suite
   would confirm a prefetch and say nothing about the property the prefetch removes. That test
   comes first.

   **And `RunService` installs no deployments source**, so `tasks::assign` finds no available
   deploy and a served claim cannot return a task. Installing one is necessary and not
   sufficient: `record_verified` in `crates/zeroship-workflow/src/service/deploys.rs` is the only
   writer of that table, so the rows are absent too until something produces them. That is why
   `start` having no endpoint is a sequencing fact and not a gap in step 3.

6. **Delete the creator-schema path**, and say where each piece lives, because they are not all
   in the workflow crates: `ensure_journal` and its route in
   `crates/zeroship-workflow-server/src/api.rs`, the journal bundle and `SCHEMA_PLACEHOLDER` in
   `crates/zeroship-workflow-server/src/journal.rs` and
   `crates/zeroship-workflow-schema/src/lib.rs`, `JournalManager` in
   `crates/zeroship-control/src/publication/journal.rs`, and the worker's repair path in
   `crates/zeroship-worker/src/workflow_creator.rs`. Only once step 5 is green.

   This step also ends an exposure rather than only removing code. `ensure_journal` is the one
   worker-authenticated endpoint that reaches no placement, app or zone, so the registry key
   lookup is its whole gate; a lapsed instance was admitted there indefinitely until that lookup
   learned the lease. The lease predicate is the fix that matters now, and deleting the endpoint
   is what removes the shape.

7. **Drop `zeroship-data-orm` from `zeroship-workflow`**, the engine - NOT from
   `crates/zeroship-worker`, which declares it directly and keeps needing it. The worker's own
   uses are creator data rather than journal: resolved bindings, connection factories and
   project keys in `crates/zeroship-worker/src/sync.rs`,
   `crates/zeroship-worker/src/cache/fixture.rs` and
   `crates/zeroship-worker/src/workflow_host.rs`. Removing that declaration would break
   `env.db`, and removing it from the engine is what this step means: once the `service/` tree
   has moved to the service, the engine holds no store and needs no ORM. It is the last step
   because nothing earlier makes the engine storeless, not because it is hard.

**Latency is not the gate; payload is, and its bound is settled.** Open 1 holds the answer and
names `crates/zeroship-workflow-client/tests/round_trip_cost.rs` as the instrument: re-run it
rather than trusting a paragraph. The bounds a crossing payload meets answer to the ceiling
constants in `crates/zeroship-core/src/workflow_policy.rs`, so step 4 plans against a bound
instead of choosing one. Read Open 4 alongside it, because every reading behind Open 1 was taken
over loopback.

**The fence is a separate track, not a gate.** Under this design only the service reaches the
journal, so the `app_id` filter sits inside a trusted process and is defensible without
row-level security. Giving every table a fence of its own remains worth doing as defence in
depth - `verify` in `crates/zeroship-workflow-server/src/coordinator.rs` is the pattern to
extend, since it already proves a login's posture against the catalog rather than trusting
configuration - but it does not block the steps above, and treating it as a prerequisite would
stall the defect fixes that motivate the move.

---

## Open

Every item below is answered or decided; what is still genuinely open lives INSIDE them, and
this index is where to find it. Read an item in full before acting on it - the reasoning is the
part that dates, not the verdict.

| | verdict | what is still open in it |
|---|---|---|
| 1 | ANSWERED - payload is the gate, not latency | nothing; re-run the instrument rather than trusting the prose |
| 2 | DECIDED - a ceiling governs both sides | nothing |
| 3 | DECIDED - its own database | the registry read STAYS by decision; severing is later work |
| 4 | DECIDED - zone-local | nothing; the work it waits on is Open 3's |
| 5 | pre-launch, no in-flight runs | the answer expires at launch |
| 6 | ANSWERED as a description | nothing; the credential question it raised moved to Open 3 |
| 7 | ANSWERED - creator payload is not in the journal | nothing |
| 8 | ANSWERED - a run input is an ordinary payload object | nothing |

1. **ANSWERED - latency is not the gate; payload is.** Measured before anything was built, by
   `crates/zeroship-workflow-client/tests/round_trip_cost.rs`, which exercises the shipped
   transport with a real assertion minted per call and a peer that really verifies it. Re-run it
   rather than trusting this paragraph.

   Three findings changed the design. First, a round trip is far cheaper than the work a
   dispatch already does, and most of a small one is the credential, which pooling does not
   remove. Second, **the realistic outcome count is one.** `ZsFrontierCoordinator` in
   `crates/zeroship-workflow-v8/js/dispatch.js` seals shortly after creation and always rejects
   with a suspend signal, so only steps issued in one synchronous turn share a batch; sequential
   `await`s become separate dispatches. The corpus agrees - the widest construction anywhere is
   far below `max_frontier`, which is exercised nowhere. So "transitions already batch" is
   mechanically true and operationally misleading: a run still costs a crossing per dispatch,
   spread out rather than concentrated. Third, and decisively, **all three crossings already sit
   beside a call that is remote today.** Merge them and the relocation adds no round trip; bolt
   them on as separate endpoints and it doubles the manager's request rate. That is a capacity
   decision, not a latency one, and it is why Plan step 4 is written as a merge.

   **Where the sign flips.** `TaskAssignment` in
   `crates/zeroship-workflow/src/service/types.rs` carries the whole replay journal on every
   dispatch, so a run's bytes grow with the square of its steps. That cost is real today as
   serialization rather than as transfer: `V8Execution` in
   `crates/zeroship-workflow-v8/src/executor.rs` encodes the invocation once per dispatch, and
   no journal crosses the client transport at all - `zeroship-workflow-client` does not depend
   on `zeroship-workflow`, and the `Assignment` it exchanges in
   `crates/zeroship-core/src/workflow_coordination.rs` carries no journal. Two confusably named
   types in two crates: read any claim about "the assignment" against which one it means.

   **The bounds describing this crossing answer to one authority.** Neither the policy bound nor
   the transport bound is authoritative over the other. The ceiling constants in
   `crates/zeroship-core/src/workflow_policy.rs` name each shared quantity once, and both sides
   stand in the same relation to it: `AppPolicy::validate` refuses `max_journal_bytes` above the
   journal ceiling and the client's `max_response_bytes` default derives from it;
   `max_input_bytes` and `max_request_bytes` stand the same way to the input ceiling. So
   `client_options` in `crates/zeroship-worker/src/workflow_host.rs`, returning
   `ClientOptions::default()`, needs no configuration surface of its own to keep the two in
   step: it will not refuse, on its own account, something admission admitted. What a peer
   accepts stays that peer's own bound, which is the next paragraph.

   **The request path is three numbers, not two.** The client's `Options::max_request_bytes`
   default, the server's `DEFAULT_MAX_REQUEST_BYTES` in
   `crates/zeroship-workflow-server/src/api.rs`, and the `workflow.max_request_bytes` setting
   that defaults to that constant. Only the first describes the input quantity. The other two
   are the global body cap `configure_with_limit` installs on the `ServiceConfig` state, which
   covers every endpoint on that server and buffers rather than streams, so raising it to admit
   a journal would widen the per-request buffer on `healthz`, `verify_assignment`, `manage` and
   `renew` alike. It stays its own bound. An endpoint that comes to carry a journal takes a
   named per-resource bound derived from the ceiling instead, the shape
   `crates/zeroship-migrate-server/src/api.rs` and
   `crates/zeroship-control/src/deployment_hold_api.rs` already use. No endpoint carries one
   today, so none has one.

   The paired quantities are not like for like, which is why the ceiling bounds each of them
   rather than equating two measurements. The policy bounds count stored `StepCheckpoint` JSON,
   one per item and one accumulated per run generation; the client bounds count a single HTTP
   message. And `replay` in `crates/zeroship-workflow/src/service/journal.rs` narrows
   `StepCheckpoint` to `JournalStep` and drops retrying rows, so the journal bound is an upper
   bound on wire bytes rather than a measure of them.

   Plan step 4 therefore inherits a bound it can plan against rather than a decision it has to
   make. See Open 2, which is the same question seen from the payload side and closed with this
   one.

2. **DECIDED - referencing does not cover the creator-facing reads, and a ceiling both sides
   derive from governs them.** The measurement settled what the paths are; the decision settles
   which bound governs, and it is the same decision as Open 1's.

   **Referencing covers the journal, not the read.** `WorkflowOutputRef` in
   `crates/zeroship-workflow/src/engine.rs` keeps the stored row a reference rather than an
   inline value, and that is the whole of what it covers. The creator-facing reads materialize
   bytes: `read_step_output` and `read_output` in `crates/zeroship-workflow/src/backend.rs` both
   answer `Vec<u8>`, so a named step's output and a run's final output arrive whole in the
   caller's process however they are stored.

   **The host budget bounding those reads derives from the ceiling the policy is refused
   above.** `ObjectStepOutputs` in `crates/zeroship-workflow-runner/src/payloads/objects.rs`
   resolves both "inside the host memory budget", holds a read limit it refuses to have empty,
   and applies it to each read through `into_bytes`, which rejects an oversized reference rather
   than streaming it. The worker takes that limit from `TaskPayloadLimits::max_payload_bytes` in
   `crates/zeroship-workflow-runner/src/outputs.rs`, whose default derives from the payload
   ceiling, and `crates/zeroship-cli/src/workflow/host.rs` composes the dev tier's reader the
   same way. `AppPolicy::max_payload_bytes` in `crates/zeroship-core/src/workflow_policy.rs` is
   refused above that same ceiling, so what `stage_payload` in
   `crates/zeroship-workflow/src/service/payloads.rs` admits on the way in is within the budget
   the read has on the way out.

   **Two refusals, because one invariant has two halves observable at different times.** Startup
   knows the configured host budgets and has observed no policy; admission knows the policy and
   cannot re-read what the host was configured with. So `AppPolicy::validate` refuses a policy
   above the ceiling wherever authority is admitted, and a host refuses a configured budget
   below it before it serves: `TaskPayloadLimits::validate_configured`, called from
   `LocalConfig::validate` in `crates/zeroship-cli/src/workflow.rs`, which is where `[payloads]`
   is configured. Either refusal alone catches one direction and leaves the other open.

   **The conflict this closes is prospective rather than live.** Nothing of this crosses that
   transport today. `zeroship-workflow-client` does not depend on `zeroship-workflow`, and the
   `Assignment` it exchanges in `crates/zeroship-core/src/workflow_coordination.rs` carries an
   app, a worker, a revision and an expiry. That is also why a ceiling is the authority rather
   than either measurement: the pairs Open 1 names are not like for like, and neither is this
   one. The policy bounds count stored `StepCheckpoint` JSON while the client bounds count a
   single HTTP message, and `replay` in `crates/zeroship-workflow/src/service/journal.rs`
   narrows `StepCheckpoint` to `JournalStep` and drops retrying rows, so a policy bound is an
   upper bound on wire bytes rather than a measure of them.

   **What this does not cover.** A ceiling serves a pair that measures the same bytes.
   `TaskPayloadLimits::max_inline_bytes` and `AppPolicy::max_input_bytes` do not: an inline
   value rides inside the checkpoint the policy bound measures, so the host bound is a part of
   the policy bound rather than the same quantity, separated by the checkpoint's own framing
   and, in `apply` in `crates/zeroship-workflow/src/service/frontier.rs`, by however many
   outcomes one batch carries. They are equal today, so a value at the host's inline threshold
   is refused by `journal.rs` rather than referenced by the host. Deciding what separation they
   need is its own item, not this one.

   It was one decision, not two, and it closes this item and Open 1 together.

3. **DECIDED - `workflow_manager` is its own database, and every design here assumes it.** A
   production deployment gives it one, so the journal's creator-volume rows never share a store
   with the control plane. Build against that assumption rather than the arrangement below; what
   follows records why the question was open and what the promotion costs. It is a
   schema in the control database today with its own migrator, login and search path. Moving
   the journal into it puts creator-volume rows - runs, pages, receipts, payloads - in the
   control plane, which is the coupling `docs/proposals/2026-09-05-gateway-central-database-decoupling.md`
   is fighting on a different axis. Step 1 installed the journal, so those rows are there now
   rather than in prospect.

   The promotion splits in two, and only one half is contained. The CONNECTION is: the
   `[workflow]` section in `crates/zeroship-core/src/config/file.rs` owns a `database_url` that
   `crates/zeroship-workflow-server/src/server.rs` requires and nothing in this repo pins, so
   which store the service opens is a deployment choice. The privilege separation is real
   alongside it - `Coordinator::verify` in
   `crates/zeroship-workflow-server/src/coordinator.rs` refuses to start when its own login
   holds `CREATE` on `workflow_manager`.

   The SCHEMA INSTALLATION is not, and it is not the whole of what stands in the way.
   `workflow_manager` and its tables arrive from
   `db/migrations-ts/20260911000000_workflow_coordination.ts`, the journal joined them in
   `20260919000000_workflow_journal.ts`, and that corpus is applied as one job against one
   database. Pointing `workflow.database_url` at another store finds no schema in it.

   **Splitting the corpus is a prerequisite rather than the substance, because the service
   reads the control plane over the same DSN.** `workflow.database_url` names one store and
   PostgreSQL does not query across databases, so every control read the service makes has to
   be severed before that setting can point anywhere else. `Coordinator::verify` in
   `crates/zeroship-workflow-server/src/coordinator.rs` refuses first, and refuses at startup
   rather than at first use: its probe is a single statement whose `workflow_manager` tables
   are followed by

       SELECT id,execution_zone_id,deleted_at FROM zeroship.apps LIMIT 0;

   which is the whole of what the workflow role is granted on `zeroship.apps` - placement's
   columns and the key it filters on. It is NOT the whole of its reach into `zeroship`:
   `worker_instances` carries `(id,status,public_key)` from one grant file and
   `(execution_zone_id,expires_at)` from another. The deployment catalog is out of reach: the
   grants that fed the deleted pointer read are dropped, and
   `deployment_catalog_is_out_of_reach` in
   `crates/zeroship-workflow-server/tests/platform_schema.rs` asserts `42501` on
   `zeroship.app_deploys` and on `apps.deploy_hash`, with the placement columns as its control.

   The crossing recurs on the request path and inside the manager. `active_key` in
   `crates/zeroship-workflow-server/src/auth.rs` answers every worker-authenticated call from
   `SELECT public_key FROM zeroship.worker_instances WHERE id=$1 AND status='active' AND
   expires_at > now()`, and `crates/zeroship-workflow-manager/src/policy/control/models.rs`
   declares `plans` under the administrative credential `PlanPolicyStore` takes, beside the
   publication tables this service owns. The highest-frequency crossing is in
   another schema entirely and a search for `zeroship.` cannot find it:
   `SharedClientReplayStore` (`crates/zeroship-authn/src/service_replay.rs`), constructed in
   `crates/zeroship-workflow-server/src/server.rs`, upserts
   `service_authn.service_assertion_replay` once per authenticated inbound call. Enumerate
   these from the tree rather than from this paragraph before sizing the work, and enumerate
   by the grants rather than by the SQL: the grant files have to name every schema they reach.

   **These crossings are three problems, not one, and only one of them is transport.**

   **DONE for the tables the service owns.** `workflow_policy_ledger` and
   `workflow_rollout_config` now live in `workflow_manager`, created by
   `db/migrations-ts/20260911000060_workflow_policy_tables.ts`, so the write a cache could never
   have answered is local rather than transported. `ControlPolicyStore` in
   `crates/zeroship-workflow-manager/src/policy/control/store.rs` holds one publication handle
   and keeps the bracket: the ledger row's lock is taken on it before the inputs are read over
   the facts capability, with that transaction still open.
   `service_authn.service_assertion_replay` is the same shape -
   the audience is checked before the store is consulted, so a per-service table keeps single
   use - and the split proposal reaches that same verdict, as recorded below.

   **The move made one thing depend on topology that nothing enforces.** The ledger's revision
   has to be monotonic PER APP, and it now lives in a store the deployment chooses, so the
   property holds only while an app is served by exactly one workflow store. Zone-local
   topology plus the zone match in `crates/zeroship-workflow-manager/src/coordinator/placement.rs`
   - `current.active && current.zone == facts.zone` - makes that true today, but neither states
   it as an invariant about STORES, and nothing refuses a second store for one app. Two stores
   serving one app would not error; the revisions would simply stop ordering the inputs they
   were computed from.

   **`docs/proposals/2026-09-20-platform-service-database-split.md` already assigns an owner to
   every table in this set, and this document should not re-derive one.** Its table gives
   `workflow_policy_ledger` and `workflow_rollout_config` to workflow, which agrees with the
   paragraph above, and its Open 3 settles the replay store by the same audience-ordering
   argument: a per-service table suffices because validation rejects an assertion addressed
   elsewhere before the store is ever consulted. Its section heading "Shared by design, owned by
   nobody" is the part that has not caught up - the prose directly beneath that heading already
   argues the opposite. Read the Open, not the heading. It also marks `app_deploys` CONTESTED,
   "workflow's DDL, control writes", which is the same table Plan step 3 records `restart` as
   unable to resolve.

   Some is Control's authority read on a path that is ALREADY eventual.
   `crates/zeroship-workflow-manager/src/eligibility.rs` says so itself - "THIS IS A LIVENESS
   HINT, NOT AN AUTHORIZATION FENCE" - and policy already runs through a cache with a
   source-supplied validity. These need a reachable source of truth and a bounded staleness the
   tree already has machinery for, not a synchronous read. Note that several of them are read
   INSIDE open queue transactions, so turning them into network calls would hold locks across a
   round trip, which Open 4's zone-local decision makes worse rather than better.

   **Whatever answers those reads owes a monotonic-read guarantee, and one database currently
   gives it away.** `publish` in `crates/zeroship-workflow-manager/src/policy/control/store.rs`
   upserts the ledger row before reading its inputs, and the comment at that line says inputs
   are read only after the wait completes. The ordering is not decorative: `observe` pins
   read-committed, so every statement takes a fresh snapshot and the inputs were never in the
   ledger's snapshot anyway. What the row lock buys is that a publisher waiting on it reads its
   inputs only after the previous publisher committed, so a higher revision was computed from
   inputs at least as new. `PolicyRefresh::install`
   (`crates/zeroship-workflow/src/service/policy.rs`) refuses a lower revision but accepts
   whatever policy a HIGHER one carries, so losing that order lets a stale policy take authority
   and keep it.

   The argument rests on one fact about the input source: a read issued after a commit cannot
   return state older than that commit saw. A single PostgreSQL instance gives that for free,
   whichever schema each side sits in. An API call, a replicated projection and a lagging
   replica do not. **So any shape that puts Control's inputs behind one of those owes this
   bracket a monotonic-read guarantee or a replacement for it** - which is a constraint on the
   answer rather than a detail of it, and it applies to every option, not to one.

   **Two of those three now cross `svc/workflow` instead of a binding.** `CONTROL_APP_FACTS`
   (`POST /v1/app-facts`, declared in `crates/zeroship-core/src/service_identity.rs`) answers
   the policy inputs and the closing lane's deletion check for a page of apps, gated on the
   `svc/workflow` issuer the way `DeploymentHoldApi` is.
   `crates/zeroship-workflow-server/src/server.rs` binds no `zeroship` schema: neither
   `connect_lifecycle` nor the policy inputs binding exists there, and the grant is narrower to
   match:
   `db/migrations-ts/20260911000050_workflow_platform_grants.ts` drops `zeroship.plans` entirely
   and narrows `apps` to `(id)`. The operator writers moved to a separate
   `PlanPolicyStore` with no production constructor, so the serving path carries no
   administrative credential for an operation it never performs.

   **Placement eligibility deliberately did NOT move, and the reason is a trap worth naming.**
   Its two reads sit inside one transaction precisely so they can DISAGREE:
   `crates/zeroship-workflow-manager/src/coordinator/placement.rs` reads again before commit
   because "a revocation committed between the two reads refuses this placement". Serving both
   from a cache makes the second read return the first one's value - the fence keeps compiling,
   keeps passing its tests, and stops fencing. It moves only when that fence is replaced by
   something cacheable, not when a cache is put behind it.

   **Only one of the two columns it reads is fence-relevant, which is where a replacement would
   aim.** `apps.execution_zone_id` cannot change:
   `db/migrations-ts/20260914000600_placement_eligibility.ts` installs a BEFORE UPDATE
   trigger, `apps_frozen_execution_zone`, raising `check_violation`
   with "an app's execution zone is fixed when the app is created". A value the database refuses
   to move is one a cache can never hold staler than the truth. So the double read exists for
   `deleted_at` alone, and a design that carries the frozen zone while leaving deletion a live
   read would keep the fence the second read provides. That is a redesign rather than this
   patch, and it does not remove the binding - deletion still needs a reader - but it names
   which column the work is actually about.

   **The monotonic-read bracket is answered rather than assumed.** Control's response carries a
   `SourceWatermark` taken in the SAME statement as the facts, so it is at least the position of
   every change visible in that snapshot, and `publish` refuses an observation below the one the
   ledger already holds. The refusal is the safe direction: the hazard is a stale PERMISSIVE
   policy taking authority, so failing closed denies admission rather than granting it.
   `a_regressed_watermark_refuses_and_publishes_nothing` binds it, and it is new coverage - no
   test covered a stale input read before, because with one database it could not happen.

   A fence built on `zeroship.apps.lifecycle_revision` looks like the obvious answer and does
   not work: it is written only by `allocate_revision` in
   `crates/zeroship-control/src/publication/catalog.rs`, from deploy accept, archive and
   restore, and by none of the writers of the policy inputs. It would be constant across exactly
   the changes it was meant to order.


   And one is authentication, which is different in kind from the rest and worth saying why.
   Services authenticate each other with one mechanism - a short-lived signed assertion naming
   issuer, audience and a single-use `jti` - but it draws its verifying key from two places, and
   `ensure_journal` in `crates/zeroship-workflow-server/src/api.rs` states the split at the door:
   "Control presents a ROLE assertion verified against the peer bundle; a worker presents an
   INSTANCE assertion verified against the enrolment registry." A role is long-lived and there
   are a handful, so a static `service-peers.json` carries its key. A worker instance mints its
   own keypair at startup and there are as many as you run, so a static file cannot: the
   registry is the dynamic half of the same scheme, and `active_key` is the read into it. That
   one query also answers liveness, since `status` and `expires_at` decide whether the instance
   is still enrolled, which is why it does not unpick into a pure identity lookup.

   Severing it is therefore a trust-model decision rather than a plumbing one, which is why it
   is deferred rather than improvised. The shape that reuses what exists rather than adding a
   mechanism is a control-signed attestation: Control already knows the key because it enrolled
   it, so it can sign a short-lived statement binding instance, key thumbprint and zone, and the
   service verifies that against Control's ROLE key - which
   `crates/zeroship-workflow-server/src/server.rs` already refuses to start without.
   `crates/zeroship-workflow/src/service/capability.rs` already mints and verifies
   control-signed, audience-bound, short-lived capabilities with no production caller. What it
   costs is that revocation stops being immediate and becomes bounded by the attestation's
   lifetime, because Control cannot un-say what it has signed. That is the shape it should take.

   **And it is the load-bearing one, not a peer of the others.** `zeroship.worker_instances` is
   what keeps the Control database binding alive: `PostgresWorkerRegistry` in
   `crates/zeroship-workflow-server/src/auth.rs` reads it twice - the readiness probe and
   `active_key` - and `ControlEligibility` in
   `crates/zeroship-workflow-manager/src/eligibility.rs` projects its worker half for placement.
   Moving the `apps` reads thins the seam without cutting it, because that table outlives them.
   The order is therefore the rest first, and authentication when its design lands.

   **DECIDED: defer the severing; the registry read stays.** A comprehensive credential design
   comes later, so `active_key` keeps reading `zeroship.worker_instances` and the service keeps
   its binding on Control's database for that one read. Nothing about the check relaxes - the
   lease predicate added with the fence still refuses a lapsed instance, and the endpoint that
   most needs the check, `ensure_journal`, reaches no placement, app or zone, so the credential
   is its whole gate. What this defers is the SEVERING, not the verification. Open 3's own goal
   is therefore reached in part: the corpus, the tables and the policy inputs have moved, and
   the registry read is what is left holding the binding open.

   **DECIDED: the journal hold follows the journal.** A journal deployment hold is the
   authority to keep a deployment alive for the runs a journal holds. Today that hold is
   `HoldScope::for_app`, `crates/zeroship-core/src/service_identity.rs` grants its endpoints to
   `svc/worker` while `svc/workflow` holds only the queue-scoped pair, and
   `crates/zeroship-workflow/src/deployment_holds/remote.rs` refuses any signer that is not an
   enrolled worker instance. So `restart` does not merely lack a row: the service lacks the
   credential to create one, and Plan step 4's deployments source is inert until that moves.
   At the cutover the authority moves with the thing it describes - `svc/workflow` gains the
   journal hold endpoints, the worker keeps only what it still owns, and the signer check
   widens to admit the service's role assertion. That is a grant change and a signer change in
   one patch, and it lands with step 5 rather than before it, because the hold is only
   meaningful once the journal it protects is the service's.


   **A gap this uncovered, unrelated to the move.** `workflow_rollout_config` has no production
   writer, and that is the stated posture rather than an omission:
   `db/migrations-ts/20260911000060_workflow_policy_tables.ts` says the service "never writes
   the rollout config at all", and an operator publishes the row under an administrative
   credential. Both setters still live in
   `crates/zeroship-workflow-manager/src/policy/control/store.rs`, but on two types now -
   `set_rollout` on `ControlPolicyStore` and `set_plan_policy` on `PlanPolicyStore` - and every
   caller of either is a test. `read_rollout` reads that table alone, the other inputs having
   left that database, so a deployment with no row published by hand fails every policy
   observation.

   **The corpus is partitionable; role identity is what still binds it.** No migration writes
   into two databases' worth of schemas any more:
   `db/migrations-ts/20260911000050_workflow_platform_grants.ts` carries most of the workflow
   login's reach outside its own schema - USAGE on `zeroship` and `service_authn`, column-scoped
   `SELECT` on the Control tables its coordinator and policy reads project, and DML on the
   shared assertion replay store - and `20260911000000_workflow_coordination.ts` keeps the
   schema, its roles and its own grants. **Two files grant that reach, not one:**
   `db/migrations-ts/20260914000600_placement_eligibility.ts` also grants
   `SELECT (execution_zone_id,deleted_at)` on `zeroship.apps` and
   `SELECT (execution_zone_id,expires_at)` on `zeroship.worker_instances`. Editing only the
   first leaves the eligibility reads working and nothing fails, so a change meant to remove a
   crossing would leave it in place.

   What still crosses is the roles, which are cluster-scoped rather than
   database-scoped. `20260914000600_placement_eligibility.ts` is a control migration granting
   to `zeroship_workflow`, so the control corpus needs that role to exist; and the revokes in
   `20260911000000_workflow_coordination.ts` name `zeroship_control`, `zeroship_worker`,
   `zeroship_gateway` and `zeroship_app`, which a separate cluster would not have. `Op::Revoke`
   in `crates/zeroship-migrate-postgres/src/vendor.rs` emits a bare
   `REVOKE {privs} ON {target} FROM {grantees}` with no `IF EXISTS`, and PostgreSQL raises
   `42704` for a role that does not exist, so a separate cluster aborts the apply at that
   statement rather than stepping over it. Settle the roles before the routing.

   **The third piece is the deployment site, and it has landed with one blocker named.** The
   `workflow` service in `deploy/compose/docker-compose.yml` supplies the four settings
   `crates/zeroship-workflow-server/src/server.rs` refuses to start without, under the runtime
   login rather than the migrator, and that is where a second store would first be named. Two
   credentials had to be created for it to run at all: `zeroship_workflow` carried no password,
   and no path wrote `svc-workflow.pem` or published `svc/workflow` to the peer document.

   **DECIDED, and built: the transport admits plaintext only to peers an operator named.**
   `Transport::configuration` in `crates/zeroship-workflow-client/src/transport.rs` admits
   `https` always, plain `http` to a loopback IP literal, and plain `http` to an exact origin
   listed in `plaintext_peers` - a shared setting reaching all three supply tiers, empty by
   default, so a deployment that says nothing keeps the old refusal. The list matches an origin
   exactly rather than a host, so a name repointed at a public address re-arms the fence by
   itself, and nothing resolves a name or classifies an address range.

   **What the list cannot do is written at the predicate, and it bounds every use of it.** It
   decides WHICH peer may be reached in clear; it cannot make plaintext safe. Assertions on
   these edges are verified under the full profile with a single-use `jti`, so straight replay
   is closed - but an assertion carries `iss, sub, aud, exp, iat, jti` and no digest of the
   request it accompanies, so an on-path attacker inside that network can lift a live one onto
   a MODIFIED body within its window. The list is also per PROCESS rather than per role, so
   naming an origin for one client authorizes every client that process builds to reach it in
   clear. Every origin on the list is therefore one whose entire network path is trusted, and
   **internal TLS remains the end state this defers rather than replaces.**

   The prose that stated the old rule moved with it, including
   `worker.workflow_manager_url`'s own doc in `crates/zeroship-worker/src/config.rs`. What the
   relaxation unblocks is both directions: the workflow service reaching Control and the
   migrate service, and - once compose sets `worker.workflow_manager_url`, which it still does
   not - the worker reaching the manager. Any new control endpoint the severing work adds
   inherits the same fence and the same list.

4. **DECIDED - the service is zone-local. One workflow store per zone, an app's runs in its own
   zone.** So Open 1's dispatch-crossing term stays the small one it was measured to be, and the
   co-location rule the decoupling states holds here too. Open 3 and this one wait on the same
   work, and Open 3 names it as three pieces rather than one: splitting the corpus, severing
   the service's control-plane reads, and giving the service a deployment site. A store per
   zone and a store of its own both wait on all three. Scope them once, there.

   The reasoning that made it the right answer. Every number
   behind Open 1 was taken over loopback. A crossing per dispatch is free at that distance and
   is not free across a wide area, so if the service is global the term Open 1 dismisses becomes
   the dominant one. It should be zone-local - one workflow store per zone, an app's runs living
   in its own zone - consistent with the decoupling's co-location rule, and worth deciding
   before the cutover rather than inheriting.

   An execution zone is already a configured thing: `join_token_zone` and the trusted
   join-signer file's permitted zones both live in `crates/zeroship-core/src/config/file.rs`.
   Since the `[workflow]` section owns its `database_url`, a store per zone is a deployment
   topology and reaches the same corpus question Open 3 names, so the two settle together.
   `the_two_zones_run_a_workflow_without_reaching_each_other` in
   `crates/zeroship-control/tests/workflow_private_zones_e2e.rs` holds zone isolation for the
   creator-schema placement this replaces; a relocated journal needs its own. The numbers Open 1
   rests on come from `crates/zeroship-workflow-client/tests/round_trip_cost.rs`, so a decision
   that the service may sit further away is one to re-measure there rather than re-argue here.

5. **What happens to in-flight runs at cutover?** Pre-launch, nothing: there are no runs. That
   answer expires, and the design should say so rather than let a later reader assume a
   migration exists.

6. **ANSWERED as a description - the worker loses a fold it holds only because the journal is
   under it, and one credential does not move with it.**

   **What it holds today is a store, not a run.** `build` in
   `crates/zeroship-worker/src/workflow_creator.rs` opens the journal itself through
   `HostStorage::open` (`crates/zeroship-workflow/src/service/store.rs`) and constructs a
   `WorkflowService` over it. That yields `WorkflowService::begin` in
   `crates/zeroship-workflow/src/service/app.rs` and `Transaction::database` in `store.rs`, so
   the ORM handle for every row in the schema is held by the process that executes creator code.
   `AppWorkflows::into_backend` (`crates/zeroship-workflow/src/service/backend.rs`) states the
   ownership plainly: "Which store the backend reaches is a choice its construction site makes,
   not one the handle carries." The construction site is the worker.

   **What it does with that reach is not its own run.** Before `DeliverySlot::run`
   (`crates/zeroship-workflow-runner/src/delivery.rs`) reaches `accept_job` it dispatches
   `activate_job`, `reconcile_job`, `cron_job`, `management_job`, `release_hold_job`,
   `collect_job`, `close_job`, `fanout_job` and `propagation_job`, and none of those starts an
   executor. They are the fold's own sweeps over the app's journal, run in the worker because
   that is where the journal is.

   **It is also the manager's only source.** `crates/zeroship-workflow/src/service/publication.rs`
   opens with "Creator-owned, immutable queue publication intents": a transition writes the
   intent before COMMIT, and "Network publication happens after it". `publish_pending`
   (`crates/zeroship-workflow-runner/src/publication.rs`) pages `AppWorkflows::pending_jobs` and
   submits each through a `JobPublisher`, and `prepare` in
   `crates/zeroship-workflow-runner/src/assignments.rs` marks an app the first time it binds,
   because "A previous process may have committed intents it never published." No lease asks for
   that sweep. It is recovery the worker owes because nothing else reads the journal.

   **Nothing in the database scopes any of it.** The journal declares no role, no row-level
   security and no grant (`crates/zeroship-workflow-schema/schema/schema.ts`). What binds the
   handle is the schema the host composes and a check in Rust. `JournalLocation`
   (`crates/zeroship-worker/src/workflow_host.rs`) answers `CreatorSchema` through
   `app_derivation::schema_name`, which returns the app id, so a schema holds a single app; its
   other arm is one journal "for every app, in a schema the service owns". `Transaction::check_app`
   (`crates/zeroship-workflow/src/service/store.rs`) refuses a foreign app, and engages only
   where a policy binding is set, while `OrmStore::begin` in the same file sets none and the
   worker holds the store. Under that other arm the Rust check is the whole fence, which is Why
   it is this way seen from the worker's side.

   **What it would hold afterwards is its task and the reply.** `accept_job` answers a
   `JobAcceptance`, `heartbeat_job` a `TaskRenewal` - "Everything one renewal changes about a
   live delivered task, with the run control intent read in the same transaction" - and
   `complete_job` a `JobReceipt` (`crates/zeroship-workflow/src/service/delivery.rs`). The worker
   derives each of those for itself: the replayed receipt, the frontier decision, the reclaimed
   lease, the control intent. Afterwards each is told to it, and a worker that disagrees has no
   second source.

   **What it costs.** Not the sweeps. They run no creator code, so they move with the fold, and
   the recovery duty above dissolves rather than transferring: the side that commits the intent
   is the side that publishes it. The cost is the object-store credential, and collection is the
   weakest case to decide it on.

   **Collection is not the forcing operation.** `AppWorkflows::collect_job`
   (`crates/zeroship-workflow/src/service/collection.rs`) takes a `PayloadDeleter`
   (`crates/zeroship-workflow/src/service/payloads.rs`), but `collect_payload_checked`
   (`crates/zeroship-workflow/src/service/payloads/collection.rs`) fences the row into
   `deleting` and commits, calls `deleter.delete` with no transaction open, then opens a fresh
   transaction to settle. The two stores are deliberately not in one operation, and the module
   header gives the reason: "an upload already dispatched by a dead writer may still arrive".
   Its scheduling half already crosses, too - `worker_operation`
   (`crates/zeroship-workflow-manager/src/coordinator/jobs.rs`) and `worker_publication`
   (`crates/zeroship-workflow-client/src/jobs.rs`) both refuse a worker-published
   `JobOperation::Collect`, so the manager already owns when collection runs.

   Other operations couple the stores harder. `cron_job`
   (`crates/zeroship-workflow/src/service/cron.rs`) takes an `InputStager`, a writer rather
   than a deleter, and `stage_inner` (`crates/zeroship-workflow/src/service/payloads.rs`) holds
   a transaction open across the object write - the one place in the tree where the journal and
   the object store really are in one operation. Decide the credential where `start` and the
   creator-facing reads move, against that evidence, rather than on collection.

   **What the credential is, as opposed to what the code asks for.** `PayloadObjects::open`
   asks for "a store whose credentials are private to the workflow host". `ProductionResources`
   (`crates/zeroship-worker/src/workflow_host.rs`) opens one `StorageStore` and hands it to
   both the payload store and the `StorageBinding` serving `env.storage`; the field it comes
   from reads "The creator object store `env.storage` uses; payloads live there". One identity
   covers deploy blobs, every app's `env.storage` and the workflow payload namespace, separated
   by key prefix alone. So a prefix-scoped credential held by the service would narrow the
   process that runs creator code, which is the opposite of how the trade reads at first.

   **Moving it is an invariant change rather than a configuration one.**
   `workflow_process_dependencies_follow_crate_ownership` (`xtask/tests/workflow_architecture.rs`)
   walks every non-dev edge and refuses `zeroship-workflow-server` any path to
   `zeroship-workflow-runner` or `zeroship-storage`, and
   `crates/zeroship-workflow-server/Cargo.toml` records the consequence beside its engine
   dependency. That is an invariant to raise deliberately at the step that needs it, not to
   work around here.

   **It is not purely subtractive.** The worker loses authority it holds as a consequence of
   where the journal sits rather than because anything granted it. The service gains authority it
   has never had: `workflow.database_url` is a "Platform coordination metadata login; no customer
   database credentials" (`crates/zeroship-workflow-server/src/config.rs`), and holding the
   journal puts creator values at rest under that login. Open 7 is where that gain is bounded,
   and this is why it is not a formality.

7. **ANSWERED - no, and the assertion is not narrowed.** Creator payload does not live in the
   platform's coordination schema. `metadata_schema_has_no_customer_authority_and_ids_are_bytewise`
   in `crates/zeroship-workflow-server/tests/coordinator.rs` stands as written: no column of
   `workflow_manager` is json, jsonb or bytea, or named `input`, `output`, `history`,
   `payload_url`, `database_url` or `task_token`.

   **One journal shape, not two.** The dev tier is not a constraint that forces a second shape.
   `main` in `crates/zeroship-cli/src/main.rs` opens a `StorageStore` unconditionally - the
   comment there reads "Storage plugin: always on in dev" and the backend defaults to
   `file://.zeroship/storage`, the `LocalFs` backend in
   `crates/zeroship-storage/src/backend/local.rs` - and hands it to the workflow host beside its
   journal binding. `LocalHost::start` in `crates/zeroship-cli/src/workflow/host.rs` then opens
   it as a `zeroship_workflow_runner::PayloadObjects`, the same call
   `crates/zeroship-worker/src/workflow_creator.rs` makes in production. A dev-tier journal can
   reach object storage on the same code path a production one does, so the SQLite tier takes
   the payload-free shape too.

   **What the decision requires, and why it is not a threshold flip.** `promote` in
   `crates/zeroship-workflow/src/service/payloads.rs` does not move an inline value into
   storage: it takes a `WorkflowOutputRef` that a worker already staged through `stage_payload`,
   resolves the owning row with `owned_reference`, and attaches it through `payload_refs`. The
   only promotion of an inline value anywhere is `PreparedExecution::from_runtime_json` in
   `crates/zeroship-workflow-runner/src/outputs.rs`, which runs in the worker and covers
   `RunCompleted`, `ContinueAsNew`, `Child` and a `step.run` completion, and nothing else. The
   first three are referenced whatever they weigh, and an arm carrying nothing references
   nothing; a `step.run` completion is referenced when the creator asked for it or the value
   exceeds `TaskPayloadLimits::max_inline_bytes`.

   A generation row keeps no inline slot for a run's input. `insert_run`
   (`crates/zeroship-workflow/src/service/app.rs`) writes `input_ref` and refuses a descriptor
   over `AppPolicy::max_input_bytes`, which is the one check every start answers to; and
   `RestartPlan::apply` in `crates/zeroship-workflow/src/service/control/restart.rs` carries the
   previous generation's reference forward.

   Staging happens in two places, both ahead of the transaction that takes `lock_app` and
   `lock_run`: `AppBackend::start` in `crates/zeroship-workflow/src/service/backend.rs` and the
   activation in `crates/zeroship-workflow/src/service/cron.rs`, each through
   `stage_start_input`. The child and the continuation stage nothing, because the runner staged
   what they carry, so `journal.rs` and `frontier.rs` pass a descriptor and reach no object at
   all. A schedule's input rides inline on `__zeroship_workflow_deploys.manifest`, and the
   activation is what turns it into an object.

   A run's result on the generation is `output_ref` alone, a payload reference the
   `RunUpdate::Completed` arm of `apply` in `crates/zeroship-workflow/src/service/frontier.rs`
   writes after promoting it, refusing an inline value outright. The successor a `continuedAsNew`
   close hands a run's work to is a typed platform-minted column that `finish_run` in the same
   file writes beside the terminal `RunState::ContinuedAsNew` in
   `crates/zeroship-core/src/workflow_coordination/lifecycle.rs`, and it reaches a caller as
   `RunStatus.continued_as_new_run_id` in `crates/zeroship-workflow/src/operations.rs`. So
   platform data on a generation needs no home inside a payload.

   **An empty list is not a payload-free journal.** The assertion matches a column's type or its
   name, so creator payload held in a text column under another name passes it.
   `__zeroship_workflow_steps.record` is a serialized `StoredCheckpoint` wrapping the
   `StepCheckpoint` declared in `crates/zeroship-workflow/src/engine.rs`, whose `output` and
   `error` are creator values while `child_input_ref` is a descriptor. `finish_run` writes a
   creator error into `__zeroship_workflow_generations.error`, a column the assertion's predicate
   does not reach.
   `publish` in `crates/zeroship-workflow/src/service/signals.rs` and `crates/zeroship-workflow/src/service/fanout.rs`
   store `options.payload` into `__zeroship_workflow_signals.payload` and
   `__zeroship_workflow_broadcasts.payload`, and `__zeroship_workflow_deploys.manifest` carries
   every `ScheduleRegistration` input a deployment declared. Emptying that set alone turns the
   assertion green and leaves the property false, which is the failure this entry was written to
   prevent. A payload-free journal is the whole set becoming references, and that set is empty:
   the predicate was not widened to reach `__zeroship_workflow_steps.record`,
   `__zeroship_workflow_generations.error`, `__zeroship_workflow_signals.payload`,
   `__zeroship_workflow_broadcasts.payload` or `__zeroship_workflow_deploys.manifest`, which
   carry creator values under names and types it does not match, so the test names all five
   under its own `WHAT THIS DOES NOT SEE` heading. An empty result marks where this ends, not
   where the journal stops holding creator bytes.

   **Where the property is checked.** The coordinator fixture builds `workflow_manager` from
   `zeroship_workflow_server::coordinator::SCHEMA_SQL` and
   `zeroship_workflow_manager::deployments::POSTGRES_SCHEMA`, and the journal is in neither, so
   that assertion reads a schema the journal never reached.
   `journal_payload_columns_are_a_closed_set` in
   `crates/zeroship-workflow-server/tests/platform_schema.rs` runs the same predicate against the
   schema the migration corpus installs, as a closed set: a payload-shaped column appearing
   anywhere in `workflow_manager` fails there, and emptying the set it names is what this
   decision looks like from the gate. Plan step 2 waits on that set being empty: a reader in
   `workflow_manager` is what puts creator payload at rest in the platform schema, so the
   promotion lands before the reader rather than beside it.

8. **ANSWERED - a run input is an ordinary payload object, and the continuation marker becomes a
   typed field.** These are not implementation detail: each changes something a creator can
   observe, so they are settled here before a slice assumes one.

   **A run input is an ordinary payload object.** `generations.input_ref` stays nullable, an empty
   input mints no object, and an input that does become one counts against
   `AppPolicy::max_payload_objects` and `AppPolicy::max_payload_storage_bytes` in
   `crates/zeroship-core/src/workflow_policy.rs`, with no exemption. Those are the same decision,
   not separate ones: `WorkflowService::stage_payload` in
   `crates/zeroship-workflow/src/service/payloads.rs` is the single admission gate, and an input
   consumes a counter only by minting an object there. A continuation seed already does.
   `PreparedExecution::from_runtime_json` in `crates/zeroship-workflow-runner/src/outputs.rs`
   references a `StepOutcome::ContinueAsNew` seed, and `PreparedExecution::stage` uploads it
   through that gate.

   A nullable reference does not reach the creator as `undefined`. `start_body` in
   `crates/zeroship-workflow-v8/src/v8_class.rs` resolves a missing input to `Value::Null` before
   anything durable sees it, and `WorkflowTrigger.input` in
   `crates/zeroship-workflow/src/execution.rs` carries no `skip_serializing_if`, so `trigger.input`
   reads the same either way. An `undefined` trigger input is a separate change to both.

   The accepted cost: "no input" and "reference never attached" become one observable state,
   because `TaskPayloadReader::input` in `crates/zeroship-workflow-runner/src/payloads.rs` returns
   success on absence.

   **The continuation successor is a typed field beside a distinct terminal state.** The nullable
   `continued_as_new_run_id` column on the generation, added by
   `crates/zeroship-workflow-schema/schema/migrations/0004_continued_as_new_successor.ts`, holds
   the successor run id and is set on any close that produces one, and continued-as-new is its own
   `RunState` in `crates/zeroship-core/src/workflow_coordination/lifecycle.rs`, among
   `RunState::TERMINAL`. The `RunUpdate::ContinuedAsNew` arm of `apply` in
   `crates/zeroship-workflow/src/service/frontier.rs` names the successor on the `Terminal` it
   hands `finish_run`. Temporal shapes it this way: the successor is the typed
   `new_execution_run_id` and never rides in the result payload, and the typed field and the
   distinct status are independent, since `new_execution_run_id` also appears on an ordinary
   completed execution for a cron successor.
   What it buys: a caller can tell a run that returned a value from a run that continued, without
   matching a key inside creator-controlled JSON that a creator can also produce.

   **`RunStatus.output` is a reference descriptor and nothing else.** `AppWorkflows::status` in
   `crates/zeroship-workflow/src/service/app.rs` emits a descriptor, `StatusOutputRef` in
   `packages/workflows/src/index.ts` declares that one shape, and `docs/reference/workflows.md`
   states it under "Large Outputs". A creator holding a run id reads the bytes through
   `WorkflowBackend::read_output` in `crates/zeroship-workflow/src/backend.rs`, surfaced to
   creators as `readOutput` on `WorkflowRun` in `crates/zeroship-workflow-v8/src/v8_class.rs`.

   **Staging has a task-less path, and both task-less callers reach it.** `stage_payload`
   requires a worker identity, a task id and a task token,
   and its only non-test caller is `HostPayloads::stage` in
   `crates/zeroship-workflow-runner/src/payloads/objects.rs`, reached from a worker that is
   executing a task. `stage_app_payload` beside it takes an `AppId` and proves only that the
   caller may act for the app: its `StagingScope::Unowned` arm names no location, so the row's
   `run_id`, `generation` and `task_id` are NULL and an edge in `payload_refs` is what owns the
   bytes once a run attaches them. `promote` in
   `crates/zeroship-workflow/src/service/payloads.rs` creates no object; it takes a reference a
   worker already staged. So the continuation stages today, and a child's input leaves a worker
   holding a task and could stage on the same path, while `AppWorkflows::start` called from a
   request handler has no task at all and `AppWorkflows::cron_job` in
   `crates/zeroship-workflow/src/service/cron.rs` starts its scheduled run under a `JobLease`
   rather than a task token. Both route through `stage_app_payload` by way of
   `stage_start_input`, which is what leaves the generation row holding a descriptor and no
   value.

   The `insert_run` callers also differ in how far a reference form is from them.
   `AppWorkflows::start` is additive: `StartOptions` in `crates/zeroship-workflow/src/operations.rs`
   can grow a reference field beside its `input`. The child run and the schedule are not. A child's
   input arrives on `StepOutcome::Child` in `crates/zeroship-workflow/src/engine.rs`, and a
   schedule's on `ScheduleRegistration` in `crates/zeroship-workflow/src/service/schedules.rs`,
   which `record_verified` in `crates/zeroship-workflow/src/service/deploys.rs` stores in
   `__zeroship_workflow_deploys.manifest`, so both are wire-format changes. None of this lands in
   `zeroship-control`: `DeployRegistration` appears in the worker, workflow, workflow-runner and
   workflow-v8 crates and not in control, and control receives only the identity projection
   `manager_schedules` in `crates/zeroship-workflow/src/service/bundle.rs`.

---

## Do-not notes

- **Do not give the worker a DSN to the journal database.** It is the one process that executes
  creator code, and the journal's only tenant separation is its `app_id` columns. A shared
  journal reachable by SQL from that process is a cross-tenant surface with no fence, and
  `db_posture` will not catch it because a workflow database carries no `zeroship` schema.
  If a future design does put SQL to a shared journal anywhere outside the service, every table
  needs a fence of its own first - a policy per table and a per-app principal, scoped by
  something the executing process cannot choose for itself. Half a fence is worse than none,
  because it reads as protection: the `app_id` column looks like a tenant boundary in a query
  and is only a filter.

- **Do not make the storage seam the RPC boundary.** The store spans roughly twenty tables; the
  creator-facing backend is seven methods. Move the whole engine, not the store.

- **Do not leave the journal in a creator schema and grant it explicitly.** That is the interim
  repair for defect 3, and it re-establishes the two ownership defects it was written to work
  around. If the ordering forces it, delete it in the same change that relocates the journal.

- **Do not assume a shared transaction was ever available.** Replay plus idempotency keys is the
  model. Any future step that relies on a journal record and a data write committing together
  is relying on something that has never been true and cannot be true across processes.

- **Do not keep `SCHEMA_PLACEHOLDER` on the PostgreSQL path "for symmetry".** One fixed schema
  needs no substitution, and a placeholder that is always replaced with the same value is a
  seam inviting a caller to pass something else. It survives for SQLite because that tier has a
  genuine reason.

---

## History

`docs/proposals/2026-08-28-app-database-decoupling.md` is the sibling that makes defect 4 real
and defect 3 urgent; its Open 3 is closed by this document.
`docs/proposals/2026-08-28-migration-record-consolidation.md` sets the rule this one deliberately
departs from: a creator-owned journal is correct for migrations, where corruption breaks only
the creator, and wrong for workflows, where the platform is executing against it.
