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

Open 3 names three pieces behind a store of its own, and all three have landed: no migration in the
corpus writes into two databases' worth of schemas, and the two tables the service
owns have moved into `workflow_manager`, which deletes a cross-database write rather than
transporting it, and the service has a compose deployment. What remains is severing the reads
that stay, plus one transport fence the deployment names - and the one of those that is
authentication is a trust-model decision rather than plumbing.

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
wire and client halves land green and unwired, but its server half shares step 5's flag day,
because the job endpoints it changes are the live claim path rather than a new one nobody
calls.

**What is easy, and what is not.** The creator seam is the easy half: `WorkflowBackend` is
narrow, with two implementations already behind a factory in `crates/zeroship-workflow-v8/src/lib.rs`,
so a third that speaks HTTP is mechanical. The execution seam is the work. `TaskTransport`'s only
production implementor, `WorkerTasks`, holds the service and calls it directly, so it has to
become remote - and it is the half carrying the durability semantics. Read the steps with that
asymmetry in mind: steps 1 to 3 are preparation, step 4 is the project.

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
   `crates/zeroship-worker/src/workflow_creator.rs` constructs on `WorkerTasks`, whose `stage`
   opens journal transactions mid-execution with no manager call to merge into.

   **The dependency gate decides where the client lives.**
   `workflow_process_dependencies_follow_crate_ownership` in `xtask/tests/workflow_architecture.rs`
   forbids `zeroship-workflow-client` any edge to `zeroship-workflow`, so the merged client
   methods cannot live there unless `TaskAssignment`, `WorkflowExecution`, `JobReceipt` and their
   neighbours move to `zeroship-core`. Putting the merged client in `zeroship-workflow-runner`,
   which may depend on both, is the alternative. That fork is the largest decision in this step.

   **One bound is already wrong for the merged settle.** `Options::default` in
   `crates/zeroship-workflow-client/src/lib.rs` takes `max_request_bytes` from
   `MAX_INPUT_BYTES_CEILING` while a settle body carrying the outcome batch answers to
   `max_journal_bytes`, which `append` in `crates/zeroship-workflow/src/service/journal.rs`
   enforces against the journal ceiling. Open 1 counts three numbers on the request path and
   does not notice that the first derives from the wrong one for this endpoint.

   **The concurrency the merge introduces.** Today every accept for an app runs in the one worker
   process holding that app's binding. Served, accepts run in `RunService`, which is one per HTTP
   worker thread because its store is `!Send`, across every replica. `lock_app_state`
   (`crates/zeroship-workflow/src/service/app.rs`) is a filtered row update and does serialize
   across connections, so the single-assignment invariant holds - but the compare-and-swap in
   `tasks::assign` stops being unreachable by construction and becomes what stands between a lock
   bug and two live tasks on one run. Nothing exercises its failing branch today, because
   `reclaim` refuses first. Close that here rather than after.

   Collection does not widen this step. Open 6 records why: the journal and the object store
   are already decoupled by a commit on that path, and the manager already owns when it runs.
   It is separate, smaller work carrying its own contract.

5. **Cut the worker over** to the remote variants.

6. **Delete the creator-schema path** - `ensure_journal` and its route, the journal bundle,
   `JournalManager`, the worker's repair path, and `SCHEMA_PLACEHOLDER` on the PostgreSQL side.
   Only once step 5 is green.

7. **Drop `zeroship-data-orm` from the crate the worker links.** The last step of moving the
   `service/` tree, not a deletion available earlier.

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

       SELECT id,deploy_hash,deleted_at FROM zeroship.apps LIMIT 0;
       SELECT id,app_id,deploy_hash,retention_state FROM zeroship.app_deploys LIMIT 0;

   The crossing recurs on the request path and inside the manager. `active_key` in
   `crates/zeroship-workflow-server/src/auth.rs` answers every worker-authenticated call from
   `SELECT public_key FROM zeroship.worker_instances WHERE id=$1 AND status='active' AND
   expires_at > now()`, and `crates/zeroship-workflow-manager/src/policy/control/models.rs`
   projects Control-owned policy inputs - `apps` and `plans` - which the manager reads to
   compute an app's authority. The highest-frequency crossing is in
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
   `crates/zeroship-workflow-manager/src/policy/control/store.rs` holds a handle each and keeps
   the bracket: the ledger row's lock is taken before the inputs are read, on the other handle,
   with the transaction still open. `service_authn.service_assertion_replay` is the same shape -
   the audience is checked before the store is consulted, so a per-service table keeps single
   use - but whether it should move is the live disagreement recorded below.

   **`docs/proposals/2026-09-20-platform-service-database-split.md` already assigns an owner to
   every table in this set, and this document should not re-derive one.** Its table gives
   `workflow_policy_ledger` and `workflow_rollout_config` to workflow, which agrees with the
   paragraph above. It **disagrees** about the replay store, which it lists under "Shared by
   design, owned by nobody" - so whether a per-service instance is correct is a live
   disagreement between two proposals rather than a settled question, and it should be settled
   in that one. It also marks `app_deploys` CONTESTED, "workflow's DDL, control writes", which
   is the same table Plan step 3 records `restart` as unable to resolve.

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

   And one is authentication. `active_key` is what verifies a worker, so severing it is a
   trust-model decision rather than a plumbing one, and it is the piece to decide before the
   rest. `crates/zeroship-workflow/src/service/capability.rs` already mints and verifies
   control-signed, audience-bound, short-lived capabilities and nothing in production calls it;
   `crates/zeroship-workflow-server/src/server.rs` already refuses to start without Control's
   verification key. The machinery for a control-signed enrolment attestation exists and is
   unwired.

   **A gap this uncovered, unrelated to the move.** `workflow_rollout_config` has no production
   writer: every caller of `set_rollout` and `set_plan_policy` in
   `crates/zeroship-workflow-manager/src/policy/control/store.rs` is a test, and `read_source`
   inner-joins that table, so a deployment with no row published by hand fails every policy
   observation. Whether hand-provisioning is the intended posture is its own question.

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

   **What it cannot do yet is reach Control, and the reason is a trust-model decision rather
   than a setting.** `Transport::configuration` in
   `crates/zeroship-workflow-client/src/transport.rs` admits an origin only when the scheme is
   `https`, or the scheme is `http` and the host is a loopback IP LITERAL. Every platform
   service on the compose bridge speaks plain HTTP under its service name and nothing terminates
   TLS between containers, so `http://control:9090` is refused at startup and `https://` binds
   over a transport that cannot handshake with a plaintext peer. This client is the only one
   with that fence - the gateway reaches Control at `--control-url http://control:9090`. Either
   the transport relaxes for a private network or the platform grows internal TLS, and that is
   the same class of decision as the authentication one above.

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
