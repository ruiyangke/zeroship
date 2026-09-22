# Moving the workflow journal out of creator databases

**Status.** PROPOSED. Nothing here is built. The journal is installed into the creator's own
schema today and written by the worker over the creator's own connection; this moves storage
and the durable fold into the workflow service, leaving execution where it is.

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
`WorkflowBackend` with six methods: `start`, `status`, `signal`, `transition`, `restart`,
`read_step_output`. `crates/zeroship-workflow-v8/src/lib.rs` already composes it through a
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
    worker   V8 task executor + WorkflowBackend RPC client (six methods)
    service  durable fold + journal store (SQL, workflow_manager) + coordination
```

The seam already exists and is narrow. `WorkflowBackend`'s six methods are the whole
creator-facing surface, `crates/zeroship-workflow/src/engine.rs` is "Pure durable-workflow fold
and DTO contracts" with no storage in it, and the V8 binding is already built to take a backend
rather than a database.

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

**The creator seam is six methods; the execution seam is its own.** It would be a much larger
proposal if the storage seam were the RPC boundary, because the store spans roughly twenty
tables. It is not: the engine's fold is pure and the creator-facing backend is narrow, so the
whole engine moves server-side and the wire carries `start`, `status`, `signal`, `transition`,
`restart` and `read_step_output`.

Say plainly that those six are the creator-facing surface and not the whole wire. A worker also
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
reported execution of a run body. A reader who takes "six methods" as the whole surface will
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

Each step below lands on its own and is verifiable on its own. Nothing here is a flag day except
step 5, and that one is a switch rather than a migration only because of Open 5.

**What is easy, and what is not.** The creator seam is the easy half: `WorkflowBackend` is six
methods with two implementations already behind a factory in `crates/zeroship-workflow-v8/src/lib.rs`,
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

2. **Let `AppBackend` bind the service's own store.** It already implements `WorkflowBackend`;
   today it assumes a creator binding. Make the binding a parameter rather than an assumption.
   Still nothing remote. Verify the existing suites pass with the service store behind it.

3. **Serve the six creator methods, and add a client for them** in `zeroship-workflow-client`.
   Not yet wired into the worker. Verify each method round-trips against the service store.

4. **Carry the three direct calls across, merged into the claims that already cross.** This is
   the step that earns its own review, and it is not "add a remote `TaskTransport`" - that trait
   is test-only, and building against it would ship a remote implementation of something the
   worker never calls. What crosses is `accept_job`, `heartbeat_job` and `complete_job`, and
   each already sits immediately beside a call that is remote today: the manager claim, the
   manager heartbeat, the manager settlement. Merge them - the assignment rides the claim reply,
   one renewal carries both leases, the frontier rides the settlement - and the relocation adds
   no round trip. Bolted on as separate endpoints it doubles them, which is a capacity cost
   rather than a latency one. `heartbeat_job` must still advance the manager's evidence that an
   execution began, `complete_job` must still carry the outcome batch the fold consumes, and a
   reported execution of a run body must still be counted once. Verify by mutation rather than
   by suite: break each property in turn and require a test to fail on it.

5. **Cut the worker over** to the remote variants.

6. **Delete the creator-schema path** - `ensure_journal` and its route, the journal bundle,
   `JournalManager`, the worker's repair path, and `SCHEMA_PLACEHOLDER` on the PostgreSQL side.
   Only once step 5 is green.

7. **Drop `zeroship-data-orm` from the crate the worker links.** The last step of moving the
   `service/` tree, not a deletion available earlier.

**Measure the latency question before step 1, not after step 4.** Open 1 decides whether this is
viable under load, and it is answerable now: transitions already batch, so a dispatch's outcome
count can be measured against a round trip on today's code. That is the cheapest de-risking
available and it gates the whole design rather than one step.

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

   Two shipped bounds already disagree about the crossing this move would create:
   `AppPolicy::max_journal_bytes` against the client's `max_response_bytes`, and
   `AppPolicy::max_input_bytes` against `max_request_bytes` - and the worker takes the client
   defaults, `client_options` in `crates/zeroship-worker/src/workflow_host.rs` returning
   `ClientOptions::default()` with no configuration surface to change them. The pairs are not
   like for like. The policy bounds count stored `StepCheckpoint` JSON, one per item and one
   accumulated per run generation; the client bounds count a single HTTP message. And `replay`
   in `crates/zeroship-workflow/src/service/journal.rs` narrows `StepCheckpoint` to
   `JournalStep` and drops retrying rows, so the journal bound is an upper bound on wire bytes
   rather than a measure of them.

   The decision Plan step 4 cannot avoid: when the dispatch reply carries the journal and the
   settle request carries the execution, which side is the authority - does the transport bound
   rise to admit what the policy already admits, or does the policy bound fall to what the
   transport will carry? Deciding how the journal crosses is the real design work behind this
   move. See Open 2, which is the same question seen from the payload side.

2. **Payload size on the wire.** `read_step_output` and step inputs cross the boundary.
   `WorkflowOutputRef` in `crates/zeroship-workflow/src/engine.rs` suggests large outputs are already referenced rather than
   inlined; whether that covers every payload path needs checking before the cutover, not after.

3. **Does `workflow_manager` become its own database?** NEEDS-DECISION, deferrable. It is a
   schema in the control database today with its own migrator, login and search path. Moving
   the journal into it puts creator-volume rows - runs, pages, receipts, payloads - in the
   control plane, which is the coupling `docs/proposals/2026-09-05-gateway-central-database-decoupling.md`
   is fighting on a different axis. The promotion is contained; the question is when.

4. **Is the service zone-local? This is half of Open 1's answer, not a footnote.** Every number
   behind Open 1 was taken over loopback. A crossing per dispatch is free at that distance and
   is not free across a wide area, so if the service is global the term Open 1 dismisses becomes
   the dominant one. It should be zone-local - one workflow store per zone, an app's runs living
   in its own zone - consistent with the decoupling's co-location rule, and worth deciding
   before the cutover rather than inheriting.

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
   is the side that publishes it. The credential is the cost. `collect_job`
   (`crates/zeroship-workflow/src/service/collection.rs`) takes a `PayloadDeleter`
   (`crates/zeroship-workflow/src/service/payloads.rs`), whose production implementation is
   `PayloadObjects` (`crates/zeroship-workflow-runner/src/payloads/objects.rs`), opened over "a
   store whose credentials are private to the workflow host". Collection needs the journal and
   the object store in one operation, so either that credential reaches the service or collection
   joins `accept_job`, `heartbeat_job` and `complete_job` on the wire. That is undecided.

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
   `crates/zeroship-workflow-runner/src/outputs.rs`, which runs in the worker and
   references a value only when the creator asked for it or it exceeds
   `TaskPayloadLimits::max_inline_bytes`; it covers `RunCompleted`, `ContinueAsNew` and a
   `step.run` completion, and nothing else.

   `generations.input` is written in one place, `insert_run`, and four production callers reach
   it (`crates/zeroship-workflow/src/service/app.rs`). `AppWorkflows::start` is outside any fold.
   The other three are inside one: `journal.rs` starts a child run from
   `StepCheckpoint.child_input`, `cron.rs` starts a scheduled run from the `ScheduleRegistration`
   carried on the deploy registration that `__zeroship_workflow_deploys.manifest` stores, and
   `frontier.rs` starts the successor of a continuation. Promoting there means object I/O under
   the app and run locks `lock_app` and `lock_run` already hold, and neither `child_input` nor a
   schedule's input has a reference form to arrive in. Making those two referenceable is a
   change to `StepOutcome` and to the deploy manifest, not a mechanism added beside them.

   `generations.output` has one writer, `finish_run` in `crates/zeroship-workflow/src/service/frontier.rs`,
   and it carries three things: nothing, a creator run output, and `{"continuedAsNew":id}`, which
   is platform data rather than payload and needs a home that is not a payload reference.

   **An empty list is not a payload-free journal.** The assertion matches a column's type or its
   name, so creator payload held in a text column under another name passes it.
   `__zeroship_workflow_steps.record` is a serialized `StoredCheckpoint` wrapping the
   `StepCheckpoint` declared in `crates/zeroship-workflow/src/engine.rs`, whose `output`, `error`
   and `child_input` are creator values. `finish_run` writes a creator error into
   `__zeroship_workflow_generations.error`, the column beside the two this entry is about.
   `publish` in `crates/zeroship-workflow/src/service/signals.rs` and `crates/zeroship-workflow/src/service/fanout.rs`
   store `options.payload` into `__zeroship_workflow_signals.payload` and
   `__zeroship_workflow_broadcasts.payload`, and `__zeroship_workflow_deploys.manifest` carries
   every `ScheduleRegistration` input a deployment declared. Removing `input` and `output` alone
   turns the assertion green and leaves the property false, which is the failure this entry was
   written to prevent. A payload-free journal is the whole set becoming references, and the two
   named columns are where it starts, not where it ends.

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

   **The continuation marker becomes a typed field beside a distinct terminal state.** A nullable
   typed successor column on the generation holds the successor run id and is set on any close that
   produces a successor, and continued-as-new becomes its own `RunState` in
   `crates/zeroship-core/src/workflow_coordination/lifecycle.rs`, joining `RunState::TERMINAL`. The
   `{"continuedAsNew":id}` marker that the `RunUpdate::ContinuedAsNew` arm of `apply` in
   `crates/zeroship-workflow/src/service/frontier.rs` hands to `finish_run` goes away. Temporal
   shapes it this way: the successor is the typed `new_execution_run_id` and never rides in the
   result payload, and the typed field and the distinct status are independent, since
   `new_execution_run_id` also appears on an ordinary completed execution for a cron successor.
   What it buys: a caller can tell a run that returned a value from a run that continued, without
   matching a key inside creator-controlled JSON that a creator can also produce.

   **`RunStatus.output` already returns a reference descriptor, so what is left there is a
   defect.** `AppWorkflows::status` in `crates/zeroship-workflow/src/service/app.rs` emits a
   descriptor for a referenced output, `StatusOutput` in `packages/workflows/src/index.ts` declares
   the union, and `docs/reference/workflows.md` states it under "Large Outputs". The open part is
   tracked as a defect rather than decided here: the descriptor `status` emits and the
   `StepOutputRef` the SDK declares disagree on `kind`, and a creator holding only a run id cannot
   dereference what `status` returns, because `WorkflowBackend::read_step_output` in
   `crates/zeroship-workflow/src/backend.rs` addresses a step by name and a run's final output has
   none - `finish_run` records the terminal outcome on the generation row and on no step.

   **Staging is task-scoped, and that is the constraint on emptying `generations.input`.**
   `stage_payload` requires a worker identity, a task id and a task token, and its only non-test
   caller is `HostPayloads::stage` in `crates/zeroship-workflow-runner/src/payloads/objects.rs`,
   reached from a worker that is executing a task. `promote` in
   `crates/zeroship-workflow/src/service/payloads.rs` creates no object; it takes a reference a
   worker already staged. So the continuation stages today, and a child's input leaves a worker
   holding a task and could stage on the same path, while `AppWorkflows::start` called from a
   request handler has no task at all and `AppWorkflows::cron_job` in
   `crates/zeroship-workflow/src/service/cron.rs` starts its scheduled run under a `JobLease`
   rather than a task token. Emptying the run-input column therefore needs either a task-less
   staging path or a decision that some inputs stay inline. That is open.

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
  creator-facing backend is six methods. Move the whole engine, not the store.

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
