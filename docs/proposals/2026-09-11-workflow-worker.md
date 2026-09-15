# Workflow manager, durable job queue and workers

**Status:** Agreed architecture with protocol decisions still identified below.
Implementation is in progress. Native coordinator and queue operations share ORM
transactions. Their database, authenticated host and platform-schema contracts
have passed verification; remaining bounded creator operations are in progress.
The local CLI host runs on the native manager and the ordinary job consumer.
Manager scheduling, creator outbox publication and the simple worker consumer
have not completed their production cutover.

The manager owns when work becomes runnable and how it is delivered. An ordinary
app worker pulls an authorized job, executes a bounded operation against creator
storage, commits the result and acknowledges closed metadata. A workflow can
outlive the worker, isolate and delivery attempt that advanced it.

The principal rule is process ownership: **workers access only authorized creator
databases; Control and the workflow manager access only the Control database.**
Production places these processes in separate zones with private databases.
Neither side receives the other side's database credentials. Customer inputs,
history and results stay in the creator zone throughout the protocol.

This proposal supersedes the earlier
[control-plane design](2026-07-05-durable-workflows-design.md),
[scheduler registration design](2026-07-08-durable-workflows-scheduler-worker-design.md)
and [implementation plan](2026-07-05-durable-workflows-implementation-plan.md).
The [workflow reference](../reference/workflows.md) describes existing APIs;
statements marked as target or required work here are not claims of shipment.

Read by concern:

- [Components and private zones](#components-and-private-zones),
  [worker identity and placement](#worker-identity-registration-and-placement),
  [policy bindings and leases](#policy-bindings-and-authenticated-leases).
- [Storage and transactions](#storage-inventory-and-schema-ownership),
  [delivery protocol](#durable-job-protocol), [lifecycle state](#lifecycle-state-and-authority).
- [Trigger sequences](#trigger-lifecycles),
  [recovery and capacity](#recovery-responsibility-and-execution-capacity).
- [Deployments and retention](#deployment-pins-upgrades-and-retention),
  [payloads and effects](#payloads-effects-and-collection).
- [Crates and APIs](#rust-apis-and-dependency-direction),
  [configuration and local development](#configuration-and-local-development).
- [Operations](#operations-security-and-backpressure),
  [failure and verification](#failure-behavior-and-verification),
  [remaining work and decisions](#implementation-progress-and-remaining-decisions).

Terms used throughout:

| Term | Meaning |
| --- | --- |
| Scope | The app whose jobs and creator binding an operation may use. |
| Run | A durable invocation of a workflow, independent of the process executing it. |
| Generation | A run's execution incarnation, with its own pinned code and replay history. Restart creates another generation. |
| Frontier | The current committed point from which execution may advance; its revision changes when the journal advances. |
| Job | A durable instruction to perform a bounded operation. A run can require successive jobs. |
| Attempt | A particular delivery of a job to a worker. Redelivery changes the attempt, not the job's meaning. |
| Fence | An identity or revision checked by a write so an obsolete execution cannot modify current state. |
| Receipt | A durable record of an accepted operation or committed outcome, used to answer retries consistently. |
| Publication intent | A creator transaction's durable instruction to publish metadata after commit; stored in an outbox. |
| Recovery responsibility | The manager's obligation to revisit an app even when it cannot see unpublished creator work. |
| Deployment hold | A durable reference preventing normal bundle reclamation while queued work or creator history still needs its code. |
| Policy binding | A trusted host's immutable association between an app handle and a local authority generation; replacement retires existing handles. |
| Policy lease | Authenticated, time-bounded permission to apply a policy under an exact worker key and assignment. |

## Components and private zones

```text
PLATFORM ZONE

  Creator management / normal deployment
                  |
                  v
            +-----------+        metadata         +---------------------+
            | Control   |------------------------>| Workflow manager    |
            |           |<--- scoped hold API ----|                     |
            | Authz     |                         | Cron and timers     |
            | Deploys   |                         | Placement/recovery  |
            | Retention |                         | Management commands |
            +-----+-----+                         | Durable job queue   |
                  |                               +----------+----------+
                  |                                          |
                  +---------------> Private Control DB <-----+
                                       metadata only

=================== authenticated service API ========================
                Workers initiate polling and reporting

CREATOR EXECUTION ZONE

  Normal app request                    Outbound poll / submit / ACK
          |                                          |
          v                                          v
  +------------------------------------------------------------------+
  | Ordinary zeroship-worker                                         |
  |                                                                  |
  | App-scoped start / signal / reads     Bounded job consumer         |
  |                                        |                         |
  | Trusted app context -> customer engine -> pinned app code in V8    |
  +-----------------------+-------------------------+------------------+
                          |                         |
                          v                         v
                 Private creator DB          Creator object storage
                 Runs, history, waits,       Inputs, outputs, signals,
                 receipts and outbox         prepared payload objects
```

`zeroship-workflow-server` is the deployable manager host. Its queue is persisted
in the Control database through the ORM. It introduces no broker service or
separate workflow-worker executable. Deployment infrastructure starts ordinary
`zeroship-worker` processes; the manager requests capacity through an injected
host adapter.

The worker retains a consumer loop, execution heartbeats and bounded transport
retry. It has no cron evaluator, due-work scanner or independent maintenance
scheduler. Reconciliation and collection can read the assigned creator journal,
but they run because the manager delivered a bounded job. Waiting for a signal,
child or deadline releases the execution slot and isolate.

| Component | Owns | Excludes |
| --- | --- | --- |
| Control | Creator authorization, enrollment, normal deployment publication, policy, routing metadata and deployment retention APIs. | Creator journal access and workflow advancement loops. |
| Manager | Calendar evaluation, durable jobs and deadlines, placement, delivery attempts, command delivery, recovery responsibility and capacity demand. | Customer code, payload credentials and creator DB connections. |
| Worker customer engine | App-scoped acceptance, replay, lifecycle decisions, journal fences, inputs, history, results and durable publication intents. | Platform tables and scheduling discovery. |
| V8 adapter | Creator-facing handles and execution of the selected app deployment under resource limits. | Queue credentials or direct manager persistence. |
| Gateway | Normal routing and request authentication. | A workflow scheduler, journal reader or private workflow-advance control channel. |
| Deployment host | Starting eligible workers and supplying trusted app context in the proper execution zone. | Deciding customer workflow transitions. |

The workflow subsystem checks app authority. Existing creator and app-request
authorization still applies at their entrypoints; the queue adds no end-user
permission model. An app-scoped native handle cannot select another app's journal,
schema or object namespace. Follow the
[data-system contract](../architecture/data-system.md). Table naming and schema
visibility are not platform authorization: creator-owned rows cannot grant
placement, turn admission on or manufacture a platform deployment hold.

## Worker identity, registration and placement

Registration records a running process; it does not start a process. Enrollment,
liveness, app placement and execution fencing answer different questions.

```text
Deployment host          Worker              Control                 Manager
      |                     |                    |                       |
      |--- start ---------->|                    |                       |
      |                     |-- enroll key ----->|                       |
      |                     |<-- instance ID ----|                       |
      |                     |                    |                       |
      |                     |----- signed registration ---------------->|
      |                     |                    |       verify enrolled |
      |                     |                    |       instance/key;   |
      |                     |                    |       record capacity |
      |                     |<---- registration acknowledgement --------|
      |                     |                    |                       |
      |                     |                    |-- authorized scope -->|
      |                     |                    |       select eligible |
      |                     |                    |       worker placement|
      |                     |----- poll assigned work ----------------->|
      |                     |<---- job + delivery authority ------------|
      |                     |                    |                       |
      |                     | execute against creator DB/storage        |
      |                     |----- committed outcome + ACK ------------>|
```

| Authority | Record or capability | Meaning |
| --- | --- | --- |
| Enrollment | `zeroship.worker_instances` and the instance signing key. | Control authorized this instance identity; the presented key must match an active enrollment. |
| Workflow registration | `workflow_manager.workers`. | The instance reports capacity, ready/draining state and liveness. |
| Scoped assignment | `workflow_manager.assignments`. | A particular worker may consume this app's work under a revision and expiry. |
| Delivery | A leased manager job and its attempt. | This instance may process this particular job under the current placement. |
| Creator execution fence | Customer journal claim, generation and frontier revision. | This execution may publish the next customer transition. |

A worker uses its enrolled instance key for service assertions. The receiver
verifies issuer, audience, permitted endpoint, assertion lifetime and replay
protection before reading the buffered request. A worker ID in JSON is only a
selector. A generic worker-role signer cannot substitute for an enrolled
instance key. Every transport retry uses a fresh assertion and the same durable
operation identity.

The manager admits placement only within platform-authorized app and execution
zone eligibility. Spare capacity is not authority to serve any app. Eligibility
comes from Control's app and enroller zones, as
[placement eligibility and capacity](#placement-eligibility-and-capacity-provider)
describes; workers cannot nominate database locations or broaden eligibility by
registration.
The worker also verifies that its locally resolved creator binding matches the
assigned app. Routing and database credentials cannot be supplied by the job.

Registration renewal does not revive an expired assignment. Assignment revisions
and released rows are retained so delayed renewals cannot recreate old authority.
Capacity admission is serialized across apps assigned to the same worker.
Ready workers may receive new placements; draining workers may finish authorized
work and reconcile committed outcomes without being selected for new placement.
Draining is terminal for an enrolled worker instance. Registration serializes
against the stored worker row and rejects any later ready heartbeat, including a
request delayed past shutdown or liveness expiry. Restarted processes use a new
enrolled instance identity; registration never resets the old process's drain.

Freshness checks belong around the work they authorize. Authenticate at ingress,
then recheck enrollment and stored placement after waiting for locks and before
admitting the mutation to commit. Reuse the verified request context for these
checks rather than consuming its assertion replay identity again. An unavailable
enrollment source is retryable infrastructure failure, not a durable customer
rejection. Existing placement TTL checks do not replace enrollment revocation.

On graceful shutdown the worker reports draining, closes local admission and
finishes or abandons bounded execution through the protocol. A crash expires
liveness and delivery authority while durable work and scope responsibility
remain. Healthy registration does not discharge an obligation to recover an
unpublished creator intent.

### Enrollment bootstrap and revocation

A worker enrolls with the credential of its deployment unit, called an
enroller: an operator-provisioned key that Control records with exactly one
execution zone. Only the enroller principal may call enrollment; an enrolled
instance key cannot enroll, and no process holds a shared worker role signing
key. Enrollment locks the active enroller row, inserts an instance bound to that
enroller, and is idempotent on the instance public key. A changed key is always
a new instance identity, and registration, leases and receipts keep comparing
the exact key that verified each request.

Revocation is an explicit operator database operation. Revoking an enroller
marks it revoked and marks every instance it enrolled `gone` in one transaction
that serializes with enrollments in flight, so a revoked unit cannot restore
authority by enrolling a fresh identity; a replacement unit needs a newly
provisioned enroller. Retiring a single instance is attribution and hygiene,
not a boundary against a process that still holds its unit's key, so
revocation for cause targets the enroller. A worker that exits gracefully
retires its own instance after its server drains; observed liveness never
writes enrollment status.

Every enrollment reader reads the authoritative row: Control on each internal
request, the manager at ingress and again after lock waits and before commit,
and the CDC relay on its session recheck. Revocation therefore stops new
admissions at the next check; leases already issued keep their original
deadlines while creator fences stay authoritative. An unavailable registry is a
retryable infrastructure failure. Local development composes a trusted
in-process worker and performs no enrollment.

This contract suits long-lived worker replicas that mount their unit key. If
production replicas churn under an orchestrator, the key moves into a
creator-zone host agent that issues single-use enrollment grants; the Control
records and revocation cascade stay the same. A native proof of concept on
branch `poc/workflow-enrollment` passes the contract against a migrated
PostgreSQL database: instance keys are refused at enrollment, revoking an
enroller cascades to Control, the manager and the CDC relay while a sibling
unit stays active, the revocation serializes with a concurrent enrollment on
the enroller row lock, and a lost-reply retry returns the same instance.

**Implementation boundary:** a worker loads only its deployment unit's enroller
credential (`worker.enroller_file`, the enroller id and key in one document)
and spends it on enrolment; no process holds a `svc/worker` role key, the peer
document publishes none, and Control refuses a `svc/worker` or
`svc/worker-enroller` assertion minted at role arity. Control imports
enrollers at startup from `control.worker_enrollers_file`: it inserts unknown
enrollers, never reactivates a revoked one, and refuses a file that conflicts
with a recorded enroller without writing. `zeroship dev init` provisions the
host's enroller and the import file, `zeroship dev enroller` adds a
deployment unit, compose mounts both, and `docs/runbooks/worker-enrollers.md`
holds the operator procedure, revocation included. A gracefully stopped worker
retires its own instance through `CONTROL_WORKER_RETIRE`. The worker's
version poll and the `env.workflows` HTTP backend still authenticate with the
shared control key rather than a worker credential, so revoking a unit does
not take that credential from a process that already holds it. The poll is on
the POLLED tier that `docs/proposals/2026-09-05-app-metadata-distribution.md`
replaces, and the backend goes with the workflow-server cutover. Production
startup registration, consumer wiring, zone eligibility (including the foreign
key from `worker_enrollers.execution_zone_id`) and capacity activation remain
cutover work.

### Placement eligibility and capacity provider

Each app belongs to exactly one execution zone, named by Control in
`zeroship.apps.execution_zone_id` when the app is created and frozen by trigger.
An execution zone is an operator-declared set of deployment units that share
creator-side connectivity. Control names it rather than letting a column
default decide: it resolves the zone the creator asked for, or the
deployment's one declared zone when none is named, and refuses to create an app
in a deployment that declares several without saying which. A worker's zone is the zone of the enroller Control
verified when it enrolled, also frozen. Registration carries no zone; the
manager copies it from Control's rows and nothing a worker sends can change it.

Control's host app reads are narrowed to the calling instance's zone. The
version, environment and project data key endpoints verify which instance
signed the call and answer only for apps in that instance's zone, because an
app's environment is its decrypted secrets and its data key is a decryption
capability.

The manager selects workers itself. It takes the app lock, then the worker
lock, and admits a placement only when all of these hold: the app is not
deleted, the zones match, the enrollment is active, the registration is ready
and unexpired, and the worker has capacity. It reads these facts from
Control-owned rows through column grants and an injected eligibility
capability, after the lock waits and again before commit. A revocation that
commits after the second read is caught by the next registration, renewal,
ownership or delivery check, the same eventual admission fence enrollment has.
Archived apps remain placeable for maintenance jobs; policy still refuses their
admission, dispatch and ingress. Deleted apps are abandoned.

A worker that cannot serve an assigned app releases it as refused, and the
manager does not offer that app to that instance again. Release carries a
closed reason and no wake hint, needs no responsible peer, and never discharges
recovery responsibility.

The manager owns placement outright. The endpoints Control used to drive it -
nominated assignment, worker listing and the recovery-scope scan - are gone,
along with the wire types and coordinator operations behind them, so the
predicate above is the only way an app acquires an owner.

The driver's placement lanes key on claimable jobs; the recovery lanes turn due
duties into jobs first, and a closing scope's Close job is a job. An app with
claimable work and no ready eligible owner is placed on free eligible capacity
first. Otherwise its demand is recorded durably in its zone. Each zone has one
declarative capacity target in placement slots, its live placements plus its
unplaced demand, so placing an app leaves the target unchanged. The target's
revision advances only when that number changes, under the zone row's lock, and
one request per revision is claimed in the same transaction. Operator
configuration bounds the target between a floor that keeps capacity warm and a
ceiling that leaves demand beyond it recorded and unplaced, and sets the
hold-down, the claim deadline and the pacing. An injected provider applies the
target outside every lock and replies with progress or a closed, durable,
retryable refusal (`pool_exhausted`, `no_enroller`, `unavailable`). Replies
apply only to the revision and attempt they answered. A lower target applies
only after the idle hold-down. Provider failure keeps jobs, demand and targets
pending.

Scale-down is a drain. A lower target authorises removing nothing: the manager
drains the least loaded registrations, and only while what remains still covers
the target, because registered slots are lumpy and a zone must not shrink below
its own demand. A drained registration is no placement candidate and no ready
owner, so the lane moves its apps elsewhere as they fall due, while the
placements it holds stay valid until the worker finishes or releases them. A
capacity request names an instance as removable only once it holds no live
placement, and an instance becoming removable is itself what makes a paced
request due, since nothing else would tell the provider it may take it away.

A provider holds only scale authority over worker units in one zone. It never
receives creator credentials, secret-mount authority or queue messages. The
local host injects an always-satisfied provider for its trusted in-process
worker. Single-host deployments use a static pool that never starts processes
and reports exhaustion durably.

Per-app provisioning intents were the proof of concept's comparison and are
not a shipping path. Racing replicas converge on one revision and one request
under either contract, but a retried intent starts another worker unless the
provider deduplicates it, and intents do not coalesce apps onto shared workers.
The declarative target is a value rather than an instruction, so a lost reply's
retry starts nothing.

**Implementation boundary:** providers that start processes wait for the
production orchestrator; the local provider and the static pool ship. The
worker does not yet release an app it cannot serve as refused, or ask for
placement when it is handed an app it does not hold - both belong on the
production worker host. The manager's half of each is in place: a refused
release tombstones the pair for the life of that instance, and an unowned app
with claimable work is placed by the lane.

## Policy bindings and authenticated leases

**Native lifecycle, lease transport and Control source implemented; production
worker refresh integration remains open.**
The creator engine accepts trusted `PolicySnapshot` values through immutable
`PolicyBinding` capabilities and ordered `PolicyRefresh` tickets. App handles,
queued backend calls and delivered execution retain the original authority across
asynchronous work. The shared closed raw policy, authenticated lease client and
server route exist. The server installs the native Control-storage provider and
refuses missing or invalid operator policy. Production worker assignment and
policy refresh still require host composition.

### Native binding identity and policy revision

Keep these identities independent:

| Identity | Authority and change rule |
| --- | --- |
| Source policy revision | Control's monotonic revision for the complete effective app policy. A revision identifies immutable policy values. |
| Assignment revision | The manager's placement fence for this app and worker. Policy refresh cannot change or renew it. |
| Host binding generation | An opaque, process-local identity allocated by the trusted host when it replaces an app binding. It is unrelated to run generation or policy revision. |
| Refresh ticket | A local ordering token for a metadata request under a particular binding. It grants no journal authority by itself. |

`HostPolicies` issues a `PolicyBinding` capability containing its registry identity,
app identity and opaque generation. Explicit replacement retires the previous
generation before the replacement can admit work. Revocation retains a tombstone;
neither a late refresh nor a clone of the retired capability can recreate it.
An operation through a retired capability also cannot revoke its replacement.
Generation and ticket identities must not wrap or be reused. The customer engine
does not store these capabilities in creator tables or reconstruct them from SQL.

The production host associates a binding with the exact authorized app, worker,
enrolled signing key and assignment revision. Changing any part of that association
requires explicit replacement. Native `zeroship-workflow` does not need service-key
types: its opaque binding is the local fence, while the host/client validates the
transport identities before installing a snapshot through that binding.

Construct `AppWorkflows` with `WorkflowService::register_app(&binding)` after
installing a snapshot, or use `bind_app(&binding)` for an existing journal. The handle retains that exact binding; cloning it does
not resolve current authority by app ID. `AppBackend`, `ConsumerScope`, runtime
contexts and queued backend calls preserve the same capability. A retained V8
handle or old consumer cannot start a fresh mutation by borrowing a replacement's
policy. An app selector in a signal capability likewise cannot create a binding;
the trusted ingress host selects an existing authorized handle.

Keep policy revision and content high water across local binding replacement and
revocation. Installing a lower source revision fails; changed policy under an equal
revision conflicts. Equal revision with identical values is valid for a newly
authorized binding even when its assignment differs. This permits unchanged policy
to serve a replacement without allowing the retired handle to refresh itself.
After a process restart, the host must obtain fresh trusted authority; customer
state and previously serialized metadata cannot restore a binding.

Local configuration uses the same capability lifecycle with an explicitly
nonexpiring configuration snapshot. Remote and configured authority are distinct
binding modes. Missing or expired remote metadata never falls back to configured
defaults. Refreshing the current binding is different from replacing it, so normal
refresh does not force healthy handles to acquire a new local identity.

### Ordered refresh and captured operations

Create a refresh ticket before starting metadata I/O. It captures the current
binding generation and expected transport association. Beginning a newer refresh
supersedes older tickets. Installation atomically verifies the current binding,
ticket, expected association, policy revision/content and remaining validity, then
consumes the ticket. Ticket comparison and snapshot installation occur under the
same host-state synchronization. Retrying transport uses a new ticket and assertion;
it cannot apply a response through a ticket already consumed or superseded.

```text
Host binding                 Metadata exchange                 Journal handle
     |                              |                                |
     |-- capture refresh ticket --->|                                |
     |-- replace / revoke binding   |                                |
     |                              |                                |
     |<-- delayed valid response ---|                                |
     |    reject retired ticket     |                                |
     |                              |                 old mutation --|
     |<----------------------------- check retained binding ---------|
     |------------------------------ unavailable; no new authority -->|
```

Within the current binding, a fresh response with unchanged policy may extend the
deadline for subsequent operations. A newer accepted response may also shorten it.
An older response must not reverse that shortening, restore a prior policy or undo
revocation. Serial refresh is a valid host optimization, but cancellation and
replacement still require the ticket fence because an outstanding response can
outlive its initiating loop.

Expiry alone does not replace a binding. A fresh authenticated response may admit
new operations under the same still-current binding after an earlier snapshot
expired. Explicit revocation requires a new host-authorized binding; refresh cannot
undo it. Neither case revives an operation captured under expired authority.

Capture binding generation, source revision and original monotonic deadline before
journal waits. `PolicyAuthority` validates the retained binding and revision, its
original deadline and the current snapshot's validity after waits and before fresh
mutation commits. A later refresh cannot extend an operation already in progress;
replacement, revocation or a newer policy invalidates its captured authority.
Accepted shortening must remain a monotonic cap on existing operations, even if
a later refresh extends the current snapshot before those operations next poll.
The host must retain that cap or invalidate affected captures; checking only the
latest snapshot would lose an intervening fence. Host invalidation closes new
admission and signals active work to cancel; execution still must join before its
slot is reused. Renewing policy cannot resurrect an expired or cancelled execution,
and never changes its independent hard execution deadline.

Structural app locking and receipt readback are separate from live mutation
authorization. An exact, already committed app/request or job receipt may be read
through the narrowly scoped replay path after its grant expires. It cannot install
policy, advance a frontier, publish new effects or capture a replacement binding.
Every operation without a matching receipt must establish current bound authority
before mutation. Once-live invalidation is retryable `Unavailable`; it must not
become a durable customer `Denied` outcome. Explicit policy values that disable an
operation remain distinct from missing or expired authority.

An operation's deadline includes database waits and commit acknowledgement. A
timeout before terminal dispatch requires rollback; a dispatched COMMIT may still
finish and must be resolved through its immutable receipt. Policy replacement
does not retract an already dispatched database commit. Publication and delivery
ACK use their own current metadata authority; reading a creator receipt alone
does not authorize a manager write.

### Authoritative source and authenticated exchange

Control owns the complete effective policy, including app lifecycle, entitlement,
operator switches and applicable limits. Its policy revision must describe a
consistent observation of all contributing values. Changes that affect policy
advance that revision atomically with their authoritative state, or use an
equivalent durable revisioned projection whose original source validity is explicit.
A content hash without ordered source authority cannot distinguish a delayed
observation from a new desired state.

The manager obtains this policy through a trusted provider in the platform zone.
The provider may read Control-owned storage or consume authenticated Control
metadata; it never reads a creator journal. A provider grant contains the exact
app, immutable source revision and values, and a finite original validity bound.
Cached values retain that bound. Repeated worker requests, manager restart or
rereading an unchanged projection cannot refresh source authority. Only a new
authoritative source observation may issue a new validity bound. Unknown source
freshness, inconsistent revisions and unavailable source storage fail closed with
retryable infrastructure failure.

`zeroship_workflow_manager::policy::PolicySource` expresses this trusted provider
contract. `PolicyObservation` validates the raw policy, retains its original
monotonic deadline and carries an opaque observation identity. Cached reads clone
the retained observation. Equal values, revision and deadline do not make a new
observation identical to an invalidated predecessor. The provider's nonblocking
revalidation must reject that predecessor permanently across shortening,
revocation and restoration. `PolicyGrant` retains the source through response
construction and cannot serialize after source authority is lost.

`policy::control::ControlPolicyStore` implements the direct Control-storage
provider through native ORM. The server binds its platform credential to the
`zeroship` schema and verifies source columns before serving. It reads no creator
database. Its durable inputs are:

| Input | Authority and contributing writers |
| --- | --- |
| `apps.plan_id`, `workflows_enabled`, `archived_at` | Registry app lifecycle and plan changes, billing plan-change transactions, and operator enablement. The database constraint makes deletion imply archive. |
| `plans.workflow_policy_json` | A complete canonical `AppPolicy`, written by the native operator `set_plan_policy` API or equivalent operator provisioning. Missing or malformed JSON is unavailable, including for a disabled plan. |
| `plans.workflows_allowed`, `archived` | Operator entitlement and catalog archival. Pricing updates omit the workflow policy column; startup seeding preserves archival and workflow authority. |
| `workflow_rollout_config` | The required global row contains dispatch/ingress switches and positive `source_validity_ms`. The native `set_rollout` operation writes these together. Missing settings never select defaults. |

The publication transaction explicitly requests read-committed isolation. It
first performs an ID-only upsert on `workflow_policy_ledger`, creating an
unpublished row or locking the existing publication without resetting it. Only
after that wait does a relational statement read the selected app, plan and global
settings together. It validates the complete policy, masks enablement and operator
switches, and publishes changed policy or source validity under an advanced
revision. Unchanged values preserve the revision. The ledger retains its app ID
as the sole primary key and has no cascading app deletion. Its unpublished state
cannot issue authority. The host refuses unsupported isolation instead of silently
substituting a different transaction contract.

Source validity begins before acquisition and ends at that original instant plus
the observed `source_validity_ms`; publication and commit waits consume it. Only
successful settlement produces a `PolicyObservation`. A ledger row alone is not
a renewable source: every refresh rereads the contributing authoritative inputs.
The ledger orders observations, not every intermediate writer transition. An
unobserved disable followed by restore need not change its revision. Writer
acknowledgement therefore promises bounded convergence, never immediate
revocation or execution quiescence. A stricter acknowledgement would require a
separate writer barrier and worker evidence.

`ControlPolicies` caches exact observations until their original expiry. A
per-app refresh reservation prevents competing requests from independently
refreshing the same entry; callers arriving during that read receive a retryable
infrastructure failure. Cancellation, timeout, source failure, invalidation or
capacity eviction removes the reservation. Opaque entry identity prevents a late
completion or its cleanup from replacing a later entry. Revalidation accepts
only the current exact observation and performs no I/O. Cache capacity is bounded
by `workflow.policy_cache_entries`; evicting a captured observation refuses an
unfinished grant, while already serialized worker leases retain their deadline.
The production worker assignment and refresh loop still needs composition.

The worker requests a lease using `AssignedScope`: app ID and assignment revision.
The server authenticates the enrolled instance before buffering the body. Worker
identity and signing-key thumbprint come from the verified assertion context, not
request selectors. The closed `zeroship_core::workflow_policy::PolicyLease`
response binds:

```text
appId
workerId + signingKeyId
assignmentRevision
policyRevision + closed policy values
remainingMs
```

`signingKeyId` is the thumbprint of the exact enrolled key used for verification.
It is metadata, not a credential. The client compares it with the key that signed
the request, and verifies app, worker and assignment revision before returning a
private validated lease handle. Policy values are a closed, validated native
contract shared through `zeroship-core`; the metadata client and manager do not
depend on the customer engine. No database address, credential, customer input or
history is admitted into the envelope. Existing TLS, endpoint/audience assertions,
replay protection, bounded bodies and closed failures apply.

Under the manager's app-before-worker lock order, verify placement and enrollment
before requesting source metadata. Release those locks for source I/O, then take
them again to verify current placement and source authority. Revalidate the
originally authenticated enrolled key after waits and before issuing the grant.
A replaced key is not equivalent to another
active key on the same instance. Cap authority by the original verified source
deadline, assignment/registration validity and configured policy-lease ceiling.
Later checks can shorten the issuing attempt's bound. Requesting policy never
renews registration or placement, and unavailable enrollment is not a permanent
policy refusal.

Convert to `remainingMs` only after charging manager clock queries, lock waits,
source I/O and transaction settlement. Use the same conservative clock-resolution
and monotonic conversion rules as delivery grants. The client anchors its deadline
before sending the HTTP request, validates a positive representable remaining
duration and rejects a reply already exhausted by the exchange. It does not compare
Control or manager wall-clock timestamps with worker or creator database time.
Cloning a lease preserves its deadline. Installing it consumes the original
request's refresh ticket; client validation alone cannot bind it to a replacement.

The native `AssignedPolicies` helper supplies that installation boundary. It owns
an immutable client and `AssignedScope`; construction explicitly allocates a new
binding generation, and cloning retains it. `refresh` reserves the ticket before
transport and installs the exact client deadline without rebasing. A failed old
request cannot revoke a newer successful refresh. Failed exchanges retain only
the previous snapshot's original authority. The production host must keep the
helper while signer and assignment are unchanged, replace it when either changes,
and drive its bounded refresh lifecycle alongside assignment discovery.

### Archive acknowledgement and verification

Calendar disable acknowledges the manager's durable calendar fence. Policy
publication acknowledges a desired source revision. Neither result proves that
workers observed revocation, stopped creator mutations or joined executions.
Control must not report execution quiescence from either acknowledgement.

With finite leases, partitioned workers eventually lose permission for fresh
mutations, provided the manager cannot renew from stale source authority. Expiry
is an eventual admission fence, not an acknowledgement from an unreachable worker
or proof that a dispatched commit rolled back. An explicit quiescence acknowledgement
requires a separate protocol that accounts for affected bindings, stops renewal,
joins their active work and resolves uncertain outcomes. That protocol remains
open; archive responses must distinguish desired state, manager acknowledgement
and any later verified quiescence result.

Required native regressions cover retired `AppWorkflows` and `AppBackend` handles
starting fresh calls, equal-policy-revision replacement, delayed refresh tickets,
shortened grants, revocation during lock waits, and exact receipt replay without
new authority. Transport tests cover app/worker/key/assignment substitution, key
replacement during issuance, stale source renewal, delayed responses and unrelated
absolute clocks. Host tests deny workers Control DB access and deny platform
processes creator access. Native binding tests cover the local lifecycle; transport
and production host verification remain required. Local policy tests alone do not
prove remote authorization or private-zone deployment.

## Storage inventory and schema ownership

### Platform metadata

The physical manager schema is generated by
[`workflow-manager/schema/schema.ts`](../../crates/zeroship-workflow-manager/schema/schema.ts).
Rust models declare their ORM metadata natively in
`crates/zeroship-workflow-manager/src/models/schema_definition.rs`; parity tests
compare those declarations with the migration artifact.
The production namespace is `workflow_manager` in the private Control database.
The server migration consolidates the former `workflow_coordination` namespace
with this queue namespace; it is not a second authoritative placement store.

| Table | Stored authority and purpose |
| --- | --- |
| `workflow_manager.schema_version` | Generated schema fingerprint. Runtime roles read it; provisioning owns changes. |
| `workflow_manager.queue_scopes` | Registered apps and the shared app lock for queue and coordinator operations. |
| `workflow_manager.workers` | Instance liveness, capacity, ready/draining state, the enroller zone registration copied from Control, and serialization of worker-wide admission. |
| `workflow_manager.assignments` | App/worker placement revision, expiry, release tombstone and refusal tombstone. A refused pair is never offered again during that instance's life. |
| `workflow_manager.placement_receipts` | Immutable assignment/release request identity, release reason and recorded result. |
| `workflow_manager.capacity_demands` | Apps with claimable work that free eligible capacity did not absorb, keyed by app and recorded with its zone. The committed input of every replica's target. |
| `workflow_manager.capacity_targets` | One declarative target per execution zone in placement slots, with its revision, provider state, closed refusal, claimed attempt and pacing. Scale-down drains against it; `workflow_manager.workers.state` records the drain. |
| `workflow_manager.management` | Job-linked authorized command, original request provenance, per-run revision, provisional execution barrier and reported closed outcome. |
| `workflow_manager.management_scopes` | Accepted and settled management revisions per app/run; independent of the creator's run existence. |
| `workflow_manager.jobs` | Immutable job specification, checked operation/run/request projections, availability, current attempt, delivery fence and settlement digest/outcome. It also supplies submission and settlement deduplication. |
| `workflow_manager.recovery_scopes` | Trusted activation provenance and revision for durable maintenance responsibility, its ingress epoch and state, the current closing attempt's watermark and Close job, its latest activity and closing pacing. The row outlives retirement and abandonment as the epoch's tombstone. |
| `workflow_manager.recovery_duties` | Independent app/kind deadlines and retained pending reconciliation or collection jobs, linked to their owning scope and queue. |
| `workflow_manager.schedule_deployments` | Immutable allowlisted schedule descriptors and the calendar interpretation for a normal deployment. No business input. |
| `workflow_manager.schedule_activations` | Stable activation job, deployment, app revision and activation instant. Job settlement determines dispatch readiness. |
| `workflow_manager.schedule_disables` | Historical Control disable commands in the shared activation revision sequence. Exact replay cannot change a newer scope state. |
| `workflow_manager.schedule_scopes` | App lifecycle revision, calendar-enabled state and optional selected activation; historical receipts remain independently replayable. |
| `workflow_manager.schedules` | Logical schedule identity across deployments, active descriptor and persisted due/catch-up frontier. |
| `workflow_manager.schedule_occurrences` | Stable occurrence request, run and job identities bound to a schedule revision, instant and activation prerequisite. |
| `zeroship.worker_instances` | Control-owned enrollment, public key and revocation state; distinct from workflow registration. The manager also reads each instance's enroller. |
| `zeroship.execution_zones`, `zeroship.worker_enrollers` | Control-owned zones and deployment-unit enrollers. The manager reads an enroller's frozen zone and its status. |
| `zeroship.app_deploys` | Control-owned immutable deployment metadata and reclamation state. |
| `zeroship.app_deploy_holds` | Control-owned app/deployment/holder generation and retention state. |
| `zeroship.apps`, `zeroship.plans` | Control-owned lifecycle, entitlement and complete workflow policy inputs, and each app's frozen execution zone. The manager receives column-scoped read access. |
| `zeroship.workflow_rollout_config` | Operator dispatch/ingress switches and the finite source-validity bound. |
| `zeroship.workflow_policy_ledger` | Durable per-app ordered policy publication. The manager locks and updates this row, without writing its app or plan inputs. An unpublished row grants nothing. |

Capacity demand lives in `capacity_demands` and `capacity_targets`, beside the
queue it describes. Prefer extending the owning manager models over adding
another store; an in-memory map is not a substitute.

A delayed job's `available_at` is sufficient for a simple timer. Separate timer
rows are warranted only where calendar cursors, cancellation or coalescing need
additional durable state. Do not create a scheduler database alongside the queue.

### Creator journal

The journal is generated by
[`workflow/schema/schema.ts`](../../crates/zeroship-workflow/schema/schema.ts).
Its Rust ORM models use native declarations in
`crates/zeroship-workflow/src/service/models/schema_definition.rs`, with migration
metadata parity checked by tests. Runtime model construction does not read the
generated JSON artifact.
The following names are in the app's resolved creator schema. Every name in this
table has the `__zeroship_workflow_` prefix; none belongs in Control's schema.

| Table suffix | Customer-owned content and target treatment |
| --- | --- |
| `schema_version` | Creator journal schema fingerprint, installed by creator-side provisioning. |
| `app_state` | App serialization, journal counters and the highest ingress epoch a delivered Close fenced. Trusted admission policy remains outside customer SQL. |
| `deploys` | Locally accepted immutable app deployment and availability state. |
| `deployment_holds` | Customer dependency intent and observed hold generation; not the platform hold ledger. |
| `activations` | Immutable readiness per manager activation job, app revision and deployment; committed with the logical job receipt. |
| `activation_scopes` | Highest manager activation revision selected for new work. Older readiness remains independently replayable. |
| `runs` | Run identity, lifecycle state, current generation, relationships and logical frontier revision, independent of the task lease epoch. |
| `generations` | Pinned deployment, input, output/error references and generation lifecycle. |
| `steps` | Replay history, checkpoints and compensation state. |
| `tasks` | Customer execution claims, frontier and lease fences, job/delivery identity and exact task completion receipts. |
| `waits` | Recorded sleep, signal and child waits and their execution scope. |
| `topics`, `broadcasts`, `signals`, `subscriptions` | Customer event bodies, accepted/completed topic ordering, app-scoped signal delivery order, targets, subscription state and fanout cursors. |
| `fanout_pages` | Exact delivered broadcast page, committed cursor transition, semantic outcome and successor specifications linked to the retained job receipt and publication. |
| `propagations` | Dependency propagation obligations: kind, source run and generation, cursor, next page revision and finished state. An unfinished cascade obligation fences its source generation's cascading children. |
| `propagation_pages` | Exact delivered propagation page, cursor transition, closed result and successor specifications linked to the retained job receipt and publication. |
| `requests` | Durable app-operation request identity, body digest and original result. Age alone cannot retire an accepted request. |
| `management_receipts` | Exact delivered job identity, requested run, management revision and durable lifecycle outcome, independently scoped from app requests. |
| `management_scopes` | Last applied management revision per app/requested run, linked to retained command history without requiring that the run exists. |
| `schedules`, `occurrences` | Existing customer schedule definitions and accepted occurrences. Calendar discovery moves to the manager; customer acceptance, overlap state and input references remain customer-side. |
| `payloads`, `payload_refs` | Prepared upload metadata, ownership, integrity and committed references. |
| `outbox` | Customer events and their payloads; distinct from manager queue metadata. |
| `job_publications` | Closed immutable Advance, Fanout or Propagate specifications, validated operation-specific projections and manager confirmation time. Pending Advance records retain deployment dependencies; publications survive history removal. |
| `job_receipts` | Immutable logical job specification and committed semantic outcome, retained independently of run history and delivery attempts. App-wide reconciliation records a selected page and its durable attempt offset; its run identity is absent. |
| `reconciliation_scans` | App-owned scan revision, publication/hold phase, ordering cursor and captured upper boundary. It schedules no work and grants no ingress authority. |
| `collection_scans` | App-owned collection revision, expiry cutoff, ordering cursor and captured upper payload identity. |
| `collection_pages` | Immutable bounded payload identity plan and reserved item offset, scoped to its logical job receipt. |

Publication intents now use dedicated journal records. The scoped unique index
binds run, generation, frontier revision and due time; `id` remains the sole
primary key. A transition that advances an idle run invalidates its older
frontier without retargeting already committed jobs. Task claim, heartbeat and
release do not advance that logical revision. A new generation starts a fresh
frontier. Advance-job acceptance now binds creator claims to the logical job,
delivery attempt and assignment revision. Superseded frontiers produce a durable
rejection; a committed job replays its original semantic outcome across delivery
attempts. Receipt records have no run/history foreign key and survive collection.
There is no manager reader of these tables. A reconciliation job reads them
through an app-bound worker, never through a platform connection.

Creator object storage holds large inputs, signals, results and prepared uploads.
The normal bundle store holds `.zship` manifests and modules. Neither is a place
to copy customer data for scheduling. An opaque payload ID in a job is not a
signed download URL, database URL, object key or credential.

### Models, migrations and transaction domains

Every table has `id` as its sole primary key. Composite domain identities use
unique indexes: for example app/worker, app/request and app/run/generation.
Scoped foreign keys retain the app identity. IDs and cursors preserve bytewise
ordering. New rows receive typed storage IDs; upserts preserve existing storage
IDs, revision tombstones and immutable receipts.

An app scope uses its `AppId` directly as `queue_scopes.id`; a worker registration
uses its `WorkerId` as `workers.id`. Dependent `app_id` and `worker_id` foreign
keys reference those primary keys. Do not duplicate the same identity in another
uniquely indexed parent column: competing first-registration upserts must share
the same conflict arbiter. Records with a composite domain identity, such as an
assignment, retain their own storage ID and composite unique index.

The canonical migration DSL generates SQL and ORM descriptors. Both platform
and creator persistence use native `zeroship-data-orm` models and collection
operations, including `schema!`, `FromRow`, `Insertable` and `Changeset` where
appropriate. PostgreSQL/SQLite selection belongs to the ORM. Do not restore
separate workflow backends or add workflow-specific ORM access exceptions.

Provisioning applies each schema in its own authorized zone before execution.
Claiming a job performs ordinary DML and creates no schemas, roles or tables.
Platform startup still verifies restricted role membership, required grants,
read-only fingerprints and lack of customer authority. Connection authority is
a binding mode, not proof that the supplied role is appropriately restricted.

The manager's coordinator and queue use the same physical namespace and callback
`Database` transaction. Internal placement checks receive that existing handle.
Opening another coordinator transaction from a queue callback could wait on its
own app lock and would separate authorization from mutation. Preserve
app-lock-before-worker-lock ordering, and validate affected rows on fenced writes.
Capacity counts cover the full matching set. Recovery pagination filters owners
before ending a result page, so an owned page cannot conceal later missing owners.

| Transaction | Operations that commit together |
| --- | --- |
| Manager placement | Scope registration, capacity admission, assignment revision and placement receipt. |
| Manager submission | Stable job specification, submission deduplication and owning scheduling/recovery metadata. |
| Manager calendar turn | Selected occurrences/jobs and the schedule cursor/revision that produced them. |
| Manager settlement | Exact delivery fence, immutable outcome, successor jobs and associated scheduling/barrier changes. |
| Creator acceptance | Input/event reference, run or signal acceptance, request result and publication intent. |
| Creator execution | Frontier transition, history, waits/children, promoted payload references, committed job outcome and successor intents. |
| Creator management | Lifecycle transition or explicit refusal, request digest, durable outcome and resulting publication intents. |
| Control retention | Deployment reclamation fence and hold mutation under the same deployment lock. |

There is no distributed transaction across these owners. Durable intents,
idempotency and reconciliation connect their commits. Object uploads likewise
cannot join a database transaction; reference promotion supplies that boundary.

Database clocks decide local expiry and due work. The manager uses its own
independent clock connection to the same Control database, sampled after lock
waits; it does not query through the transaction's held pool connection. Creator
fences use the creator database and trusted host policy. Raw clock reads and
host privilege inspection are narrow public-API gaps, not alternate SQL backends.

A monotonic caller budget includes lock acquisition, authority checks and waiting
for commit. Clock conversion includes elapsed clock-query and transport time;
later checks can shorten an attempt's budget but cannot restore elapsed time.
The deadline expires execution and new mutation admission. Cancellation before
terminal dispatch must roll back. A timeout after COMMIT dispatch is ambiguous:
settlement may finish, and the caller must read its durable receipt. It is never
proof that the transaction rolled back.

Delivery authority crosses zones as `DeliveryLease.remainingMs`. The manager
captures a monotonic deadline when issuing the database lease, anchored before
the database clock query and reduced by the clock sample's resolution. Later
samples can only shorten that captured deadline.
After commit, `DeliveryGrant::lease` subtracts all intervening elapsed time and
refuses an exhausted grant. The client captures its own monotonic instant before
starting the request and adds the returned remaining duration to that instant,
conservatively charging the entire exchange. It never compares the manager's
`Delivery.deadline` with a worker wall clock or creator database clock.

A heartbeat commits under the previously stored delivery lease and the original
transaction budget. Its successful new grant has its own deadline; capping that
grant by the old transaction budget would prevent renewal. The client still
rejects the reply if its previously confirmed local grant expired while waiting.
Renewal also cannot restore a cancelled execution or extend the executor's
original hard deadline. Creator frontier fencing remains required independently
of these delivery leases.

The customer engine consumes the trusted Rust `JobLease` contract, implemented
by the native `DeliveryGrant` and authenticated client's `LeasedJob`. Native
grant identity is private; a mutable wire `Delivery` alone is not execution
authority. This trait is a trusted host composition seam, not a cryptographic
boundary against arbitrary Rust implementations. Creator acceptance captures
the grant and policy before opening its transaction. It translates remaining
time into the creator clock conservatively and returns a private `DeliveredTask`
capped by the actual task deadline. Full-operation timeouts bound database waits
and commit acknowledgement. Expired calls can recover committed receipts through
bounded reads, while fresh mutations still require live captured authority.

## Durable job protocol

### Identities and allowed metadata

The current closed contract lives in
[`workflow_jobs.rs`](../../crates/zeroship-core/src/workflow_jobs.rs):
`SubmitJob`, `JobSpec`, `JobOperation`, `Delivery`, `DeliveryLease`, `Settlement`
and `SettlementReceipt`.
It defines activation, advance, cron, management, fanout, propagation,
reconciliation and collection operations.
Executable operations carry their deployment prerequisite inside the operation;
journal-only commands, fanout, propagation, reconciliation and collection carry
none. Management names
its lifecycle revision and a closed resolved command, including the immutable
target for a latest restart. Shared restart validation derives the effective
policy before that target is selected.
`JobOutcome` is a closed tagged object: `Completed`, `Waiting`, `Rejected`, or
`Management` containing the closed lifecycle outcome. Generic results belong to
non-management jobs; a management job requires its lifecycle result. Unknown
fields and the former string shape are refused. Successor jobs carry further
availability. Activation names its platform revision; cron names the
logical schedule identity and bundle declaration name alongside the occurrence's
request, run, revision and instant. The input-free registration and activation
envelopes live in `workflow_schedules.rs`. Creator activation and cron acceptance
are implemented natively; production host composition, event delivery and
paginated maintenance still need integration.

Keep the following identities distinct:

| Identity | Retry rule |
| --- | --- |
| App request | Reuse for retried start/signal/management acceptance; changed body conflicts. |
| Logical job | Reuse across publication retries, successor submission and delivery attempts. |
| Assignment revision | Changes when placement changes; registration renewal cannot restore an old revision. |
| Host policy binding/source revision | Binding replacement retires existing handles; policy refresh preserves source revision only for identical values and cannot reinstall a retired binding. |
| Delivery attempt | Changes on redelivery; a stale attempt cannot settle the new attempt. |
| Run generation/frontier | Fences customer history and identifies the next admissible transition. |
| Wait/event/occurrence | Identifies the semantic trigger even if several delivery attempts observe it. |
| Hold generation | Fences retention acquisition/release independently of task and assignment leases. |

Bodies and nested variants are closed and bounded. The manager receives only
allowlisted scheduling metadata: app, job, deployment, run/reference identity,
revision, operation, deadline and closed outcome. Workflow export identities,
cron expressions, timezone and declared scheduling policy can be allowlisted
deployment metadata. Arbitrary inputs, result JSON, signal bodies, stack traces,
customer connection details and free-form error messages never cross this API.
Detailed run reads terminate at an authorized worker through normal app routing.

An identifier parsed from a body grants no authority. Host authentication selects
the worker and allowable app; all referenced jobs, deployments and successors
must agree with that scope. Unknown or cross-app references must not become a
probe into another tenant's state through differing payloads or diagnostics.

### Authenticated delivery boundary

The server and typed client expose the following worker requests through the
bounded, authenticated metadata transport. Worker startup and the production
consumer still need to use them at cutover.

| Operation | Request metadata | Authority and reply checks |
| --- | --- | --- |
| `POST /v1/jobs/submit` | Assigned app/revision and immutable job specification. | Current enrolled signer and stored placement; exact specification in the receipt. |
| `POST /v1/jobs/claim` | Assigned app/revision. | Current enrolled signer and stored placement; delivered app, worker and assignment revision must match. |
| `POST /v1/jobs/heartbeat` | Delivery identity. | Stored attempt and live lease; reply preserves the immutable job, worker, assignment revision and attempt. |
| `POST /v1/jobs/settle` | Delivery, closed outcome and immutable successors. | Active delivery authority for writes, or original-worker enrollment for an exact stored receipt; reply matches app, job, attempt and outcome. |

Derive worker identity from the verified instance signer. An echoed worker must
match it. A request cannot supply its own assignment expiry. Resolve placement
from manager records, and perform revalidation inside the queue transaction after
the app lock and before commit. The enrollment check retains the originally
verified key identity; a replacement key under the same worker ID must not keep
an earlier key's pending request authorized. Revalidation queries registry state
without consuming the signed assertion's replay token again.

App placement grants scoped execution and publication, not manager scheduling
authority. Worker submission and successors must reject manager-origin cron and
management operations unless they resolve to matching authoritative manager
records. Normal manager scheduling uses the trusted native submission path.
Operation provenance is additional to the queue's app and identity checks.

An exact settled receipt may outlive its original placement. Do not reject it in
a current-placement preflight before the queue can select its receipt-replay
branch. That branch checks current enrollment of the original worker, compares
the stored complete settlement identity and admits no new successor writes.
Changing the job, attempt, outcome or successor contents is a conflict.

Control scheduling uses the same bounded authenticated transport through
`POST /v1/schedules/register` and `POST /v1/schedules/activate`. The server checks
the exact `svc/control` issuer before buffering either request. Worker placement
and instance credentials grant neither operation. Registration returns the
accepted typed declaration, which the client compares in full; native storage
canonicalizes its ordering independently. Activation returns the stable job,
whose app, deployment, operation and revision must match the original request.
These routes require no enrolled or assigned worker. Control's lifecycle
publisher is their production caller, delivering the durable intents described
under [normal deployment publication](#normal-deployment-publication).

### Delivery, execution and settlement

```text
Manager job:

  durable ready -- available_at reached + authorized claim --> leased
       ^                                                    /      |
       |                         expiry / recoverable loss /       |
       +-------------------------------------------------+        |
                                                        exact ACK |
                                                                  v
                                                               settled
                                                                  |
                                           immutable replay <-----+

Creator transition:

  delivered job -> existing committed receipt? -> report same outcome
                        |
                        no
                        v
              claim expected frontier
                        |
              execute bounded operation
                        |
              commit state + receipt + intents
                        |
              report closed outcome / publish intents
```

```text
Manager/queue                  Worker                       Creator DB
      |                           |                              |
      |<---- authenticated poll --|                              |
      | lock app; verify current placement/enrollment             |
      | persist delivery attempt |                              |
      |---- job + authority ---->|                              |
      |                           |-- receipt/fence transaction ->|
      |                           |<-- prior outcome or claim ----|
      |                           |                              |
      |                           | load pin; execute bounded turn|
      |<---- heartbeat ----------|                              |
      |---- bounded authority -->|                              |
      |                           |-- checkpoint + receipt ------>|
      |                           |   + payload refs + intents    |
      |                           |<-- confirmed commit ----------|
      |<---- outcome + ACK -------|                              |
      | lock app; verify fence; settle + insert successors        |
      |---- settlement receipt -->|                              |
      |                           |-- mark publication confirmed ->|
```

The queue provides at-least-once delivery. It deduplicates immutable submission
and settlement content, including the successor set. A heartbeat can update the
stored deadline; the mutable echoed deadline is not part of immutable delivery
identity. Retrying after a lost heartbeat reply must remain possible.

Renewal cannot revive expired execution. A retry can observe a still-live stored
manager lease, but the worker rejects renewal after its local guard has expired
or been cancelled. A later lease never extends the original execution deadline.
Redelivery requires a fresh attempt and admission, including a check for a
previously committed customer outcome.

The creator journal checks a committed receipt before running app code. A job
that already committed returns that result. Otherwise the worker claims the
expected app/run/generation/frontier, captures trusted policy and pins execution
authority. Competing jobs for the same frontier cannot both advance it. A new
attempt must not reuse another attempt's write authority.

The manager's lease alone cannot stop an old process writing a private creator
DB. Creator transactions check the execution fence and current frontier, and a
replacement serializes with any earlier transaction on that frontier. If an
older COMMIT was already dispatched, recovery reads its result after settlement;
it cannot assume the older execution did nothing merely because delivery expired.
Synchronous JavaScript is interrupted through the execution budget. Quarantine
prevents isolate reuse and initiates cancellation; it does not prove that native
work has stopped. The executor must join native operations through runtime
shutdown before releasing the slot or publishing completion. Rejection of a
JavaScript promise alone is insufficient. Already dispatched COMMIT remains
supervised until its outcome is settled or explicitly uncertain.

A fresh delivery may find a customer outcome committed by an older attempt. It
settles that semantic outcome using its own current delivery fence. The stored
creator outcome must therefore be independent of the old delivery envelope.
Conversely, exact replay of an already settled manager attempt returns its
immutable receipt after placement expiry, but still requires current enrollment
of the original worker. Changed content, another worker or a superseded unsettled
attempt cannot use receipt replay to admit new writes.

### Publication and receipt retention

Successor identities and content are generated once in the creator transaction.
ACK publication and outbox reconciliation use the same IDs and immutable
specifications. They may race; manager submission and settlement share the same
deduplication domain. Neither path substitutes a fresh ID after a lost response.

The current creator delivery API settles the semantic outcome with an empty
successor list. Its committed successor intents use the independent publication
path above. Passing those same persisted specifications in ACKs remains a
consumer integration option; this implementation does not yet capture or confirm
successors through settlement. Delivered reconciliation publishes those records;
production manager dispatch and scope-duty admission are still required to make
eventual publication a production guarantee.

Creator state commits before its ACK. If the manager is unavailable or full,
intents remain pending. Marking publication confirmed happens only after a
manager receipt is validated against app, job and content. A lost confirmation
write merely causes another idempotent publication attempt.

`AppWorkflows::pending_jobs` pages creator-owned advance intents under trusted
app policy. `publish_job` reads and commits locally before calling its
host-bound `JobPublisher`, validates the entire immutable specification, then
confirms it under the app lock in a new creator transaction. Concurrent
publishers may submit the same job; a confirmed intent remains as a durable
receipt. `AssignedPublisher` uses the authenticated worker client and its
current app assignment. These methods do not discover apps or schedule work.
Delivered reconciliation uses the same confirmation path with additional captured
delivery and policy checks around publication, lock waits and commit.

Starts, child/continuation creation, task checkpoints, restart and runnable
lifecycle/signal/dependency wake-ups record their advance intent in the creator
transaction. A checkpoint publication failure rolls back history and its task
receipt together. Pending intents prevent creator deployment-hold release even
if run history has been removed. Confirmed records do not retain that customer
hold by themselves; the manager's queue and retention fences then own delivery
dependencies. The production collector cutover remains required.

App request receipts and delivery receipts have separate identities. Both retain
their deduplication state until an explicit retirement protocol proves that the
relevant submissions and redeliveries are no longer admissible. The creator
`requests` table records creation time without an expiry, and `AppPolicy` offers
no request-receipt TTL. A repeated request returns its original result or rejects
changed content, including after later lifecycle changes. Capability receipt
replay cannot issue fresh authority: the original token keeps its own expiration
and revocation checks. History collection cannot erase the only proof that a job
already ran, and unpublished intents remain pending until confirmed.
Receipt retirement/watermarks are unresolved; arbitrary TTL-based deletion is not
the default design.

### Lifecycle state and authority

These states describe different objects. A settled queue job can leave its run
waiting for a later job; a live worker does not imply a running workflow. Manager
observations of customer lifecycle are reported metadata, never permission to
reconstruct or overwrite creator history.

| Object and owner | State and transition responsibility |
| --- | --- |
| Worker registration, manager | Ready admits placement; draining stops new placement. Expiry removes live eligibility without deleting pending work or recovery responsibility. |
| Assignment, manager | An active revision can renew until expiry or explicit release. Reassignment advances the revision; retained tombstones fence delayed messages. |
| Job, manager | Pending work becomes deliverable at its deadline, receives a leased attempt, and settles against that attempt. Expiry permits redelivery of the same job. |
| Run generation, creator | The journal owns `queued`, `running`, `sleeping`, `waiting`, `paused`, `stalled`, `compensating`, `completed`, `failed` and `cancelled`. The existing lifecycle rules determine legal transitions. |
| Publication intent, creator | A committed pending intent remains recoverable until a matching manager receipt confirms publication. An unknown remote result remains pending. |
| Lifecycle intent, Control | A committed pending intent blocks later revisions of its app and, for activation, retains its deployment until the manager's exact receipt confirms it. |
| Management request, both owners | Manager acceptance creates delivery responsibility. Creator application/refusal produces the durable outcome; manager confirmation reports that outcome and settles the matching barrier. |
| Deployment, both owners | Platform activation selects code for new work; creator activation confirms local prerequisites. A run's existing pin changes only through an explicit lifecycle operation. |

The run-state vocabulary lives in
[`workflow_coordination/lifecycle.rs`](../../crates/zeroship-core/src/workflow_coordination/lifecycle.rs).
The customer lifecycle engine owns its transition matrix. Moving scheduling to
the manager does not give queue handlers a parallel run-state machine. In
particular, job `Completed` means the bounded operation committed; it does not
necessarily mean the workflow returned a terminal result.

Terminal completion and collection are separate. A terminal generation can still
retain history, restartable checkpoints, dependent child results, payloads and
deduplication receipts. Management acceptance likewise does not promise immediate
quiescence: its creator transition and fence determine when cancellation or pause
has actually taken effect.

## Trigger lifecycles

Each trigger follows the commit and delivery rules above. The origin determines
which owner first has durable work and which database can contain its data.

| Trigger | First durable write | Manager receives | Worker commits before ACK |
| --- | --- | --- | --- |
| Explicit start | Creator run, input reference, request receipt and intent. | Stable advance job and run identity. | Next frontier, receipt and successor intents. |
| Cron/interval | Manager occurrence and job under activated schedule revision. | Normal deployment schedule metadata. | Occurrence acceptance and run creation, or a durable overlap refusal. |
| Sleep/retry | Creator checkpoint, wait/retry state and timed successor intent. | Delayed job with expected generation/frontier. | Resume transition or stale-wait no-op receipt. |
| Signal/topic event | Creator event body, target/cursor and intent. | Opaque event or fanout reference. | Consumption/fanout page, affected frontiers and intents. |
| Child/continuation | Creator relationship, child/continuation acceptance and intent. | Stable run/event references. | Child progress or parent notification consumption. |
| Management | Manager authorized command and delivery barrier/job. | Closed command with stable request and provenance. | Lifecycle outcome, durable receipt and resulting intents. |
| Reconciliation/collection | Manager scope obligation or maintenance deadline. | Known app and bounded operation/cursor. | Publication/collection progress and continuation intent. |

### Explicit start and input acceptance

`start(input)` runs through the app-scoped native handle in its ordinary runtime.
Rust code in the creator zone can call that same handle. Platform services do
not import it to write customer data.

```text
App code              Worker / creator DB                 Manager
   |                           |                             |
   |-- start(input, request) ->|                             |
   |                           |-- establish scope duty ---->|
   |                           |<-- durable responsibility ---|
   |                           |                             |
   |                           | prepare input; transaction: |
   |                           | run + request result + intent|
   |                           | COMMIT                      |
   |<-- accepted run handle ---|                             |
   |                           |-- submit same job ID ------->|
   |                           |<-- submission receipt -------|
   |                           | mark intent confirmed        |
   |                           |                             |
   |                           |-- poll -------------------->|
   |                           |<-- advance job --------------|
   |                           | commit bounded turn          |
   |                           |-- outcome + ACK ------------>|
```

Creator commit is durable acceptance. The returned handle means accepted work,
not completion or immediate queue visibility. Request retries return the same
accepted run; a changed input under the same identity conflicts. If input upload
or the creator transaction fails, there is no accepted run. If the response is
lost after commit, the request receipt resolves the uncertainty.

Before accepting a start or signal, the worker must hold a manager-recorded scope
responsibility covering unpublished intents. A scope-registration record without
a recovery deadline and drain protocol is insufficient. This obligation is
established before customer acceptance, so failure between databases can leave
extra reconciliation responsibility but cannot leave accepted work undiscoverable.

If no worker can receive the request, the platform requests authorized capacity
and retries the customer-side ingress through normal routing. The metadata-only
manager cannot durably accept the input on behalf of the creator. A request that
never reached creator storage is not accepted.

### Deployment schedules, activation and cron

Normal deployment publication registers an allowlisted schedule descriptor with
the manager without requiring an app isolate, HTTP request or resident worker.
It contains immutable deployment identity, workflow export, schedule identity and
revision, timezone, timing and declared policy. Static business input remains in
the pinned bundle or creator storage; do not send the existing arbitrary
`ScheduleRegistration.input` field as manager metadata.

```text
Control                  Manager / Control DB              Worker / Creator DB
   |                               |                                |
   |-- register deployment ------->| persist prepared revision        |
   |-- activate revision --------->| fence old scheduling revision    |
   |                               | retain deployment               |
   |                               | queue activation if required    |
   |                               |<------------ poll --------------|
   |                               |---------- activation ---------->|
   |                               |                     verify bundle;
   |                               |                     commit deploy state
   |                               |<--------- activation ACK -------|
   |                               | mark dispatch prerequisite ready|
   |                               |                                |
   |                       calendar selects scheduled instant        |
   |                       transaction: occurrence + job + cursor    |
   |                               |<------------ poll --------------|
   |                               |------------ cron job ---------->|
   |                               |                     accept occurrence;
   |                               |                     resolve local input;
   |                               |                     commit run + intent
   |                               |<--------- outcome + ACK --------|
```

Preparation, activation and dispatch readiness are separate states. Activation
selects the revision allowed to produce new occurrences. Delayed deployment
messages cannot reactivate an older revision. Creator-side activation verifies
the retained ordinary bundle and initializes required local deployment state;
it is a bounded job, not a worker calendar loop. Until required activation is
confirmed, due occurrences can be durable but are ineligible for execution.
Failed activation never silently selects another deployment.

Creator activation records readiness independently from the highest revision
selected for new work. If an older activation arrives after a newer one, it
still verifies and retains its own deployment and completes its own readiness
receipt. It cannot replace the current deployment. Rejecting that older job only
because of its revision would strand occurrences the manager already accepted.
Resolve its deployment ID through the host's authenticated ordinary deployment
catalog interface, then verify the retained bundle hash. The latest deployment
and creator-editable deployment metadata cannot substitute for that identity.
Artifact and hold I/O consume the captured job lease; commits recheck both
delivery authority and the current creator admission policy.

Occurrence identity binds `(app, schedule, schedule revision, scheduled instant)`.
The manager persists any generated job/run IDs with that identity before delivery.
Replicas advancing a schedule lock its state and commit the occurrence/job with
the cursor. They cannot mint unrelated jobs for a retry. Already created jobs
keep their selected revision and deployment even when a newer schedule activates.
Creator acceptance binds the authenticated manager schedule ID to its logical
name, but resolves the declaration and static input from the job's immutable
deployment registration. A mutable current schedule row cannot supply historical
input. Under the app lock, cron acceptance records its occurrence and exact
manager-selected run identity, creates the Advance publication intent, and
finishes its retained receipt atomically. An overlap skip is also retained;
capacity or unavailable prerequisites remain retryable. Cron acceptance does
not execute the workflow body.

Keep the existing declared policy vocabulary while moving its evaluation:

| Policy | Required meaning |
| --- | --- |
| `allow` | Occurrences may create overlapping runs, subject to ordinary app admission limits. |
| `skipIfRunning` | Customer acceptance checks the logical schedule's nonterminal runs under the app lock and records a skipped occurrence if it overlaps. Redelivery cannot retry that skip as a new run. |
| `skip` catch-up | Preserve the existing current-due-occurrence behavior, then advance beyond the captured evaluation boundary rather than replaying the intervening backlog. |
| `backfill { max }` | Consider overdue instants from the stored cursor in order, bounded by the declared policy and host ceiling; after exhausting that catch-up allowance, advance beyond the captured evaluation boundary. A processing-page limit must not masquerade as a semantic skip. |
| Interval anchor | Retain the declared epoch or deployment anchor; restarting a process does not reset the schedule. |

The shared calendar resolves an ambiguous local time to its earlier instant
and a nonexistent local time to the next valid local time. Preserve this explicit
behavior during extraction unless the schedule contract is deliberately changed.
Use IANA timezones and stored UTC instants. Nominal times that resolve to the same
instant cannot create duplicate occurrence identities. A timezone or calendar
interpretation change requires an explicit revision policy; persisted occurrences
are never recomputed into different jobs. Prepared manager metadata captures
`zeroship-workflow-calendar::interpretation()`, which includes the calendar
semantics identity and timezone database version. A changed interpretation fences
new activation and frontier extension for that prepared deployment. The platform
must prepare and activate a new normal deployment under the new interpretation;
already persisted occurrences keep their original instant and job. Retrying an
old activation receipt never changes current scheduling or its interpretation.

The lifecycle cutover uses the same Control-owned app revision for activation,
disable and restore. A disable command records a durable receipt and advances
the manager's scope fence even before its first activation. An exact historical
disable replay returns its receipt without undoing a newer restore; activation
and disable cannot share a revision. An unknown older request cannot reopen
scheduling. Disabled scopes are excluded before due-page limits, and calendar
publication rechecks the scope inside its transaction.

Disable preserves calendar cursors, interval anchors, frozen catch-up state,
accepted occurrences and recovery responsibility. A frozen frontier publishes
nothing, so it retains no code: once the disabled deployment's accepted jobs
settle, the [queue hold release policy](#queue-hold-release-policy) releases its
queue hold. Restoring the same immutable deployment publishes a fresh activation,
which acquires the queue hold again before its transaction resumes the retained
calendar progress. Restoring a different staged deployment uses normal
schedule replacement. Historical jobs continue to depend on their original
activation, rather than the new readiness receipt.

Control must commit each desired lifecycle change and its immutable publication
intent together. Its response distinguishes pending synchronization from manager
acknowledgement. Calendar acknowledgement proves that subsequent manager turns
observe the disable fence; it does not establish creator admission policy or
quiesce an executing task. The legacy archive promise based on a shared Control
database lock cannot survive private-zone cutover unchanged. Worker policy
fencing and its acknowledgement require their own authority contract. Calendar
disable continues to permit accepted Advance and reconciliation jobs.

The native manager persists a catch-up boundary and remaining allowance across
processing pages. New observations apply the declared allowance and host ceiling;
an existing observation retains its committed remaining allowance. Restart or a
different processing-page size cannot turn a page boundary into a semantic skip.
Queue readiness joins each occurrence to its own activation job before limiting
candidates. Only a completed activation releases those occurrences; a blocked
earlier cron job cannot hide later activation or recovery work.

The manager does not infer overlap from stale completion metadata. Customer
acceptance serializes occurrence deduplication, overlap/admission, run creation
and input references. Capacity or infrastructure failure remains retryable; it
must not be persisted as an overlap skip. No arbitrary cron input crosses zones.

Removing a schedule from a newer activated deployment fences future generation.
Already queued occurrences remain accepted manager work under their original
deployment and activation prerequisite; removal does not silently withdraw them.
Accepted runs retain their normal lifecycle. Queue-owned deployment retention
starts before dispatch, not after the worker first opens its journal.

### Normal deployment publication

A normal deploy is a command with a stable identity. Control accepts it once,
commits the app's desired lifecycle and its publication intent together, and
delivers that intent to the manager afterwards. No HTTP call to the manager is
the only record of a lifecycle change.

```text
Client               Control / Control DB                    Manager
  |                          |                                   |
  |-- deploy (command id) -->| authorize app; hash upload         |
  |                          | receipt? -> replay original result|
  |                          | ingest blobs; project schedules   |
  |                          | transaction: app lock, receipt,   |
  |                          |   admission, deployment, pointer, |
  |                          |   revision + intent               |
  |<-- acceptance result ----| COMMIT                            |
  |                          |                                   |
  |                  publisher: lowest pending revision per app  |
  |                          |-- register deployment ----------->|
  |                          |-- activate or disable revision -->| queue hold, then
  |                          |<-- exact receipt -----------------| commit activation
  |                          | transaction: acknowledge intent   |
```

**Command identity.** A deploy request carries a canonical deploy command id
(`dcm_` typed id) in exactly one `Idempotency-Key` header. Control refuses a
missing, repeated or malformed key before reading the body. The client mints
the id once per logical deploy; its transport retries reuse the id with the
bytes it captured. A later invocation is a new command unless it explicitly
resumes a surfaced id. The artifact hash is never the command identity: a
redeploy of the same artifact, including an intentional rollback, is a new
command and a new activation.

**Receipt.** Control hashes the bounded upload it actually consumed. The
immutable receipt binds the command id to the app, the authenticated actor, the
operation, the normalized content type and that archive digest, and stores the
selected deployment id and hash with the acceptance result, including the blob
counters of the first acceptance. Authorization and app scope run before any
receipt lookup, so a deleted or unauthorized app is refused without disclosing
a receipt. An exact retry returns the stored result without ingest, admission,
retargeting or a new revision. The same id with another digest, actor, app or
content type is a conflict that reveals nothing about the stored receipt.
Refusals by ingest, schedule projection or schema admission create no receipt,
so a retry after the creator fixes the cause is evaluated afresh. Receipts
outlive bundle retention. Erasing the actor clears the receipt's attribution; a
later retry under that id conflicts. Receipt retirement is not defined.

**Catalog transaction.** One native ORM transaction on the Control database
performs every write through its callback database: lock the app row, treating
a deleted app as absent; recheck the receipt; apply the existing schema
admission against the newest applied descriptor; acquire or insert the
deployment row after the app row, refusing reclaiming or deleted storage;
update the app pointer and manifest; for an active app, allocate the next
lifecycle revision and insert its activation intent; insert the receipt. Any
error, including a workflow projection or domain refusal raised inside the
callback, rolls all of it back. Manager and blob I/O stay outside the
transaction.

**Revisions and intents.** `zeroship.apps.lifecycle_revision` is the app's
revision high-water mark, shared by activation and disable and never reused.
Each transition inserts one intent `(app, revision, action)`. Activation
carries the deployment id and the exact `RegisterSchedules` projection;
disable carries neither. The projection is read from the verified manifest by
the creator engine's declaration parser and checked against the manager's
schedule limits, so static input and unknown fields stay in the artifact and a
projection the manager would refuse fails the deploy before acceptance. An app
without schedules still publishes an activation with an empty list: replacement
fences the previous calendar and recovery follows the new deployment.

| Transition | Catalog effect |
| --- | --- |
| Deploy, active app | Pointer update and activation intent at the next revision. |
| Deploy, archived app | Staged pointer update only; no revision and no intent. |
| Archive | Archive marker and disable intent at the next revision; archiving an archived app changes nothing. |
| Restore | After the existing migration checks, activation of the staged deployment at the next revision. Restoring an active app, or an app with no staged deployment, adds no intent. |
| Delete | No new intent. Pending intents still publish in order; a deleted app is absent to deploy, restore and command replay. |

Restoring the deployment the manager last selected resumes its retained
calendars; restoring a different staged deployment replaces them. A fresh
redeploy of the active deployment is an intentional activation.

**Publisher.** A Control driver reads a bounded page of pending intents in
`(app, revision)` order. It publishes each app's lowest pending revision and
continues with that app's next revision only after confirming the previous one;
the app's first failure ends its turn. Activation registers the deployment's
projection, which the manager keeps immutable per deployment, then activates
the revision; disable disables the revision. Calls use the exact-Control signed
register, activate and disable routes, outside any database transaction and
under a per-attempt deadline. A fresh transaction records only the receipt the
manager returned for that exact request: the Activate job for the app,
deployment and revision, or the exact disable echo. A lost reply, timeout,
refusal or failed confirmation leaves the intent pending, and the next attempt
resends the same revision, which the manager replays exactly. A conflict never
becomes an acknowledgement, and a changed app pointer never implies one. The
page cursor rotates across apps, so a blocked app cannot starve others, and a
later intent of a blocked app is never sent ahead of an earlier one. Replicas
may publish concurrently: the manager serializes each app scope, an identical
confirmation is idempotent, and a different receipt for an acknowledged intent
is a storage failure.

A disable queued behind an unsent activation is delivered after that
activation. The archived app's execution fence is creator admission policy;
manager acknowledgement of a disable proves only that later calendar turns
observe it.

**Retention.** The collector treats a pending activation intent as a direct
dependency of its deployment, checked under the app lock beside the current and
staged pointers, the newest deployment and both holder classes. The manager
acquires its queue hold before committing activation, so the hold exists before
Control acknowledges; acknowledgement is the same row update that removes the
intent dependency. Disable adds no executable dependency. Once a later
activation or a disable leaves the deployment unselected and its jobs settle,
the manager's [queue hold release policy](#queue-hold-release-policy) releases
the queue hold, and the collector may reclaim the deployment when no other
holder or pointer retains it.

**Clients.** `zeroship deploy` mints one command id per invocation or resumes
one named by `--command-id`, reads the artifact once, retries transport
failures and server errors with the same id and bytes, and prints the id with
the resume command when the outcome remains unknown. `@zeroship/control`
deploys an immutable command value holding the id and a `Blob` snapshot; it
accepts no stream or form body.

**Legacy schedules.** The `zeroship.workflow_schedules` reconciler, its sweep
and its table are deleted; only the manager creates occurrences.

### Sleeps, workflow retries and bounded turns

```text
Worker / Creator DB                          Manager / Control DB
        |                                             |
        | commit checkpoint + wait/retry + intent      |
        |-- ACK or publish delayed successor -------->|
        |<-- durable receipt --------------------------|
        | release execution slot                       |
        |                                             |
        |                               available_at becomes due
        |-- poll ------------------------------------>|
        |<-- job for expected generation/wait ---------|
        | validate wait; commit next turn or stale no-op|
        |-- outcome + ACK ---------------------------->|
```

A bounded turn ends at a replay frontier, durable wait, terminal result or host
budget boundary. The journal records which business retry policy applies and
computes the resulting delay. The manager stores the typed deadline and delivers
the next job; it does not inspect an exception body to choose business behavior.
Retry identity includes the expected generation and frontier so redelivery cannot
increment a workflow retry counter again.

Budget exhaustion cancels the attempt; only explicitly committed checkpoints
survive. It cannot manufacture a workflow failure or a continuation checkpoint
when no customer transition committed.

Transport/worker failure causes delivery retry of the same logical job. An
application failure that the engine intentionally records can produce a new
workflow retry intent. An infrastructure error, expired authority or missing
artifact must not become a catchable business failure that changes history.
The host bounds retries and uses backoff; accepted work remains durable even if
no immediate execution is possible.

A timer binds the app, generation and wait revision. Signal, cancellation,
restart or child completion may obsolete it. A late timer is checked against
customer state and yields an idempotent stale result; it cannot reopen an old
wait. There is no resident isolate sleeping until the deadline.

### Signals and topic fanout

```text
App ingress          Worker / Creator DB                    Manager
     |                         |                               |
     |-- signal / broadcast -->| scope duty already held       |
     |                         | commit body + job intent      |
     |<-- accepted ------------|                               |
     |                         |-- immutable job metadata ---->|
     |                         |<-- submission receipt --------|
     |                         |-- poll ---------------------->|
     |                         |<-- Advance/Fanout job ---------|
     |                         | consume or persist fanout page|
     |                         | + frontiers + continuation    |
     |                         |-- outcome + ACK -------------->|
```

Signal bodies and topic subscription details remain customer data. Direct signals
publish the affected runnable frontier through Advance. Fanout jobs name an opaque
broadcast identity and page revision; recipient cursors stay in the creator DB.
A worker cannot use those references outside the assigned app. Target generation
and wait revision are verified before consumption.

Signal-before-wait is safe because events are durable and wait registration checks
already stored events in the creator transaction. Event and timeout delivery
serialize on the same wait; the winner records the transition and the other sees
a satisfied or obsolete wait. Event delivery order cannot be implemented solely
by arrival order at the manager.

Topic fanout captures its subscription cutoff and commits progress with created
deliveries and publication intents. A retry resumes that cursor without duplicate
semantic delivery. New subscriptions cannot retroactively join an already
captured broadcast. The following delivered-fanout contract defines bounded
continuations and ordering independently of manager arrival order.

### Delivered topic fanout

The native creator delivery path implements this contract. Direct run signals
persist their body and runnable Advance intent in the creator transaction;
they do not require another event job merely to repeat that acceptance.

The closed code-free operation is `Fanout { broadcast_id, revision }`, where
`broadcast_id` is a typed broadcast identity and `revision` identifies the
broadcast's next bounded page. The manager sees no topic, subscription cutoff,
recipient, signal body or database cursor. Its existing scoped submission,
delivery, fairness and settlement protocol applies. Fanout neither acquires an
executable hold nor creates a periodic maintenance duty.

Under the creator app lock, a topic owns monotonically increasing accepted and
completed broadcast sequences. Publishing stores the next sequence, the immutable
subscription cutoff and the first stable publication intent together. The
broadcast owns its current page revision and subscription cursor. App/topic
sequence and app/broadcast/page revision have scoped uniqueness; every table
retains its sole required `id` primary key.

The publication journal retains a closed immutable JobSpec plus explicit,
validated optional projections. Advance requires its deployment, run, generation
and frontier projections and no broadcast fields. Fanout requires its broadcast
and page revision and no executable projections. No other operation can be
manufactured by this journal. Reconciliation recovers both through its existing
bounded publication phase. Before releasing creator-held code, retention validates
pending app publication specifications and projections; a damaged null projection
cannot hide an Advance dependency.

A delivered page captures original policy and delivery authority before journal
I/O. Exact committed receipt replay is checked first. Fresh work requires the
matching persisted publication and current page revision, and the broadcast must
be the topic's next unfinished sequence. A later broadcast defers without creating
a receipt, moving a cursor or acknowledging the job. Manager delivery expiry and
fair dispatch retry it; an unavailable predecessor cannot be silently skipped.
The initial page must begin at the initial cursor. Later progress must match the
previous committed Waiting page and its exact successor publication. A completed
topic head must resolve a finished broadcast and its retained Completed receipt;
an advanced or regressed scalar head alone cannot authorize skipping work.

A bounded page selects only eligible subscriptions within the captured cutoff,
verifies their current run generation and wait, and inserts each semantic delivery
once. The creator transaction commits recipient signals, affected Advance intents,
fanout cursor, immutable page receipt and the next Fanout publication together.
The receipt retains those exact successor specifications; the existing creator
outbox and Reconcile path publish them after commit. Settlement sends the page
outcome without coupling its recipient bound to the manager's inline successor
limit. Waiting certifies a durable creator successor, not manager acceptance.
The final page also advances the topic's completed sequence. Waiting denotes a
durable successor page; Completed denotes finished expansion. Historical page
receipts retain their exact job and publication linkage when current topic and
broadcast progress has advanced. No customer transaction spans manager I/O.

Signal consumption uses a required monotonically increasing delivery sequence
allocated under the app lock whenever a direct or topic signal is first
materialized. Replay retains that sequence. The topic predecessor rule makes
same-topic materialization follow broadcast acceptance. Direct signals and
different topics merge by materialization order; this is not a promise of a
global acceptance order. Signal timestamps still govern age and deadline
eligibility, never break ordering ties. Sequence exhaustion rolls back the
enclosing transition.

Native verification must cover reversed job delivery, page/reply replay,
timestamp ties and regression, cutoff changes, stale generation/wait targeting,
app isolation, original authority expiry across lock waits, atomic rollback of
cursor/signals/publications/receipts, counter exhaustion, damaged projections and
progress through separate manager and creator databases. Production ingress
responsibility and trusted host composition remain prerequisites for deleting
the old dispatch paths.

### Child workflows, continuation and compensation

```text
Parent worker / Creator DB                   Manager                 Child job
          |                                    |                        |
          | commit child run + parent wait     |                        |
          | + child scheduling intent          |                        |
          |-- publish/ACK successor ---------->|                        |
          |                                    |<--------- poll --------|
          |                                    |------ child job ------>|
          |                                    |        commit child outcome
          |                                    |        + parent-event intent
          |                                    |<----- outcome + ACK ---|
          |<------- parent notification job ---|                        |
          | commit parent wait consumption     |                        |
          |-- outcome + ACK ------------------>|                        |
```

The parent/child relationship, input and result are creator-side records. Creating
the child and parent wait is atomic with its scheduling intent. Child completion
records a parent notification intent in the same transaction as its result.
The parent job reads that result locally and verifies the parent generation and
wait, so a delayed child cannot advance a restarted parent.

Continuation and compensation use the same pattern: a committed semantic
transition produces stable successors. Partial progress and retry state live in
the journal. Cascading cancellation and large dependency sets yield bounded
continuation jobs rather than an unbounded worker loop. All relationships remain
within the assigned app; cross-app workflow calls are ordinary authenticated app
integration, not a bypass around these journal boundaries.

### Stable continuation identity

The next creator identity change removes eager rewrites of every waiting parent
when a child continues. A stable creator-owned head identifies the current
physical run and generation; generation membership records its history. Pending
parents follow that head through fixed native joins. They never consume an
intermediate continuation marker or recursively traverse run links.

The native model uses the existing generation journal identity as its membership
identity. Every table retains `id` as its sole primary key, with scoped uniqueness
and references:

| Planned model | Identity and relationship |
| --- | --- |
| `continuation_heads` | Journal identity, app, current generation identity and positive head revision. References the generation table. |
| `continuation_members` | Generation identity as `id`, app, head identity and its immutable head revision. References both its generation and its head; app/head/revision is unique. |
| Parent checkpoint | Accepted child member and, after consumption, exact terminal result member. Both are service-owned scoped references. |
| Parent wait | Uses its exact parent checkpoint relationship to resolve the logical child; the duplicated mutable physical child target disappears. |

Heads reference generations rather than members, keeping insertion acyclic.
Create a run and generation before its head and initial member. Continuation
creates the successor generation and member before comparing and advancing the
existing head. Native reads must validate that the head's generation has a member
under that exact head and revision. Missing or inconsistent linkage is a storage
failure, never permission to read another current generation.

Continue-as-new commits source completion, successor creation, head advancement,
promoted input, inherited ownership and the ordinary Advance intent together.
Completing the intermediate source does not notify parents. Only a terminal head
produces the parent notification obligation. The existing bulk checkpoint/wait
retargeting and unused physical continuation links disappear.

Restarting the current head advances the same chain to its new generation.
Restarting a historical continued source creates a fresh head for that source's
new generation; existing waiters remain attached to the original successor
chain. The usual task, descendant and compensation safety checks still apply.
Physical run status continues to describe the requested run. Completed parent
checkpoints never re-resolve a mutable head: they retain the exact result member,
physical result-producing run and copied inline or referenced output. Prefix
replay preserves this provenance; a wait timeout has no child terminal member.

The private stored checkpoint wraps the ordinary step checkpoint with its
accepted and consumed member identities. Reads compare those identities with
the native reference columns before decoding the engine checkpoint, then
validate any consumed result against its immutable terminal generation. A
later generation returning the same output cannot substitute for the original
result. Saving consumption updates the record and reference together; prefix
replay copies them unchanged. Every native step uses this closed record shape,
while the engine and V8 checkpoint contract remains unchanged.

A terminal head's delivered notify pages read its waiting parents one bounded
page at a time from that head's members through their accepted checkpoint
references. The page
carries the member, generation and checkpoint rows it validates, so waking a
parent costs no per-parent resolution and a completion never visits waits on
other children. On PostgreSQL, a child checkpoint must name its accepted member
and no other step may name one, so a pending parent cannot fall out of that
path. The migration compiler does not yet render table checks for SQLite; there
the journal writer alone upholds that rule. Continuation identities keep the
default collation of the generation identities they join, so each join uses
the other side's index.

Continuation preserves the creation-owner relationship, cascade policy, depth
and schedule association. A keyed join creates a waiting relationship without
acquiring ownership. Cycle admission, restart dependency checks and parent wakeup
queries must resolve through the same head protocol. Retention preserves members
and generations needed by heads, unresolved waits and result checkpoints; copied
parent payload references remain independently owned.

Failure/cancellation cascading and parent notification use the closed page
protocol and effective cancellation fence defined next. A delayed page must
neither cancel a restarted generation nor let ordinary continuation escape an
applicable cancellation. Durable parent completion records its propagation
obligation; it does not certify that every descendant has stopped.

### Delivered dependency propagation

Terminal and cancellation propagation across child relationships uses the same
bounded delivery as topic fanout. The originating creator transition records a
durable propagation obligation and its first page intent; delivered pages apply
the effects. No creator transaction visits an unbounded set of children or
parents, so large fan-out and fan-in progress page by page instead of failing
against the transaction deadline.

The closed code-free operation is `Propagate { propagation_id, revision }`, where
`propagation_id` is the typed identity of one creator obligation and `revision`
names its next bounded page. The manager sees no obligation kind, source run,
cursor, affected run or count. Its scoped submission, delivery, fairness and
settlement protocol applies unchanged. Propagate has no deployment prerequisite
or run projection, acquires no hold and creates no maintenance duty, so an
execution barrier cannot delay the cancellation it must finish. Workers publish
it through the outbox like Fanout. `Waiting` confirms a committed successor page;
`Completed` means the obligation is discharged.

Each obligation has exactly one source generation and one kind:

| Kind | Recorded when | Page effect |
| --- | --- | --- |
| Cascade | A generation settles as failed or cancelled, including entry into compensation, while some run names it as a cascading parent. | The next page of that generation's cascading children, in run identity order: live children record cancellation intent, and idle ones also receive an Advance intent. Terminal children are passed over. |
| Notify | A continuation head's current generation becomes terminal while a current parent wait accepted a member of that head. | The next page of those waits, in wait identity order, through the head-directed member join: each idle parent is woken once per page with an Advance intent. |

Settlement probes the parent linkage index for a single cascading child, and
terminal completion probes the head's member join for a single current waiting
parent; neither probe changes a run. Without a match, no obligation is
recorded, and none can become necessary later: a settled generation creates no
further children, and a parent that accepts a terminal head resolves that wait
when it next prepares. Repeated settlement of the same generation, such as
cancelling a run that is already compensating, reuses its existing obligation.

Creator storage adds two tables, each with `id` as its sole primary key:

| Model | Durable identity and fields |
| --- | --- |
| `propagations` | Typed obligation identity, app, kind, source run and generation, cursor, next page revision, finished flag and creation time. App/source run/generation/kind is unique and also serves the cancellation fence lookup. |
| `propagation_pages` | Job identity linked to the exact job receipt and publication, obligation and page revision, and the closed result: cursor transition, finished and superseded flags, affected count and successor specifications. App/obligation/revision is unique. |

The publication journal gains Propagate projections: obligation identity and
page revision are required for Propagate and absent for every other operation,
and app/obligation/revision is unique. Cascade pages select through an index on
the parent linkage, cascade flag and run identity, so each page is an index
range scan. Notify pages and the notify probe reuse the head's member join,
which returns one page in wait identity order; its reads still grow with the
waits remaining behind the cursor and with earlier joiners whose checkpoints
already resolved. Every page's writes stay within its bound.

A delivered page captures original policy and delivery authority before journal
I/O. Exact committed receipt replay is checked first under the app lock and
needs no fresh authority. Fresh work requires the matching persisted
publication, the obligation's current revision and an unfinished obligation.
The first page starts at the initial cursor; a later page must follow the
previous page's Waiting receipt, its committed cursor and its exact successor
publication. The page commits its effects, their Advance intents, the obligation
cursor and revision, the immutable page record, the job receipt and the next
Propagate publication together. A failed page commits nothing, so redelivery
repeats the same page. Historical receipts replay after later pages advance the
obligation; text cursors are validated by equality with neighbouring pages,
never by comparing identity order outside the database.

A cascade page always addresses its recorded source generation. Restarting the
source run creates another generation number, whose children fall outside
every page and outside the fence. A notify page first checks that the head's
current generation is still the terminal source. After a head restart the
obligation completes as superseded without effects: parents keep waiting, and
the restarted generation records its own obligation when it terminates.

Pages reach children lazily, so an unfinished cascade obligation is also the
effective cancellation fence. A cascading child whose parent generation has an
unfinished cascade obligation is treated as cancelled by frontier preparation,
task completion and heartbeat renewal, exactly as if its cancellation were
recorded. Before any page reaches it, it cannot continue as new or suspend into
another wait: its next preparation, completion or resumed frontier settles it as
cancelled, whatever order continuation identities sort in. A completing child
that creates new children settles with its own cascade obligation, so work
created mid-propagation is reached through its owner. Restarting a cascading
child while its parent's cascade is still propagating is a durable conflict, so
a restarted generation never meets a pending page. Once the obligation
finishes, every child it selected had recorded cancellation or was already
terminal; a later restart is an explicit override that no remaining page can
cancel.

### Management and delivery barriers

```text
Creator -> Control              Manager                       Worker / Creator DB
          |                        |                                  |
          |-- authorized command ->| commit command + job + barrier    |
          |<-- accepted receipt ---|                                  |
          |                        |<---------------- poll ------------|
          |                        |--------------- command ---------->|
          |                        |                     check receipt/policy;
          |                        |                     quiesce/fence as needed;
          |                        |                     commit transition + outcome
          |                        |<--------- outcome + ACK ----------|
          |                        | settle job + command outcome      |
          |                        | + revision-scoped barrier changes |
          |-- read status -------->|                                  |
          |<-- closed result ------|                                  |
```

Control authorizes pause, resume, cancellation and restart. The manager records
typed commands with stable request identity and provenance. An accepted management
receipt means durable delivery responsibility; only the creator receipt proves
that the lifecycle operation was applied. Detailed customer state remains a
worker read.

The creator engine first matches the complete request digest. Matching requests
return the recorded closed outcome; changed bodies conflict without altering the
receipt. Explicit lifecycle refusals may be durable `NotFound`, `Conflict` or
`Denied`. Database errors, capacity exhaustion and expired/missing host authority
remain retryable and create no permanent refusal. In particular, expiry of an
admission-policy lease is not equivalent to valid policy with admission disabled.

Delivered management must carry the complete immutable command and a manager-issued
per-run management revision. This order is distinct from deployment activation,
run generation, task epochs and execution-frontier revisions. Persist the command,
job linkage and provisional barrier together. Deliver commands in revision order;
the creator independently rejects gaps and changed identities, and records durable
refusals in that same ordering. Its attempt captures the delivery lease and policy
before waiting for the app lock or performing I/O.

The native manager and creator models link ordered commands to queue delivery
and retained creator outcomes. Every table keeps `id` as its sole primary key
and uses unique indexes for scoped domain identities.

| Owner and model | Durable identity and fields |
| --- | --- |
| Manager `management` | Job identity as `id`, with scoped job linkage. Original request and actor remain separate from the resolved job command. Required run identity and management revision, unique app/request and app/run/revision, derived `blocks_execution` and closed outcome. Queue settlement owns acknowledgement; separate run-state and inbox-ACK fields are removed. |
| Manager `management_scopes` | Opaque typed `id`, unique app/run identity, accepted revision and settled revision. It has no creator-run foreign key. |
| Manager `jobs` | Native operation-kind, optional run identity and optional management request identity. Validate these projections against the immutable specification and digest. Unique app/management-request supplies an independent replay anchor; the linked command supplies management revision. |
| Creator `management_receipts` | Job identity linked to the exact job receipt, app/request uniqueness, required requested-run identity and management revision, unique app/run/revision. Retains the resolved identity digest and closed outcome. Requested-run identity has no run foreign key, so `NotFound` needs no invented run. |
| Creator `management_scopes` | Opaque typed `id`, unique app/requested-run identity and the last applied management revision, linked to retained command history. It survives run and execution-history retention. |

Creator application advances its management revision for both applied commands
and durable lifecycle refusals. Gaps, substituted identities and unknown older
commands cannot advance it. Completed job replay must find matching management
history, while allowing the scope to have advanced since that receipt. Missing
history is corruption, never permission to reapply a lifecycle change.

Pause/cancel/restart can provisionally block conflicting execution jobs. Management
and required reconciliation remain deliverable so the barrier cannot prevent
its own resolution. Barrier ownership includes command identity and lifecycle
revision. A rejected command releases only its own provisional barrier; a delayed
ACK cannot clear a newer command's barrier or undo a later command. Creator fences remain
authoritative for work already delivered when the barrier was installed.

After settlement, remove the matching provisional barrier and let creator control
state govern later execution. A blanket persistent barrier would also suppress
the Advance work needed to finish cancellation, compensation or interrupted-task
recovery. Filter provisional barriers before applying the queue candidate limit.

An unsettled blocking command is its own barrier. Candidate selection admits a
management command only at the successor of its run's settled revision, and
excludes Advance jobs with a pending blocking command for that run. Activation,
cron acceptance and required reconciliation remain eligible. Claim must also
validate the relevant authoritative command/job linkage under the app lock:
damaged native run or blocking projections must not hide a barrier from an
otherwise valid Advance. Join pending commands to their jobs and order records in
bounded pages, then independently inspect pending jobs and open order ranges.
Validate retained rows in memory without per-command database round trips or a
scan of completed history. A failed or exhausted validation cannot grant a
delivery. Settlement records the exact command outcome and advances
the settled revision with the queue receipt, resolving only that command's barrier.

Resume requires current authorized admission. Restart requires source-generation
quiescence before creating another; generation changes fence timers, children
and stale completions. The current creator operation durably refuses live execution
or unsafe descendants. Automatic quiescence would require a separate bounded
preparation state; it cannot wait indefinitely inside a delivery slot.

Creator lifecycle changes, publication intents, management ordering and the exact
job receipt must commit together. Preserve the requested run identity in command
history independently of the actual run foreign key, so `NotFound` can be receipted
without inventing a run. Carry the closed management outcome through queue
settlement and commit it with the command outcome and matching barrier changes.
Generic completion classifications cannot substitute for that lifecycle result.

Deployment prerequisites follow the effective management operation:

| Operation | Deployment choice and authority |
| --- | --- |
| Pause, resume, cancel | Journal-only operation with no deployment prerequisite. Cancellation cannot depend on loading an unrelated current bundle. |
| Restart using the started deployment | Under the creator app/run lock, freeze the current source generation and its deployment identity/hash when the ordered command applies. Verify its existing held journal retention before committing the new generation. The manager does not need that deployment identity. |
| Restart using the latest deployment | Resolve the target from trusted platform metadata when the manager accepts the command. Persist it in the immutable command identity and job. Creator preparation uses that exact deployment, and verifies its journal hold before the final fenced transaction. |

The default full restart uses the latest deployment. A restart from a task uses
the started deployment, and explicitly requesting the latest deployment for a
partial restart is invalid. Derive these effective policies before constructing
the delivery prerequisite; checking only an explicit deployment option would
misclassify a default restart.

`deployments::latest::LatestDeploymentSource` now uses a native ORM join over the
platform catalog: match `zeroship.apps.deploy_hash` to the same app's `app_deploys`
row and return its typed deployment identity and validated hash only when
available. Missing or unavailable targets refuse without a history fallback;
malformed matched identities, hashes or retention states are storage failures.
Calendar activation history and activation timestamps cannot select the ordinary
live deployment. This reader acquires no lock or hold and grants no admission
authority. PostgreSQL and SQLite tests exercise app isolation, moving pointers,
unavailable targets, malformed storage and refusal of source writes. Give the
manager only the source columns required by this query. The canonical platform
migration supplies those grants and server readiness probes them. This source
needs no creator database or run metadata; the creator
checks workflow membership when preparing the frozen target.

Latest is observed during the acceptance attempt. It does not promise that the
app pointer remains unchanged at the later manager commit. Under the manager app
lock, replay the exact original request before consulting the source. For a new
request, keep its selected target through hold preparation outside queue locks,
then reacquire the lock and repeat receipt matching before atomically inserting
the resolved command, job, order and barrier. Require the confirmed hold for that
same target before accepting. Source or retention failure before acceptance is
retryable; a committed receipt bypasses current-deployment lookup. Preserve raw
request identity separately from effective restart normalization. The manager
acceptance path and server now compose this reader with queue retention. Command
and job records independently anchor the original request identity: damaged
lookup metadata must fail closed rather than admit a duplicate command.
Status lookup first checks whether the app scope exists, then takes its lock
before reading either request anchor. Separate unlocked reads could straddle
atomic acceptance under PostgreSQL's default isolation and report valid metadata
as corrupt. An unknown app still returns no receipt without creating a scope.

For a started restart, "started" means the deployment of the source generation
when that command applies, including a generation created by an earlier ordered
restart. It does not mean the original-ever generation or a target guessed at
Control submission. Existing generation retention protects the source while the
command waits. Missing, releasing or mismatched retention is retryable
infrastructure failure, with no permanent refusal and no substitution of active
code. If preparation ever needs external I/O, persist the frozen source generation
and target before releasing the lock. Retries of accepted latest restarts never
re-resolve the current catalog.

`AppWorkflows::management_job` accepts the exact manager `JobLease`. It captures
delivery and policy authority before opening the journal transaction and keeps
their original deadlines through app-lock waits, external I/O and commit. The
handler checks retained job/command identity and management order under the app
lock, then commits lifecycle state, publication intents, command history, the
applied revision and the semantic job receipt together. There is no separate
raw-command application API.

Transitions and Started restarts use the app-locked journal directly. Latest
first checks lifecycle refusal precedence under that lock, then discards its
draft before loading the frozen target outside the transaction. Final application
rechecks receipt/order and the retained target generation, prepares a new draft,
binds the verified registration exactly and commits under the original authority.
An infrastructure failure retains no lifecycle refusal.

Exact completed replay needs neither fresh policy nor deployment clients. Both
application replay and public management `job_receipt` readback take the app
lock before inspecting the linked receipt, command history and applied head, so
their reads cannot straddle atomic application. Requested-run identity belongs
to command history; the generic job receipt carries no invented run reference.

A pause or cancellation outcome may record an applied intent while a
task is leased. Its acknowledgement is not proof that execution stopped; the
executor must observe that intent and stop and join before reporting quiescence.

The wire cutover removes the blanket `JobSpec.deployment_id`. Activation, advance
and cron carry their required deployment inside their operation. Delivered
management carries its request, run and management revision with a resolved
command: transition with its operation, started restart with its optional task
boundary, or latest restart with its frozen deployment. Latest has no task-boundary
field, so a partial latest restart cannot be represented. Normalize the effective
restart policy before resolving this command, while preserving exact acceptance
request matching in the manager.

Reconciliation and collection have no executable prerequisite. Reconciliation
does not acquire the desired deployment's queue hold or keep that hold alive
through its recovery scope. Recovery registration still validates
desired-activation provenance; it does not need executable retention to repair
app journal publications. Queue persistence projects an optional deployment
and validates it against the closed operation and immutable specification digest.
Release scans bounded pages of unsettled app jobs under the app lock and validates
their specifications before ruling out dependencies; a damaged nullable projection
cannot hide an executable job from reclamation. Acquisition,
claim, renewal, settlement and successor insertion enforce retention only for
operations that actually require code.

The representation is implemented across core, queue, creator readers and metadata
transport. Settlement preflight checks the outcome family, and creator receipts
also enforce the operation's supported result. Manager settlement now validates
the authoritative command/job/order linkage and commits its lifecycle outcome,
settled revision and queue receipt together. Exact settled replay validates the
retained linkage before its final enrollment check; it cannot clear a newer
barrier. Public queue submission and successors cannot create management jobs.
Only authorized manager acceptance creates those jobs. The separate inbox polling
and acknowledgement wire routes, client methods and worker grants are removed.
The creator management handler carries its closed outcome through the exact
receipt and ordinary queue settlement without starting an executor. Collection
delivery remains to be implemented. The manager must never query the customer
journal to fill a deployment gap.

## Recovery responsibility and execution capacity

A creator outbox cannot recover itself when its last worker disappears. The manager
therefore owns a durable obligation for every ingress-enabled app, established
before accepting customer data. It survives registration expiry, deployment
replacement and manager restart.

```text
Creator commit exists; publication was lost
                    |
                    v
Manager scope obligation + reconciliation deadline
                    |
          +---------+--------------------+
          |                              |
   eligible worker exists        no eligible worker exists
          |                              |
          |                     persist capacity demand
          |                              |
          |                     host starts authorized worker
          |                              |
          +<--------- enrollment / registration / placement
          |
          v
Deliver bounded reconciliation job
          |
Worker reads its app's pending intents -> submit stable identities
          |
Commit confirmed progress -> ACK + continuation or next recovery deadline
```

Each obligation has a manager-owned deadline even when no job is visibly pending.
Healthy heartbeats cannot postpone it indefinitely because the manager cannot
observe an unpublished customer commit. Bounded immediate publication retry is
an optimization; periodic manager-issued reconciliation supplies correctness.
The native `recovery::Recovery` ledger supplies independent deadlines and pending
jobs for reconciliation and collection. `ensure` registers a trusted activation
and establishes both duties atomically. Repeated activation validates the complete
retained pair without postponing either duty; a newer activation changes only
provenance. Both duties remain app-scoped and require no retained executable.
`dispatch` serializes with queue operations under the app lock and commits the
chosen duty's job identity with its next deadline. A pending job is returned
unchanged across retries and replicas until it settles. A fresh `Waiting`
settlement advances only its matching duty's deadline to manager time without
postponing an earlier deadline. A completed scan preserves the periodic deadline.
Receipt replay and unrelated jobs cannot modify the current obligation. Scoped
pending-job foreign keys prevent deleting a job that still carries a duty or
substituting another app's job.

`due` pages each duty kind by app identity using manager database time and includes
healthy owners. The scheduler resumes after the last returned app, then begins
another sweep after an empty page; newly due work behind the cursor joins that sweep.
Job publication failure leaves the prior deadline and pending identity intact.
The server drives these duties through independently bounded lanes, and its
placement lanes give every app with a claimable duty job an eligible owner or
record its capacity demand. Ingress epochs tie this responsibility to creator
acceptance, and the closing lane retires it once an app idles or is archived;
see [ingress epochs and scope retirement](#ingress-epochs-and-scope-retirement).

A reconciliation job processes an app-scoped page without loading app code.
The persisted scan alternates between publication intents and deployment-hold
intents. Its immutable job receipt captures the phase, selected IDs, current scan
revision and a stable upper boundary before external I/O. These cursors stay in
creator storage; the manager receives only a closed outcome. Selection reads IDs
rather than decoding the whole page's specifications or hold records, so a
malformed intent does not prevent attempts on later IDs. Each phase has a captured
boundary, so publication churn cannot indefinitely defer hold recovery.

The receipt stores a durable offset. Reserving an item advances that offset under
the app lock before attempting recovery; cancellation may leave the item
pending, but redelivery proceeds to its suffix. Each attempt is bounded and
only an exact manager receipt can confirm publication. A hold attempt rereads
the existing durable intent under the app lock and verifies its generation, hash
and requested state before and after the platform call. It never creates an
acquisition or release decision. Missing clients and invalid intents fail only
their reserved item; receipt replay needs no hold client or artifact. Exact
committed receipts remain readable after policy or delivery expiry; absent or
unfinished receipts still require live authority. Failed or interrupted items
remain pending for a later sweep. This separation prevents a repeatedly timing-out
prefix from trapping all later intents in an immutable page.

Finishing the page commits its semantic receipt and scan cursor together. The
scan revision changes even for an empty pass or phase switch. An equal-revision
phase or cursor mismatch is refused before item I/O. A competing page whose
revision was already advanced records its stable receipt without regressing the
cursor or phase. Completing publication scanning switches to deployment holds;
completing hold scanning wraps back to publications. Empty phases follow the same
transitions. `Waiting` requests the next page or phase through the manager's
recovery deadline; `Completed` closes the captured cycle and retains periodic
responsibility. Neither means every intent was confirmed or the app drained. New
intents behind the cursor or above the captured upper boundary join a subsequent scan. Fresh reads
and writes remain bounded by the original manager grant and host policy; this
native metadata turn does not renew itself or run an independent timer.

### Ingress epochs and scope retirement

A scope's recovery responsibility carries a monotonic ingress epoch and a state:
open, closing, retired or abandoned. Activation opens it. A worker obtains the
epoch with its policy lease, bound to its exact key and placement; a plain
refresh never reopens responsibility. An establishment request names the epoch
the creator journal refused, or none when the host holds no epoch and the
journal closed none, as at startup. The manager returns an open epoch above the
named one, reopening a retired scope or advancing a closing one, and recreates
reconciliation and collection duties under the app lock after enrollment,
placement and admission checks and before the lease is issued. Establishment
follows the admission policy; archive masks admission, so an archived app
cannot be reopened by ingress. An epoch the manager never issued is a conflict.

Hosts establish an epoch before they accept ingress. The local host does so
after registering its startup activation's responsibility and before it
announces readiness, through `Recovery::establish`, because it is its app's
platform authority and holds no policy lease. A worker's `AssignmentBindings`
establishes on the first preparation of a placement and falls back to a plain
lease when policy refuses establishment or the app has no responsibility yet, so
an archived app still serves delivered work such as its own closure; later
refreshes only renew. Both hosts attach their establishment to the app
(`AppWorkflows::with_ingress` with `IngressEpochs`; the worker's
`CreatorFactory` receives the placement's `AssignedPolicies`). An acceptance the
journal refuses for a closed epoch establishes an epoch above the refused one
and retries once under the same request identity, capturing the binding's newly
installed authority rather than the authority the request isolate captured.
Concurrent establishments serialize, and one that finds a newer epoch already
installed does not ask again. Hosts report accepted ingress as activity: a
worker through its next lease exchange, the local host with each placement
renewal.

Every creator ingress acceptance captures the epoch with its policy and, under
the app state lock before commit, requires it to exceed the journal's closed
epoch: start, direct signal, broadcast, signal ingestion to a run or a topic,
the pause, resume and cancel transitions, and restart. The fence runs after the
authority, admission and lifecycle checks, so an expired lease or disabled
admission reports its own refusal; a missing or closed epoch is a retryable
refusal. Issuing and revoking signal capabilities commit no run, publication
intent, payload or hold, so they are not fenced; redeeming a capability is.
Delivered jobs are not fenced by the epoch. Their claims and publications meet
the manager's watermark and re-arm below, and payload uploads run under a task
claim that the drain predicates count.

The manager driver's closing lane visits attempts in progress and open scopes
past their backoff that are idle or whose calendar Control disabled, as archive
does. A scope is idle after `recovery::Options::idle_after` without activity:
its epoch's opening, reported ingress, a worker publication or an
intent-producing claim; maintenance and closure never count. The manager begins
closing only when no job for the app is leased, no maintenance job is pending
and no earlier Close is unsettled. It records the app's dispatch cursor as the
closing watermark, suspends the scope's periodic duties for the attempt, and
delivers a manager-origin Close job for the current epoch. An attempt whose
Close has not settled within `closing_timeout` returns the scope to open. Each
attempt defers the next until its timeout plus `closing_backoff` have passed,
the backoff doubling per consecutive attempt up to `closing_backoff_max`; live
work that refuses an attempt defers the next by the backoff alone, and
reopening resets the pacing. The server maps the `workflow.closing_*` settings
and the local host its `[manager]` settings into these options.

Under the same app state lock the worker raises the closed epoch and evaluates
the drain predicates in one transaction: no unconfirmed publication intent, no
hold in transition, no payload in preparation or deletion, no live task claim,
and no deletion tombstone still owed its final resweep. Settlement retires the
scope only when it is still closing at that epoch, the result is drained and no
job was published or claimed above the watermark. Claims during closing do not
cancel the attempt; the watermark refuses its retirement at settlement.

Claiming an intent-producing job or a worker publication reopens a retired scope
before execution. Workers cannot publish Reconcile, Collect or Close, and Close
is never a settlement successor. Registration expiry, release, empty polling,
healthy heartbeats, completed scans and calendar or policy acknowledgements
never retire responsibility. A creator snapshot restore must reopen
responsibility for the restored apps.

Deletion abandons responsibility instead of closing it. For each candidate page
the closing lane reads Control's terminal deletion marker through the manager's
column grant on `zeroship.apps`, and abandons each deleted candidate: its duties
are deleted, a closing attempt is cancelled and the scope row stays as the
epoch's tombstone. Nothing reopens an abandoned scope; establishment and
activation are refused, and claims and publications leave it abandoned. A page
whose deletion state cannot be read visits nothing. The local host has no
Control catalog, so its app is never abandoned; idleness still retires it.

Native PostgreSQL and SQLite contracts cover this protocol. Manager contracts
fence a still-valid lease after Close, refuse retirement in both orders of a
racing acceptance and Close, keep a scope open when a job is claimed or
published above the watermark, reopen exactly once across retries and racing
replicas, converge lost acknowledgements and redelivery on one retirement,
drain archived apps, and hold duties that fall due during closing. The closing
lane's contracts cover the idle and archive triggers, expiry, the doubling
backoff and its reset, deferral by live work, abandonment that nothing reopens,
and a driver pass that closes an idle scope while abandoning a deleted one; the
server repeats abandonment over the canonical platform schema and its column
grant. Creator contracts refuse each fenced path under a closed epoch and admit
it under the next, and pin one establishment and one retry per acceptance,
including through a request isolate's backend. The local host retires an idle
app through a delivered Close, and its next start is fenced, establishes a newer
epoch and completes. An app that deleted a payload retires once the tombstone's
resweep made it final.

**Remaining before production:** the production worker executable must compose
`AssignmentBindings` and the creator factory, which carry this establishment,
and publish the request isolates' backend from them (slice four). Control does
not yet publish deletion as a lifecycle intent, so the lane learns of a deletion
only for the candidates it visits: a deleted app whose scope had already retired
is abandoned only after a re-arm reopens it and the next pass visits it. Snapshot
restore still needs its reopening contract.

The capacity provider takes a zone's declarative target and returns progress or
a durable, retryable refusal, as
[placement eligibility and capacity](#placement-eligibility-and-capacity-provider)
describes. Repeated requests from manager replicas converge on the same target;
provider failure keeps jobs, demand and the target pending. A provider that
starts processes starts the ordinary worker and supplies authorized creator
connectivity through the normal deployment host, not through a queue message.
No new workflow infrastructure service is assumed.

Manager maintenance schedules journal reconciliation, payload collection and
customer retention checks as explicit jobs. Such a job examines customer records
only inside the worker. Its failure retains responsibility and references; it
does not permit platform SQL to inspect the journal or declare the app drained.

### Delivered payload collection

The manager duties, creator handler and server driver implement this contract,
and the local CLI host consumes the delivered jobs. The production worker still
requires consumer composition.

Collection starts with abandoned payload preparations and deletion tombstones.
The manager retains a periodic Collect duty independently of reconciliation.
Activation establishes both duties in its platform transaction. Each duty owns
its deadline and pending job identity; a failing reconciliation lane cannot
postpone collection. Manager replicas publish and retain each job under the
queue app lock. Fresh settlement of the exact pending job may advance only that
duty when the result is `Waiting`. Receipt replay, unrelated maintenance jobs
and another duty's settlement do not change it.

`AppWorkflows::collect_job` consumes a code-free, app-bound delivery. It
captures the original manager lease and host policy before journal I/O and keeps
them through every transaction and object-store operation. A live policy with
admission disabled permits cleanup. Missing, replaced or expired authority
cannot authorize fresh deletion; an exact committed receipt remains readable
without fresh authority or a configured object store.

The creator journal owns `collection_scans` and immutable `collection_pages`,
both under its `__zeroship_workflow_` table prefix. Each page has a scoped foreign
key to its job receipt and no run reference. The manager's `recovery_scopes`
retains activation provenance; `recovery_duties` owns independent per-app/kind
deadlines and pending jobs, with a sole typed `id` primary key and scoped uniqueness.
A scan captures an expiry cutoff and upper payload identity under the app lock,
then pages candidate identities within that fixed range. Its revision, cursor
and cutoff prevent later uploads or expiry changes from extending a sweep
indefinitely. A page stores its exact job linkage, immutable plan and reserved
item offset. Collection does not reuse reconciliation's fields or expose object
identities and cursors to the manager.

Before attempting an item, the worker advances the durable offset under the app
lock. It then rereads that payload's current state, expiry and reference absence.
Only eligible unreferenced objects may enter `deleting`; this state commits
before external deletion and fences uploads and reference promotion. Object
I/O runs outside the transaction. Confirmation reacquires the app lock, checks
the original authority and matching deletion observation, and records a
`deleted` tombstone with the policy's resweep deadline. Concurrent cleanup may
observe that another attempt already advanced the tombstone and leave it alone.

An uncertain or failed delete preserves the fence. A tombstone is deleted once
more after its window, because an upload already dispatched by a dead writer may
arrive after the first deletion. That resweep marks the payload `purged`, which
is final: collection never selects it again and closure evidence no longer
counts it, so an app that deleted payloads can retire. Collection never treats
an absent object as permission to revive its payload record.

Malformed records and failed or timed-out items remain eligible for another
sweep. Their reserved offset lets redelivery continue to the page suffix.
Finishing a page commits its semantic job receipt and scan progress together.
Only the matching scan revision and cursor may advance current progress;
an older competing page can settle without regressing it. `Waiting` means the
captured sweep has another page, while `Completed` closes that sweep. Neither
outcome proves every object was deleted or the app drained.

This cleanup does not retire terminal history, creator request receipts,
management history, job receipts or deployment holds. Those require their own
retention and drain contracts. The local host has no collector of its own; the
manager's Collect duty delivers collection to its consumer.

## Deployment pins, upgrades and retention

Every run generation pins an immutable deployment ID and the normal manifest
content hash. The worker loads the ordinary `.zship`, verifies app/deployment
identity, module graph and runtime descriptor, and uses trusted policy and storage
bindings. Missing or corrupt code leaves durable work unavailable; it never falls
back to the latest deployment. There is no workflow-only archive, executable
snapshot store, upload flow or `--workflow-bundle` option.

A normal redeploy changes the deployment for new runs and activated schedules.
Existing generations replay against their pins. Full restart can deliberately
select another deployment and re-execute the original input. Partial restart
retains the deployment that produced its preserved history. Retained step effects
and payload references copy with their complete app/generation scope.

Mid-run upgrade is a separate proposed checkpoint handoff, not an automatic
consequence of redeploy and not a shipped API. It requires quiescence, retained
target code, bounded business-state transformation and a creator transaction
fencing the source generation. Signal, child, wait and compensation carry-over
must be defined before enabling it. It is not required for the queue cutover.

### Distinct queue and journal dependencies

```text
Manager schedules / pending jobs -- queue holder -----+
                                                      |
                                                      v
                                             Control hold ledger
                                                      |
Creator history / live generations -- journal holder -+
                                                      |
                                                      v
                                             Normal bundle retention
```

The manager acquires queue retention before making a schedule or job executable.
The creator engine acquires journal retention before accepting a dependency needed
for execution or replay. A delivery ACK can discharge queued work while customer
history still needs the deployment. These holders use distinct authenticated
classes and generation sequences.

Control derives holder identity from the authenticated service role and scoped
app. A body cannot choose another holder. Replacement workers preserve the stable
journal holder; manager replicas share their stable logical queue holder. Releasing
a queue dependency cannot release a journal dependency, or vice versa. The queue
hold endpoints accept the workflow service role and derive `HoldScope::for_queue`;
the worker endpoints retain their enrolled-worker checks and derive
`HoldScope::for_app`. Queue requests carry the app, deployment and generation,
without choosing a holder or claiming worker placement.

The manager records acquiring, held, releasing and released intents in its own
database. A fresh publication confirms the hold before committing an executable
dependency. External hold requests run outside the queue transaction; publication
revalidates its authority and hold under the app lock within the original request
budget. Release closes admission under that lock and checks unsettled jobs and
the frontiers of an enabled calendar; recovery provenance retains no code. The
manager decides when to release through its
[queue hold release policy](#queue-hold-release-policy). Completed receipts remain
useful for exact retries without retaining executable code. Generation tombstones
fence late replies after release and reacquisition.

The Control reclamation loop in
[`deploy_retention.rs`](../../crates/zeroship-control/src/cron/deploy_retention.rs)
uses native typed ORM over the platform deployment catalog and the normal app
deployment pointer. It takes the app lock before the deployment fence, matching
normal activation. Current or staged code and either holder class prevent
reclamation. Activation refuses a reclaiming or deleted deployment in the same
transaction that changes the normal app pointer.

The collector commits the reclaiming fence before deleting the manifest outside
the transaction. Failed or interrupted deletion leaves durable retry state;
an already absent manifest permits completion. Catalog and holder tombstones
remain closed to acquisition. Bounded rotating scans advance past retained or
failing candidates, and a captured upper bound prevents new deployments from
indefinitely postponing retries. Collection runs independently of the old
workflow sweeps and never opens customer journals.

Control remains the production owner of hold acquisition and reclamation APIs.
Its native ledger lives in `zeroship-workflow-manager::deployments` for reuse by
Control and the local host. Constructing the manager queue does not open deployment
catalog tables; the server injects the scoped remote Control hold capability.
The customer engine has only its intent state and metadata client capability,
with no dependency on the platform ledger implementation.

Acquisition and reclamation serialize on the deployment record. Confirm retention
before admitting a new dependency. Before releasing a journal hold, a bounded
worker job closes admission for that deployment, checks all customer dependencies
under its app lock, and commits release intent. Generation tombstones reject stale
release/reacquire messages. Queue release similarly accounts for every referencing
schedule/job under manager serialization.

A lost acquire reply causes an idempotent retry before admission. A lost release
reply retains intent until reconciliation; it must not turn into a new release
generation. Reclamation never reads customer journals, and placement expiry or
worker death never proves that code is unreferenced.

#### Queue hold release policy

The manager releases its own queue holds; nothing else does. `driver::Driver`
runs the policy in its retention lane, so the workflow server and the local CLI
host, which share the driver, both release superseded deployments. A queue hold
is released when all of these are true under the app lock:

- The app's enabled calendar does not select the deployment. A later activation
  superseded it, or archive disabled the calendar. A disabled calendar's frozen
  frontiers publish nothing and retain no code; restore publishes a fresh
  activation, which acquires a new hold before its transaction resumes them.
- `require_unused` finds no unsettled job for the deployment and no frontier of
  an enabled calendar on it. A refusal because the deployment is in use is not
  an error: the hold stays held and a later pass retries. Release is never forced.
- The hold has been held for at least `driver::Options::hold_grace`.

The grace protects acquirers. Activation, publication, settlement successors and
resolved Latest restarts confirm a hold with Control outside the queue
transaction, then commit their dependency under the app lock within the same
request budget, which the queue's `transaction_timeout` bounds. The manager's hold
row records `held_at`, the manager clock at its latest transition to held, and the
policy leaves a younger hold alone. `Driver::new` refuses a grace that does not
exceed the queue's transaction timeout, so no pass releases a hold between its
confirmation and the commit that uses it, and an activation racing the pass
commits. An acquirer that meets a release already in flight is refused
retryably and acquires the next generation once the release settles. The
workflow server derives its grace in `ServerOptions::resolve` so that it always
exceeds `workflow.database_command_timeout_ms`; the local host reads `hold_grace_ms` from
the `[manager]` table of its workflow configuration.

The lane pages candidates in hold identity order under a captured upper bound.
It joins the app's enabled selection and the unsettled jobs' deployment
projection before the limit, so selected and in-use holds take no page slot, and
it includes unfinished intents. `Queue::maintain_deployment` decides each
candidate again under the app lock: an unfinished intent resumes, and a held one
is released only if the grace, the selection and `require_unused` still permit
it, so a stale page cannot release a hold that was reacquired or selected after
the page was read. A held row without a recorded time is reported as damaged
and stays held.

Releasing the queue holder leaves the journal holder untouched. The Control
collector reclaims a deployment only once both holder classes are released and
no current, staged or pending pointer needs it; the local host's catalog applies
the same holder-aware fence.

**Remaining before superseded code is actually reclaimed:** creator activation
acquires a journal hold on its deployment, and no creator job releases journal
holds yet. `WorkflowService::release_deployment_hold` performs a checked release,
but nothing schedules it, so every activated deployment keeps its journal hold
and the collector still cannot reclaim it. The journal release job described
above must land for reclamation to follow republication in practice.

The manager's `tests/hold_release.rs` contracts run on PostgreSQL and SQLite:
replacement, archive and restore through holder-aware reclamation, an activation
reacquiring its hold while another replica's lane passes, stale candidate pages,
and a hold without a recorded time. The CLI's `workflow_local` contract completes
a run on one bundle, republishes, and observes the superseded deployment's queue
hold released, in the manager and in the ledger, once every job pinned to it has
settled. The server's driver contract releases an aged, unselected hold through
its Control client.

## Payloads, effects and collection

V8 receives app-scoped primitives and replay data. Rust owns task credentials,
trusted app binding, payload preparation and journal writes. The manager never
receives inline input/output bodies, storage URLs or credentials.

```text
Worker                            Creator storage            Creator DB
   |                                     |                       |
   |-- reserve stable preparation / cleanup identity ----------->|
   |<-- confirmed reservation -----------------------------------|
   |-- upload prepared object ---------->|                       |
   |<-- verified identity ---------------|                       |
   |-- confirm prepared object --------------------------------->|
   |                                                             |
   |-- commit outcome + promote payload references ------------->|
   |<-- confirmed commit ----------------------------------------|
   |                                                             |
   |-- later collection job: fence eligibility ------------------>|
   |-- delete collectible object ------->|                       |
   |-- confirm collection progress ----------------------------->|
```

Preparation preserves a stable upload identity across retries and records enough
integrity metadata to verify reads. A durable reservation before upload lets
recovery find abandoned preparations. Upload failure cannot produce a committed
reference. Outcome, history and reference promotion commit together; an uncertain
commit is resolved before treating a prepared object as abandoned. Collectors
serialize eligibility with promotion so they cannot delete newly retained content.
A crash between object deletion and confirmation is handled by idempotent deletion
and retry of the collection job.

Input acceptance may prepare storage before its final transaction. It must still
obey app ownership, upload limits and durable cleanup responsibility for abandoned
preparations. Payload collection is a manager-issued bounded operation, and cursor
progress commits with its continuation intent. Terminal history, child results,
restart prefixes and pending receipts retain their referenced payloads until their
own retention rules permit release.

Replay verifies app, generation, object identity, content integrity and resource
limits. Missing or corrupt content stops the attempt without allowing different
history to be committed. External effects can repeat if a remote operation
succeeds before its checkpoint commits. Stable effect identities support
idempotency at the destination; queue deduplication does not provide exactly-once
arbitrary external effects.

The replay contract stays in the customer engine and SDK. A durable step reuses
its committed result on replay; an uncommitted step can execute again. Workflow
code must preserve the recorded ordering and identity of durable operations for
its pinned generation. A mismatch stops progression instead of silently accepting
a different history. Wall-clock values, random values and external responses
needed for replay must enter through recorded operations. The manager transports
no replay history and does not interpret application exceptions or compensation
bodies.

## Rust APIs and dependency direction

The native manager is reusable without an HTTP listener. The customer engine is
reusable without the manager implementation or V8. The service client carries
metadata and authentication, not either side's persistence.

| Crate | Responsibility and native surface |
| --- | --- |
| `zeroship-core` | Closed workflow metadata and transport-independent capability contracts; canonical entity identities are supplied by `zeroship-id`. No ORM, HTTP implementation or customer replay envelopes. |
| `zeroship-workflow-calendar` | Shared schedule definitions and pure cron/interval calculations with an explicit interpretation identity. No clock owner, ORM, runtime or scheduling loop. |
| `zeroship-workflow` | `WorkflowService`, bound `AppWorkflows`, creator journal transitions, replay, payloads, bounded job acceptance/execution and publication intents. `management_job` applies delivered lifecycle commands in the creator journal. |
| `zeroship-workflow-manager` | `Queue`, native `coordinator::Coordinator`, scheduling/recovery modules and platform `deployments` ledger. No creator engine, V8 or listener. |
| `zeroship-workflow-client` | `WorkerCoordinator`, `ControlCoordinator` and bounded authenticated transport, extended with the job/hold protocol as callers cut over. No ORM or scheduler. |
| `zeroship-workflow-v8` | `WorkflowBinding`, V8 argument conversion, trusted app binding and executor shutdown barrier over the customer engine. |
| `zeroship-workflow-server` | HTTP routes, enrollment/service authentication, configuration, readiness and manager lifecycle composition. |
| `zeroship-worker` | Existing executable hosting normal requests and the bounded workflow consumer with trusted creator context. |

```text
Normal Cargo dependencies; arrows are not network calls

workflow-server --> workflow-manager --> data-orm [platform binding]
       |
       +----------> workflow-client  --> authenticated metadata transport

worker ----------> workflow --------> data-orm [creator binding]
   |                   +-----------> storage / bundle
   +-------------> workflow-v8 -----> workflow + runtime
   +-------------> workflow-client

Control ---------> workflow-client
   +-------------> workflow-manager::deployments [authorized platform binding]

CLI -------------> workflow-manager [local platform metadata]
   +-------------> workflow         [normal app database]
   +-------------> workflow-v8

All workflow contracts use core / canonical typed identities
Schedule calculation uses workflow-calendar without either persistence owner
```

Manager and customer engine do not depend on each other. The client depends on
neither persistence implementation. Hosts inject metadata capabilities; local
composition can invoke native manager operations with a trusted local caller,
while production uses the client. Customer replay envelopes, inputs, payload types
and detailed execution errors remain in the customer library and V8 adapter.
Owner-specific errors are mapped to closed boundary failures rather than making
the manager depend on `WorkflowServiceError`.

The target keeps queue, scheduling, management and recovery as modules of the
manager. `zeroship-workflow-scheduler` is replaced, including its old binary and
configuration surface. The bundle crate continues to own artifact formats and
verification, not ORM deployment persistence. A generic deployment service or
additional ledger crate is outside this restructuring.

`WorkflowService` is a library handle, not a new deployable service. Its existing
scheduler methods do not justify retaining discovery in workers. Keep customer
transitions and bounded runner mechanics while replacing `WorkerTasks` local
polling and maintenance sweeps with delivered-job acceptance. Runtime-loader
interfaces should match actual I/O; constructing an already loaded runtime can
remain synchronous. `async-trait` is not an architectural requirement. Shipped
I/O stays on compio; no additional async runtime is introduced.

The delivered-job slot retains the current manager grant, creator `DeliveredTask`
and executor handle together. It renews the manager grant first, then the creator
claim, and updates the execution guard only when both succeed. The original hard
execution deadline remains fixed through code loading, input reads, execution
and the executor's payload preparation. Joined shutdown and final journal writes
have a bounded finalization budget with live lease checks. Cancellation drains native
operations before creator release or slot reuse. After a durable creator outcome,
the slot retries immutable settlement metadata without running app code again.
`WorkerCoordinator::release` releases an app assignment; it is not a job NACK.
A deferred or abandoned job stops renewal and remains eligible for redelivery
after its manager lease expires.

Update Cargo declarations, configuration registration, schema generation,
container build inputs, xtask selection and dependency gates with each move.
The existing client's `cyper` carrier follows the client crate; do not retain
duplicate clients or add Tokio as a normal dependency to avoid updating a gate.
Use main's shared ORM API and coordinate its changes with the ORM owner.

### Executable host composition

`WorkerHost`, `AssignmentBindings`, `JobConsumer` and `WorkflowCreatorFactory`
provide native composition seams; the worker executable does not yet construct
them. The production host owns its enrolled identity, configured capacity and
assignment registry on a dedicated compio thread. HTTP runtime threads call fixed
`AppBackend` senders published by that owner. Starting independent hosts under
the same enrolled identity in every HTTP thread would duplicate capacity and let
their policy generations retire each other.

Publish a ready backend only after assignment preparation's final current-entry
and original-authority checks. Successful resource construction alone is not
readiness: its association may retire before installation. Installation and
removal carry the immutable app and binding identity. Removal closes admission
synchronously; previously cloned handles retain their retired generation. An
unknown or unready app receives a retryable refusal. It cannot acquire an ambient
policy binding or fall back to the old Control workflow backend.

The creator-resource provider supplies the exact `ConnectionFactory`,
`ProjectKeySource`, `DbBinding`, object store, deployment capability and signal
authority independently of manager metadata. Assignment scope cannot select
credentials or a schema. Context refresh may supply environment and runtime
limits under explicit freshness, while the workflow backend remains fixed.
Production journal provisioning belongs to the migration path; the worker does
not run local schema initialization. Signal authority provisioning and rotation
and resource eligibility remain explicit host contracts.

Construct the authenticated manager client from the enrolled instance signer,
validated manager origin and bounded transport options. A scope-only retention
adapter must accept an `AssignedScope` and its fixed signer; it must not fabricate
a complete `Assignment` or expiry to satisfy a constructor. Control revalidates
the actual assignment when authorizing each hold operation.

The local host uses the same `JobConsumer` with a CLI-owned native `JobTransport`
over the manager coordinator's real delivery grants. The manager, its Driver and
the platform metadata file live on a dedicated manager thread; the consumer,
creator engine and V8 executor live on the workflow host thread and reach the
manager through a `Send` client. This mirrors the production zone split and keeps
thread-local state of app isolates, such as the ORM usage meter an `env.db`
isolate stamps on its thread, away from platform metadata. There is no journal
polling or maintenance loop. The normal creator storage and retained app archive
remain unchanged. `zeroship_workflow_manager::local::LocalPlatform` is the one
explicit combined bootstrap for the local platform file: it installs the
deployment catalog and manager schemas together and refuses any other stored DDL,
including a deployment-only catalog. This adds no deployable service or
workflow-only database or bundle switch.

The production cutover changes creator request bindings and consumer startup
together with removal of the old claim/provision/advance path. Worker database
posture currently requires Control catalog reads and workflow-owner membership;
those checks and their canonical grants must disappear with their callers.
Control must likewise lose creator-journal access, and the Control/Gateway
advance transport must disappear. The decisive process contract uses isolated
creator and Control databases, ordinary app ingress, manager delivery, revocation
of retained request handles and joined shutdown. Native library availability
alone does not prove this boundary.

### Service operation inventory

This inventory defines responsibility and success semantics. Existing coordinator
routes are registered in
[`workflow-server/src/api.rs`](../../crates/zeroship-workflow-server/src/api.rs).
Authenticated job submission, claim, heartbeat and settlement routes are in
[`workflow-server/src/api/jobs.rs`](../../crates/zeroship-workflow-server/src/api/jobs.rs).
Control schedule preparation, activation and disable routes are in
[`workflow-server/src/api/schedules.rs`](../../crates/zeroship-workflow-server/src/api/schedules.rs).
Control's lifecycle publisher is the durable handoff for normal deployment
publication. Ingress-scope host composition remains cutover work; endpoint
availability alone does not provide its durable handoff.
The inventory includes required semantics beyond the currently available routes.

| Operation | Authorized caller and receiving owner | Successful result |
| --- | --- | --- |
| Enroll/replace instance | Deployment host and worker bootstrap to Control. | An instance identity bound to its enrolled key and authorized deployment context. |
| Register/drain instance | Enrolled worker to manager. | Recorded liveness/capacity; grants no app assignment by itself. |
| Place app | Manager placement lane, reading Control's zone and enrollment rows. | Durable app/worker revision admitted under capacity and zone constraints. |
| Apply capacity target | Manager to the injected zone capacity provider. | Progress or a durable, retryable refusal for that target revision. |
| Poll/renew/release assignment | Enrolled worker to manager. | Only that worker's authorized scopes and current revision outcomes; release does not retire the app's recovery duty. |
| Register/activate deployment | Control to manager. | Idempotent immutable schedule metadata and monotonic activation state; dispatch readiness remains distinct. |
| Disable calendar | Control to manager. | Durable app revision fence and historical receipt; accepted jobs, recovery and creator policy remain independent. |
| Obtain policy lease | Enrolled worker under its current assignment to manager. | Validated policy bound to the exact app, worker key and placement revision, capped by original source freshness and remaining authority. Native Control source, server route and client exist; production worker refresh integration remains open. |
| Establish/close ingress scope | Trusted creator host through manager policy. | Durable recovery responsibility or an explicit fenced drain result. A worker cannot create authority for an arbitrary app. |
| Submit job/intents | Assigned worker or native manager scheduling logic to manager queue. | Receipt for the stable immutable specification; changed content under the same job identity conflicts. |
| Claim job | Enrolled worker with current assignment to manager queue. | A persisted delivery attempt and bounded authority, or no eligible work. |
| Heartbeat delivery | Its worker to manager queue. | Current bounded lease authority after fresh identity/placement checks; cannot revive a replaced attempt, expired execution or elapsed execution budget. |
| Settle delivery | Its worker to manager queue. | Atomic outcome, stable successors and scheduling/barrier changes, or replay of the matching receipt. |
| Submit/read management command | Creator-authorized Control to manager. | Durable command acceptance or closed delivery outcome; no customer history or result body. |
| Acquire/release deployment hold | Authorized queue or journal holder to Control. | Generation-fenced retention result for that holder class and app. |
| Start/signal/read run | App code or Rust caller through an authorized creator-bound handle. | Creator-side acceptance or detailed state. Customer bodies stay on this path. |

The worker requests work through outbound authenticated calls. Polling is bounded
and backs off when no eligible work exists; transport reconnection never changes
a logical operation's identity. The manager does not reach into V8 through a
private advance endpoint. Network loss changes delivery availability, not the
ownership of customer data or the durability of already accepted work.

Read/list operations return bounded pages with opaque, scope-bound cursors. A
cursor is a position, not an authorization token or a promise of a snapshot;
every page rechecks caller scope. Operations needing a stable scan explicitly
capture a revision/cutoff. Queue status remains scheduling metadata; detailed
workflow inspection goes through the authorized creator path.

## Configuration and local development

Host configuration parses once and produces validated Rust options. TOML and
programmatic construction configure the same underlying library behavior;
libraries do not independently reread environment variables. Use the normal
configuration precedence and secret handling rather than workflow-specific
fallbacks.

| Configuration owner | Relevant settings and boundary |
| --- | --- |
| Manager host | `WorkflowSettings` supplies listener, service peers, platform DB binding, body/page bounds and worker/assignment policy. `workflow.database_url` is a platform credential. |
| Native coordinator | `coordinator::Options::{worker_ttl, assignment_ttl, batch_limit, max_pending_management}` bounds placement and command behavior. |
| Native queue | `Options::{max_connections, lease, transaction_timeout, max_successors, max_metadata_bytes}` bounds storage concurrency, delivery and metadata transactions. |
| Native manager driver | `driver::Options::{page_limit, lane_timeout, hold_grace, scheduling, recovery}` bounds each calendar, recovery, retention and closing lane; `hold_grace` is the minimum age before the [queue hold release policy](#queue-hold-release-policy) may release a hold, and `recovery::Options::{idle_after, closing_timeout, closing_backoff, closing_backoff_max}` pace closing. The server maps `workflow.batch_limit` to the candidate page, owns cadence through `workflow.driver_interval_ms` and derives the grace from `workflow.database_command_timeout_ms` so that it exceeds that budget; `workflow.driver_lane_timeout_ms` bounds each lane's complete turn, and the `workflow.closing_*` settings map to the closing bounds. |
| Metadata client | Client `Options::{timeout, max_request_bytes, max_response_bytes}` bounds the complete exchange. Each call uses the host signer. |
| Customer host | Normal creator DB/storage, trusted app identity and policy snapshot. `ConsumerOptions` bounds slots, assigned scopes, claim polling and backoff; `DeliveryOptions` bounds execution and finalization. Worker maintenance scheduling settings disappear with their loops. |
| Local CLI host | `--workflow-config` TOML: `[consumer]` maps to `ConsumerOptions` and `DeliveryOptions`; `[manager]` maps to the queue lease, the local worker's registration and placement lifetime, driver cadence and lane bound, the hold release grace (`hold_grace_ms`, which must exceed the queue transaction timeout), the recovery interval, and closing idleness, timeout and backoff; `[payloads]` maps to `TaskPayloadLimits`. Unknown keys, including database or bundle settings, are refused. |
| Scheduling/recovery host policy | Explicit misfire, overlap, reconciliation and capacity/backpressure bounds. New setting names are finalized with those modules, not invented CLI switches. |

Policy snapshots are host-owned and revisioned. Expired remote metadata does not
become self-renewing authority through a retry, a customer row or a mutable
`APP_ID` environment value. Runtime identity comes from the immutable trusted app
context. Enrollment, policy refresh, code availability and recovery eligibility
are separate readiness concerns.

```text
zeroship serve / Vite local host
       |
       +--> manager thread: manager + queue + driver
       |          |
       |          +--> .zeroship/platform/metadata.sqlite
       |               (deployment catalog + manager metadata)
       |
       +--> workflow host thread: native consumer + creator engine
                  |
                  +--> normal app database + app storage
                  +--> same V8 executor and normal app bundle

               shared protocol, separate storage bindings
```

The CLI composes these libraries in process with normal app configuration. Local
calls omit network and enrollment ceremony while preserving scope checks, grants,
receipts, fences and recovery. The CLI owns setup, startup and shutdown; it
contains no bespoke cron evaluator, workflow bundle loader, deployment watcher or
journal scheduler. Supporting multiple app deployments in the CLI is a separate
concern.

Startup opens the creator journal, starts the manager thread and registers a
freshly minted worker identity as ready. The trusted host places exactly the
configured app on that worker, so local eligibility is that one app. Serving an
archive ingests it into the retained store, records the normal deployment in the
catalog, prepares its schedule descriptors, which carry no creator input, and
activates it at the next revision unless it is already the enabled selection.
The creator applies that Activation as a delivered job; the CLI establishes
recovery responsibility for the selected activation, establishes and installs
its ingress epoch, and accepts requests only after the creator has committed the
activation receipt. A request the journal later fences establishes a newer
epoch through the local manager and is retried once. Serving a plain script
keeps the existing selection. Direct creator activation is not used.

The host renews its registration and placement on an interval derived from the
placement lifetime (`LocalConfig::renew_interval`), reports the ingress it
accepted since the previous renewal, and places the app again under the next
revision when the manager refuses the old one. The HTTP and
workflow isolates share one app backend whose commit hint wakes a publication
pass after every start, signal, transition or restart; the transport wakes it
after every settled delivery, and startup wakes it once for intents a previous
process left behind. A failed pass leaves intents pending for the manager's
reconciliation job. Shutdown joins execution, reports draining and stops the
manager thread after its in-flight operations and current pass.

Customer history stays in the normal app database. The manager uses the normal
local platform metadata/catalog binding alongside deployment identities and
holds, not the customer binding. There is no dedicated workflow SQLite file,
workflow database environment variable or `--workflow-bundle`. Creators need not
supply `APP_ID`. Local co-location does not change the production database
boundary.

Restart preserves queue metadata, journal receipts, pending intents and retained
bundles. Vite's hot reload republishes the archive and restarts the CLI, which
activates the new immutable deployment while existing generations keep their
pins. Local durability and failure behavior match production; replacing the
queue with an in-memory shortcut would defeat that parity.

## Operations, security and backpressure

### Startup and readiness

The manager validates configuration, schema fingerprints, least-privilege grants,
authentication/replay storage and required host capabilities before admitting new
work. It recovers durable jobs, scheduling cursors and responsibility state from
its own DB. Startup does not scan customer databases or depend on a resident worker.
A health endpoint reports process liveness; readiness reports whether the host can
perform its required authenticated metadata operations.

The worker obtains its authorized runtime identity and creator context, enrolls,
registers and receives eligible assignments. It validates creator schema, policy,
bundle and payload capabilities before accepting ingress or execution. A workflow
whose artifact is unavailable remains observable and durable; it does not silently
run another deployment. Authentication or migration failure is a startup/readiness
failure, not permission to fall back to elevated credentials.

### Shutdown and crash recovery

Workers stop new claims and ingress, report draining, and finish bounded executions
or cancel them through the executor shutdown barrier. Quarantine closes isolate
admission; runtime shutdown must still join native operations before capacity is
reused or completion is published. Committed outcomes and
intents remain publishable even if shutdown cannot contact the manager. Unfinished
jobs may expire for redelivery, while the app's durable recovery duty remains.

Managers stop new admission, stop initiating capacity work, and allow owned
transactions to settle or become explicitly uncertain. Timers, job leases,
submission receipts and obligations live in storage, so another replica can
continue. Neither shutdown path deletes durable work merely to make a process
exit cleanly.

### Backup, restore and disaster recovery

Each database owner backs up its own state. Creator recovery includes journal
records and referenced payloads; platform recovery includes queue, schedules,
scope responsibility, enrollment, deployment metadata and holds. Retained bundles
must remain available for every recovered pin. Backup access does not give a
platform workflow process permission to read a creator database.

A process restart is different from restoring an older database snapshot. An old
snapshot can resurrect already settled jobs, lose receipts or reinstate old
assignment authority. At-least-once delivery and ordinary lease expiry alone do
not repair that loss. Independent database restores also cannot be treated as a
consistent snapshot of both zones.

Restore therefore closes admission and fences prior process/placement authority
before replay resumes. Operators must reconcile manager obligations and creator
receipts through authenticated worker jobs, validate payload and code availability,
and retain deployment holds while dependencies are uncertain. Never advance a
manager cursor or mark an app drained merely because restored metadata is empty.
If customer receipts were lost, external-effect duplication must be handled by
the destination's durable idempotency contract or explicit operator remediation.

The restore-epoch and reconciliation handshake is a required operational protocol
still to be defined and tested with the storage owners. Routine crash-recovery
tests are not evidence for snapshot-restore safety. No operator recovery path may
replace the private-zone boundary with direct Control access to creator storage.

### Backpressure and fairness

Bound request/response metadata, successor sets, claim batches, payload
preparation, live runs, replay/frontier size and worker slots through their owning
options. Placement capacity bounds assigned scopes; execution slots separately
bound simultaneous jobs. A worker claims only what it can start within valid
authority. Queue depth is not permission to overcommit the creator database.

Reject new unaccepted work before its durable customer commit when local admission
limits require it. After acceptance, queue or transport pressure leaves publication
pending; it cannot erase the run. Manager rejection must distinguish invalid
metadata, capacity pressure and temporary storage/authentication unavailability.

Scheduling and recovery process bounded pages and yield between app scopes.
Management and reconciliation need progress even while execution admission is
paused or saturated. Retry backoff and fair dispatch belong to manager/host policy;
a hot app or repeated broken deployment must not starve other eligible work.
Within an app, the queue issues a persistent dispatch ticket when publishing a
new job and on each successful claim. Both use the app's `dispatch_cursor` under
the app lock and in the transaction that inserts or leases the job. Candidate
selection filters due times, live leases and calendar prerequisites before ordering
by `dispatch_order` and storage identity. A delivered job whose lease expires
therefore retries behind already waiting work, and later arrivals receive later
tickets so they cannot continually displace that retry. `available_at` remains
immutable eligibility metadata; it is not rewritten to rotate work.

Exact submission replay, heartbeat and settled acknowledgement replay allocate
no ticket. Failed or cancelled claim transactions roll back the cursor and job
together; a lost response after commit preserves the rotation across host restart.
Counter exhaustion refuses the operation rather than wrapping or reusing tickets.
This provides progress among successfully claimed jobs that later fail or expire.
Failures before a successful claim, including invalid stored metadata or a missing
retention prerequisite, still need an observable retry or parking policy. Cross-app
host fairness and management priority remain separate policies. Silent deletion
on retry exhaustion is not acceptable.

### Observability and trust limits

Correlate app, deployment, job, delivery attempt, assignment revision, run generation
and command identity where relevant. Emit closed reason codes for authentication,
capacity, stale fences, unavailable artifacts, publication backlog and recovery.
Measure queue age, due-work lag, retries, publication progress, capacity demand,
execution budget termination and hold/collection progress through native metrics.
Identifiers belong in appropriately scoped traces; avoid unbounded metric labels.

Platform logs and metrics contain no customer inputs, outputs, history, signals,
raw database diagnostics, credentials, signed assertions or remote response bodies.
Detailed workflow errors stay in creator storage and authorized worker views.
Metering remains trusted runtime infrastructure, never a customer-supplied result.

Remote service transport uses authenticated TLS and validates the intended
endpoint and audience. The native client accepts plaintext only for literal
loopback development origins, rejects redirects, bounds streamed responses and
returns closed failures. Body closure and response identity checks apply to nested
metadata as well as top-level envelopes.

Crate boundaries make authority reviewable; database grants, network isolation,
trusted runtime bindings and transaction predicates enforce it. The protocol
defends against stale/foreign requests and malicious app selectors. It cannot
protect creator databases that a compromised native worker process is already
authorized to access. Untrusted app code must stay behind its app-bound V8/native
handles; the native host is trusted to select bindings and enforce those handles.
If deployment policy requires isolation from another app's compromised host,
place those apps in separate worker processes with disjoint credentials. A
registration or job must never expand a process's authorized app set or give it
Control database credentials or another holder's platform authority.

## Failure behavior and verification

| Failure or race | Required behavior |
| --- | --- |
| Input upload or acceptance transaction fails | Return failure without an accepted run; retain collectible preparation state where needed. |
| Acceptance commits but enqueue or response is lost | Request receipt preserves the run; its intent and pre-existing scope duty recover publication. |
| Worker remains healthy while publication repeatedly fails | Manager reconciliation deadline still produces recovery work. |
| Last worker disappears before publication | Durable scope duty requests authorized capacity and dispatches reconciliation. |
| Manager crashes after submitting a job but before replying | Same job identity returns the stored submission result. |
| Worker crashes before customer commit | Redelivery reclaims the frontier without assuming a result exists. |
| Customer COMMIT is uncertain | Read the creator receipt after settlement; never infer rollback from timeout. |
| Worker commits then loses ACK | Redelivery reads its customer outcome without executing the committed turn again. |
| ACK and outbox both publish successors | Shared immutable IDs deduplicate; changed successor content conflicts atomically. |
| Manager COMMIT is uncertain | Retry exact settlement; the stored receipt determines whether it committed. |
| Placement changes or enrollment is revoked during a lock wait | Fresh authorization rejects new mutation; database clock and original budget remain binding. |
| An old app handle or delayed policy reply survives binding replacement | The retained binding/ticket fails; neither captures the replacement's authority. |
| A manager's cached policy source expires | Further leases fail closed; worker polling cannot refresh stale source validity. |
| Old heartbeat reply is lost | Retry can observe/renew a still-live stored lease; it cannot revive locally expired execution or restore elapsed budget. |
| Old delivery tries to settle a replacement | Attempt and assignment fences reject it; settled-receipt replay admits no new writes. |
| Concurrent cron replicas or delayed activation | Occurrence/cursor transaction and activation revision prevent duplicate or retargeted work. |
| Signal races timer, restart or child completion | Creator wait/generation predicates select the valid transition and preserve durable events. |
| Pause is rejected or an old command ACK arrives | Only the matching provisional barrier changes; newer lifecycle authority survives. |
| Payload promotion races collection | Serialized eligibility prevents deletion of committed references. |
| Artifact reclamation races hold acquisition | Deployment fence decides admission; pending jobs/history retain their distinct holds. |
| Capacity adapter or creator DB is unavailable | Demand and accepted jobs remain durable; no cross-zone database fallback occurs. |
| A database is restored to an older snapshot | Fence prior authority and reconcile both owners' durable state before reopening admission; missing receipts require explicit duplicate-effect handling. |

Native Rust tests exercise behavior through the owning crate, with PostgreSQL and
file-backed SQLite for ORM contracts. Required external environments belong to
Testcontainers using major image tags. Unavailable required infrastructure fails
tests; it does not turn them into optional checks. Examples own their Vitest,
TypeScript fixtures and Playwright suites. Use the workflow xtask/native test
selection rather than adding bash orchestration or source-text gates.

| Test owner | Required evidence |
| --- | --- |
| Core/identity | Closed nested variants, malformed IDs/revisions, stable wire identity and no customer fields. |
| Manager | Concurrent capacity/claims, sparse recovery pages, atomic command/queue changes, stable receipts and storage IDs, authority after lock waits, shortened budgets and uncertain-commit retry. |
| Scheduling | Calendar/DST parity, catch-up and overlap semantics, activation without a resident worker, schedule replacement/disable, stable occurrences and due-work recovery. |
| Customer engine | Acceptance/outbox atomicity, duplicate delivered jobs, stale frontiers, receipt retention, policy expiry, signal/child races and generation-safe restart. |
| Worker/V8 | Pinned complete module graph, immutable app context, bounded synchronous and async execution, stopped-native-work barrier and a consumer with local scheduling removed. |
| Client/server | Real authenticated HTTP, instance-versus-role keys, replay protection, enrollment changes during waits, scope/order validation, TLS, streamed bounds and reuse after cancellation. |
| Retention/payloads | Upload failure, corrupt reads, uncertain reference promotion, concurrent collection, queue-versus-journal holders and delayed generation messages. |
| Host integration | Private disjoint DB access, no forbidden grants, zero-worker capacity recovery, ongoing-heartbeat publication failure and restart across both commit boundaries. |
| Local/examples | Same protocol and durability with local native composition; each example's creator-facing behavior through its own tests. |

Lock/cancellation tests observe the actual blocked operation and its storage
result rather than sleep and assume progress. Preserve the distinction between
rollback before COMMIT and uncertainty after terminal dispatch. Dependency gates
inspect normal Cargo edges so production workers cannot import manager/server
persistence, platform services cannot reach the customer engine/V8, and clients
cannot reach either ORM store. Test fixtures may compose both zones.

## Implementation progress and remaining decisions

### Implemented foundations and current gaps

The branch contains the customer-bound ORM journal, replay/lifecycle operations,
durable creator management receipts, payload preparation, bounded runner, V8
binding/executor, normal verified bundle loading and deployment-hold foundations.
The local CLI host composes that journal with the native manager and the
ordinary job consumer; see
[configuration and local development](#configuration-and-local-development).

Local host contracts in `crates/zeroship-cli/src/workflow/tests.rs` start the
host on a real compiled archive. The delivered Activation selects the archive at
the first revision and settles as dispatch ready, and a run started through the
host's ingress completes through manager delivery while the workflow thread runs
metered `env.db` isolates. A sleeping run resumes after a host restart from queue
metadata alone. A republished archive activates at the next revision while
existing runs finish on their pinned code, including after a restart without an
archive. Reconciliation publishes an intent committed outside the host, and an
acknowledgement lost before the manager replays the committed turn without
executing it again. An idle app retires through a delivered Close; its next
start is fenced, establishes a newer epoch and completes, and a restarted host
reopens the retired scope. `crates/zeroship-cli/tests/workflow_local.rs` repeats restart
after process death through the real `zeroship serve` binary, and
`zeroship-workflow-manager` tests the combined platform bootstrap and its
refusal of partial or changed files.

Creator management now consumes ordered deliveries and commits lifecycle state,
publication, applied order, command history and the job receipt atomically. The
manager accepts the command and settles the reported outcome in its separate
queue transaction. The creator native suite passes, including PostgreSQL and
SQLite lifecycle, atomic receipt/order application, original-authority expiry,
exact deployment retention and lost-acknowledgement coverage. Generated schemas
match the migration DSL, and changed code has no Clippy diagnostics. The
production host cutover remains separate from these native handlers.

Creator collection verification covers fixed-cutoff paging, original policy and
delivery deadlines, failed or malformed items, concurrent deletion confirmation,
receipt rollback and exact replay without live storage. Native PostgreSQL and
SQLite cases verify recovery when deletion succeeds but its reply or database
confirmation fails. Referenced history stays intact, and a tombstone resweeps
late uploads once and then becomes final. Delivery-slot and separate-database
consumer tests verify collection without executable work; the shared payload
regression suite also passes.

Started restart validates the locked run against its current generation's
deployment, then checks that deployment's registration and existing held journal
retention. An inactive available deployment remains usable; source inconsistency
is a retryable infrastructure failure. This path uses only the creator journal
and requires no artifact client.

Restart preparation now borrows the caller's app-locked transaction through its
lifecycle draft and bound plan. The plan captures app, run and observation time;
applying it cannot substitute another transaction or target. Ordinary latest
restart uses the same exact-target binding needed by delivered commands. That
binding validates the complete locally verified registration, availability and
journal hold without reselecting the current deployment. Existing lifecycle
refusal precedence remains before deployment binding and counter exhaustion.
The delivered handler obtains and verifies its frozen target outside the journal
transaction, then prepares again under the final creator app lock with the
original captured authority. Started delivery remains code free.

The crate split includes the metadata client, closed job/delivery contracts,
manager ORM queue and platform deployment ledger. Native coordinator placement
and management now share the queue's ORM namespace and transaction handle.
Deployment prerequisites belong to executable operations, with an optional native
queue projection checked against the operation and immutable digest. Recovery
keeps activation provenance without retaining its bundle; each pending duty job
must decode as its recorded maintenance kind. Closed management commands carry
the management revision and resolved restart policy. The native current-deployment reader,
column-scoped platform grants and server readiness checks are composed into
authoritative acceptance. Management request anchors, per-run ordering and
provisional barriers share the queue transaction; barrier and command eligibility
filters precede candidate limiting. Linked management settlement and creator
management delivery and bounded payload collection are implemented.

Native management contracts exercise source changes during hold acquisition,
competing acceptance, original-request replay after catalog changes, damaged
request anchors and pending barriers, ordered settlement and enrollment changes.
Backlog cases cross native page boundaries under the normal queue transaction
deadline. PostgreSQL lock observations verify that status waits behind acceptance
before reading the linked command and job; SQLite exercises the same receipt and
unknown-scope behavior. These tests live in
`crates/zeroship-workflow-manager/tests/management.rs` and its companion modules.

Canonical parent primary keys eliminate the conflicting duplicate identities in
concurrent first registration. Native PostgreSQL/SQLite coordinator and queue
contracts, authenticated server processes, actual platform migration/grants and
normal-dependency ownership checks have passed. Authenticated job submission,
claim, renewal and settlement now have server routes and a typed client. Real
HTTP tests cover scope denial, enrollment revocation and key replacement during
lock waits, process restart and receipt replay after placement expiry. These
checks do not establish a completed distributed workflow system.

Native recovery contracts cover replica races, absence of workers, restart,
activation changes, deadline persistence and atomic publication rollback for each
maintenance kind. Independent-duty tests verify mixed due pages, exact settlement
effects, damaged pending identities and refusal of missing responsibility. The
server process tests prove failed reconciliation cannot suppress collection,
including restart and signed receipt replay without executable holds. Canonical
platform migration tests execute these duties and scheduling with the runtime
role and check table ownership, scoped constraints and denial of worker, gateway
and app access. Authenticated queue HTTP tests and the runtime's combined plugin and
lazy-bundle import regressions pass after the native ORM and module ownership
merge. These checks do not establish the final production host composition or
ambient worker-login isolation.

The ORM owner's cancellation fix passes the creator lifecycle and receipt tests
with their database barriers still held. The shared ORM typed-read allocation
fix also passes the complete workflow library test target on the normal test
thread stack, including the creator journal, management, payload and runner
contracts on PostgreSQL and SQLite. Request receipt tests cover aged records,
reopen, later lifecycle changes and revoked signal capabilities. Host integration
and distributed acceptance remain separate verification obligations.

Ordinary creator ingress now captures its host policy before journal I/O and
rechecks the captured authority after lock waits and before commit. Its original
deadline bounds the entire attempt, including database cancellation; a concurrent
host refresh cannot extend that attempt. Native tests force policy replacement
after staged writes and expiry during a blocked write, and verify rollback and
durable receipt replay. These local fences do not establish assignment-bound
remote policy delivery or a distributed archive acknowledgement. The native
binding and authenticated lease contract is specified in
[policy bindings and leases](#policy-bindings-and-authenticated-leases); its
native capabilities are implemented. Native tests exercise retired app/backend
handles, delayed and consumed refresh tickets, shortening followed by extension,
blocked database cancellation, queued calls retaining their original deadline,
and returned payload streams stopping under revoked authority. Delivery tests
revoke policy while retaining the same consumer scope and keep occupied capacity
until native shutdown joins. Exact semantic receipts and status remain readable
through the retained app scope after execution authority is gone. Authenticated
transport accepts an injected finite source and retains original deadlines across
manager transactions and HTTP. The server now uses the native Control policy
ledger and finite cache. Production worker refresh composition remains required.

Workflow provisioning preserves an existing creator schema's migrator ownership.
Native PostgreSQL container tests exercise both provisioning orders, repeated
runtime provisioning and actual table creation through a confined migrator login.
They also check the scoped app runtime role's data access and DDL/sibling denials.
These scoped-role checks do not prove ambient worker-login isolation: existing
platform migrations still grant the workflow owner role to worker and Control,
and workflow provisioning still grants that role schema creation authority.
Remove those obsolete edges with the legacy journal provisioning paths.

Creator publication contracts cover acceptance and checkpoint rollback, changed
acknowledgements, lost replies, confirmation failure, concurrent publication,
reopen, scope isolation and generation/frontier changes. Queue submission in
these tests uses a separate native manager database. Creator advance delivery
now has exact-run acceptance, task renewal/release, atomic checkpoint/outcome
receipts and retained semantic replay. The old poller excludes manager-owned
runs both during candidate selection and again under the app lock; old task APIs
refuse job-bound claims. This exclusion is temporary cutover protection, not a
production execution mode to preserve.

Native creator delivery tests use PostgreSQL containers and SQLite journals
with a separate manager database. They cover competing frontiers, duplicate
live acceptance, receipt-write rollback, shorter creator leases, expiry and
policy changes during lock waits, stale completion/release after reclaim, and
lost-ACK redelivery without another execution. Retained outcomes survive creator
history removal and reopening. These contracts do not prove the production
scope-duty admission handshake or the executor/queue consumer integration.

`runner::delivery::DeliverySlot` now drives an already authorized advance job
through the existing executor and payload pipeline. SQLite journal tests with
deterministic metadata/executor fixtures cover paired renewal, retained ACK
content, redelivery without execution, changed lease identity, hard execution
limits and a blocked shutdown that stops renewing without freeing its slot.
The remote metadata adapter uses `WorkerCoordinator`; authenticated executor
integration remains a separate verification obligation.

`runner::consumer::JobConsumer` now drives manager claims through that slot.
Its trusted host supplies a bounded snapshot of `ConsumerScope` values, each
pairing an app-bound creator handle with its own executor and task payload store.
Cloning a binding preserves its local identity. Replacing or removing it cancels
its in-flight claim and execution; the occupied slot stays unavailable until
native shutdown joins. A caller that abandons the consumer future must drain it
before discarding it; restarting consumption also drains first. Other free slots
may serve current bindings. Manager and creator fences remain authoritative when
an old and replacement attempt overlap across different slots or workers.
Retirement is retained by the `ConsumerScope` identity, including external clones.
Reapplying a cached snapshot cannot restore a scope rejected by manager authority
or removed by the host. Fresh host authorization creates a new binding; ordinary
refreshes preserve live bindings and do not interrupt their execution.

Claim selection rotates between eligible apps with per-app idle and failure
delays. Concurrent claim I/O for the same app is serialized locally; execution
capacity is shared across all bindings. The consumer validates returned app,
worker and assignment identities before creator acceptance. It does not derive
policy leases from wall-clock placement timestamps, register its own app scope,
scan the creator journal or retire durable recovery responsibility on shutdown.
The placement/runtime host must authenticate and refresh binding snapshots;
manager claim and heartbeat operations verify current placement and enrollment.

Consumer tests cover separate creator databases, foreign delivery rejection,
capacity and scope rotation, atomic binding replacement, claim cancellation,
and execution retained through a blocked drain. A native coordinator/queue test
drives publication, creator execution, checkpointing and exact ACK retry through
separate ORM databases. These tests use a deterministic executor; they do not
establish V8, authenticated network or production host composition.

`runner::assignments::AssignmentBindings` now composes the authenticated worker
client, remote policy bindings and consumer snapshots. It reads assignment pages
until an empty page, with a cumulative scope bound and a deadline for the complete
scan. Failed, oversized or superseded scans preserve installed bindings. Pages
are not a transactional snapshot: fresh scoped policy and job exchanges remain
the authority, and a later scan discovers concurrent placement changes.

Unchanged assignments retain their policy generation and live consumer identity.
Complete scans retire removed or replaced generations before preparation I/O.
An injected `CreatorFactory` resolves independently authorized creator resources;
the reconciler verifies its returned app and exact policy binding. Each app's
renewal, policy refresh and setup progress independently, with setup bounded by
its captured original policy deadline. Cancelled setup is retryable, and a late
completion cannot reinstall a retired association. Closure cancels preparation
and revokes local admission; it does not release placement or discharge durable
recovery responsibility, and the host must still join consumer execution.

`runner::host::WorkerHost` composes the runtime-local registration and refresh
lifecycle with this reconciler and the authenticated job consumer. Initial
registration succeeds before discovering placements. Registration, placement
scans, policy renewal and job consumption then progress independently. Each loop
waits after its operation instead of replaying missed ticks. Advertised capacity
counts app placements, with execution slots bounded separately. Transient
transport and service outages retry; identity refusal and invalid registration
responses stop the host. No registration receipt creates local creator authority
or a readiness promise for an app whose resources have not been prepared.

Shutdown drops pending ready-registration and refresh futures before publishing
draining. Local bindings close synchronously, and bounded draining publication
runs alongside joined consumer shutdown. A failed manager exchange cannot skip
joining execution or turn the same host ready again. Cancelling the lifecycle
future also retires local authority; the owner must call `drain` to join retained
slots before discarding their capacity. The manager's terminal worker tombstone
fences a delayed ready registration whose outcome was unknown to the host.
Shutdown does not fabricate assignment-release or recovery-completion evidence.

This native composition leaves the production creator resource provider with the
host. It does not replace enrollment bootstrap, zone eligibility, normal
deployment recovery handoff or the private-zone cutover.
The worker's `WorkflowCreatorFactory` implements creator assembly over an injected
`WorkflowResourceProvider`. The provider receives an `AssignedScope` so it can
bind artifact retention to the current assignment revision, but database and
storage capabilities must come from independent deployment-host authorization.
Before journal I/O, the factory checks the resolved storage app, runtime app and
physical schema and validates native peers and network policy. It opens and
verifies the already provisioned ORM journal; it never applies creator DDL.

`HostPolicies::run_bound` captures the factory registry's original policy authority
before polling resource preparation. Foreign-registry bindings cannot start I/O;
refreshes do not extend an existing preparation deadline. The registered
`AppWorkflows` produces both its app-scoped `WorkerTasks` payload/artifact handle
and its V8 backend. The runtime loader retains that backend instead of accepting
one from refreshable metadata. Runtime contexts may update env, limits, network
rules and ordinary peers, but must retain the original app and creator schema.
Replacing a policy generation therefore cannot route an old executor through a
newly authorized workflow backend. The injected provider and startup installation
remain production integration work; this adapter creates no enrollment, placement
eligibility or platform database capability.

Native factory tests reject wrong app, registry and physical schema before
creator I/O, verify missing journals remain unprovisioned, and cancel pending
resource resolution when its policy generation retires. Delivered activation
and bounded Advance tests execute a retained bundle in V8 and stage/read blob
step output through the supplied creator object store. Loader tests rotate env
metadata across policy replacement and verify retained workflow authority stays
retired until the host explicitly constructs a replacement loader.

Native HTTP tests verify signed endpoint assertions and replay rejection while
creator fixtures open isolated SQLite journals. They cover failed and superseded
scans, exact factory authority, replacement, cancellation, closure and independent
policy refresh while another app's setup is stalled. Runner tests separately
retain execution capacity until native shutdown joins.
Host lifecycle tests cover startup ordering, registration retry, malformed and
refused receipts, independent registration during a stalled scan, cancellation
before startup and terminal draining. Creator fixtures verify that cancellation
retires in-progress setup and that registration refusal revokes retained app
authority before draining waits on the network.

Native manager scheduling now prepares immutable deployment descriptors, records
monotonic activation and publishes due cron/interval occurrences through the ORM.
Activation and recovery responsibility commit together. Occurrence publication
commits stable jobs and the catch-up cursor together, and readiness depends on
each occurrence's own completed activation. Stored descriptor and job linkage
checks reject inconsistent records before publication, replay or delivery.
Pure calendar calculations live in `zeroship-workflow-calendar`; the worker and
manager share their interpretation without sharing persistence.
PostgreSQL and SQLite scheduling contracts cover immutable preparation,
activation replay, blocked delivery, replacement and removal with pending work,
replica races, restart, persisted catch-up limits, due pagination and transaction
rollback. Corruption regressions reject changed descriptors, substituted jobs
and missing occurrence linkage. Native schema declarations also pass parity
checks against the migration artifacts.

Native schedule disable/restore shares the activation revision order. Disabling
an app preserves its calendar cursor, frozen catch-up allowance and accepted
jobs. Restoring the same deployment creates a fresh readiness activation while
preserving calendar progress; replacing it selects the replacement's calendar.
Historical command replay cannot undo a newer selection. Disabled scopes are
excluded before due-page limits. Exact-Control authenticated register, activate
and disable routes expose these native operations independently of workers.
Control's deploy, archive and restore commands reach them through committed
lifecycle intents that its publisher delivers in revision order.

Creator activation now resolves the immutable deployment hash through the
authenticated journal hold receipt and verifies the normal app artifact. Its
readiness history, current selection and completed job receipt share a creator
transaction. Late older activations remain ready for their queued jobs without
replacing newer selection. Exact retries validate the stored readiness and
replay without loading or reacquiring the artifact. The bounded delivery slot
acknowledges activation without starting an executor. Captured delivery and
policy authority fence lock waits, external I/O and journal commits. Once manager
activation selects code, direct local activation cannot replace it.
PostgreSQL and SQLite activation contracts cover delayed delivery, lost replies,
expired authority, policy replacement, missing artifacts, readiness corruption
and transaction rollback. Retention tests cover unresolved hash recovery,
concurrent replies and stale generations. The full creator library passes,
including schema parity, object storage and executor-free activation delivery.

Creator cron acceptance now consumes manager-delivered occurrences. It verifies
the selected activation and ordinary retained bundle, resolves static input from
that deployment, and binds the manager's schedule identity to its logical name.
The app transaction checks overlap and admission, creates the exact run and
Advance publication intent, and retains the occurrence and completed receipt.
An overlap skip is durable; capacity, policy and unavailable prerequisites remain
retryable. Journal hold reacquisition and the final generation check preserve
retention across delayed delivery. Exact retries validate receipt linkage before
performing artifact I/O.

The creator's calendar loop and its reconciliation metadata have been removed.
Creator schedule rows hold acceptance identity; occurrence rows require the
manager revision and receipt link. The delivery slot acknowledges cron acceptance
without starting an executor, and a lost acknowledgement replays the retained
result. Paired native contracts cover historical input, conflicting identities,
overlap through continuation, admission changes, retained code, expired authority
and rollback of the run, publication and occurrence together.

Queue retention is required by executable queue and scheduler operations.
Journal-only jobs and recovery responsibility need no deployment hold. Release
validates unsettled job specifications through native ORM pages, so inconsistent
deployment projections fail before any external release request.
PostgreSQL and SQLite tests cover lost hold replies, stale generations,
failed publication, schedule replacement, pending jobs after replacement and
independent journal retention. They use a separate deployment catalog and ordinary
app artifacts. Authorization tests preserve post-write revocation and cancellation
rollback while also refusing unauthorized hold preparation. The Control collector
now uses the same native retention ledger and protects the normal deployment
pointer in its reclamation transaction. Its canonical platform grant, deletion
recovery and activation-race regressions pass. The tests also verify independent
queue and journal holders, app scope, archived current deployments and denial of
creator-schema access. The collector is the production caller of manifest
deletion; it no longer delegates deletion authority to creator journal scans.

The native manager driver now runs in the workflow server independently of
worker registration and placement. Its calendar, reconciliation, collection,
retention and closing lanes each share an original deadline across their scans
and candidate page.
Each lane captures an upper storage identity and advances past an attempted
candidate before external work, preserving progress through malformed metadata,
timeouts and cancellation. Failed candidates retain their durable jobs or
intents and retry after the finite sweep wraps. New rows and work becoming due
behind the cursor join a subsequent sweep. The host delays between completed
passes and joins its current bounded pass during shutdown.

The retention lane resumes acquiring and releasing queue hold intents and runs
the [queue hold release policy](#queue-hold-release-policy) in the workflow
server and the local CLI host. It never takes an empty queue or an expired
worker as permission to release held code or retire an ingress responsibility:
every release checks all manager dependencies under the app lock, after the
hold outlived its grace, and only a delivered Close with drained evidence
retires a responsibility. Capacity activation and production worker consumer
composition remain to integrate.
The consumer accepts activation, cron, advance, reconciliation, management,
collection, fanout, propagation and closure jobs. Collection uses the assigned
creator journal and object store without loading an executable, creating a task
or publishing unrelated intents.
Fanout uses the assigned creator journal without an executable or object store;
its bounded page commits recipient signals, affected frontiers and the successor
intent together. The former creator broadcast scanner has been removed. The queue
claim is not filtered by operation.
The paired native fanout contracts cover reordered delivery, immutable page
replay, signal ordering under timestamp regression, expired original authority,
counter exhaustion, corrupt progress and atomic rollback. Delivery tests verify
deferral and lost acknowledgements without an executor or storage access, and
the consumer contract progresses a published broadcast through separate creator
and manager databases. The full creator library and changed-code lint pass.

Stable continuation heads replace the eager parent rewrites. Paired native
contracts cover repeated continuation with paused and keyed joiners, current
and historical restart, inherited cancellation and compensation, rollback,
retired authority and scope refusal. Provenance contracts retain inline and
referenced child results through a later child restart, a parent prefix copy
and delivered collection of an abandoned preparation. Checkpoint references
keep their members and generations until the checkpoint itself is removed.
Terminal-history retirement remains open.

Delivered dependency propagation replaces the inline cascade and parent wakeup.
Settlement and terminal completion record only an obligation and its first
Propagate intent; `AppWorkflows::propagation_job` commits each bounded page, and
the delivery slot and consumer route it without an executor or object store.
Paired PostgreSQL and SQLite contracts page cascades and notification past
`MAX_ROW_LIMIT`, replay pages exactly after later pages and with exhausted
authority, wake a parent with several waits once per page, and supersede a
notify page after a head restart. They leave a restarted cascade source's new
children untouched, cancel continuation and child creation mid-propagation
through the fence, refuse restart until the obligation finishes, report the
fence at delivered and legacy renewal, and fail closed on damaged projections,
obligations and page records. Injected page failures roll back the cursor,
effects and receipt together. Manager contracts accept worker-published
Propagate pages and successors without holds or run projections, keep them
deliverable behind blocking management commands, and leave maintenance duties
unchanged; the consumer contract settles a page through separate manager and
creator databases.
The server injects an authenticated Control hold client into its native queue.
Control's deployment transaction records a lifecycle intent at a stable app
revision, and its publisher delivers registration, activation and disable
afterwards, so no HTTP attempt after commit is the only handoff. The collector
keeps a pending activation's code until the manager's exact receipt discharges
the intent. The manager acquires its queue hold through Control outside
Control's deployment transaction, so the callback never waits on the
transaction that initiated it. Control contracts drive the signed manager routes
over the canonical platform schema: revision order across deploy, archive,
staged deploy, restore and rollback; empty schedule removal; a publisher killed
after commit, a reply lost after remote activation and a failed confirmation;
a manager conflict; a blocked app beside others; retention until the queue hold
takes over; and rollback of every staged catalog write.
Publication intents and advance-job receipts exist in the journal; their
delivered reconciliation and queue settlement are integrated natively. Creator
reconciliation tests exercise persisted progress, failed publication, lost hold
replies, stale generations, malformed intents, policy replacement, concurrent
scans, receipt rollback and expired-authority receipt replay on PostgreSQL and SQLite.
Manager contracts cover continuation deadlines, periodic responsibility, old ACK
replay and atomic rollback. Consumer tests connect manager-issued reconciliation
to outbox publication and subsequent execution in separate ORM databases. The
production dispatch/activation host and ingress responsibility handshake remain
unwired. No host polls the creator journal or runs journal maintenance loops any
longer. Direct task polling (`RunnerSlot`), direct activation (`activate_deploy`)
and the synchronous payload collector remain in the creator library only for
executor and journal tests.

Legacy production paths still include Control journal access, worker platform
queries, grants incompatible with private zones and Control/Gateway workflow
advancement. Remove their producers, consumers, schema/grant dependencies and
configuration together at cutover. Do not describe the production boundary as
complete while those paths remain. There are no production users requiring
compatibility aliases or parallel legacy modes.

The V8 executor still calls `Runtime::call_workflow_dispatch`, and runtime startup
still supplies its workflow replay entry. The bootstrap package has been removed;
the live interpreter remains in the runtime's private workflow bridge. Coordinate
changes to `crates/zeroship-runtime/src/core/init.rs` and
`crates/zeroship-runtime/src/core/runtime.rs` with their startup owner. Verify
outcome batches, task-bound payload reads, interruption and joined shutdown through
the replacement before deleting that interpreter.

Retained-entry workflow lookup now captures the constructor-to-export binding
before invoking a workflow. Parent lookup and child frontiers use the same
binding; neither minification nor mutation of `Function.name` changes a target.
Named exports and explicit `default.workflows` entries must agree, and ambiguous
aliases or unexported child constructors fail dispatch. The native module-graph
contract verifies generic named-export forwarding without a Vite collector.
Vite now forwards creator named exports through the ordinary app bundle and
provides native development HTTP/RPC entry snapshots without a workflow lookup
callback. Local workflow tasks use ordinary retained bundles independently of
those replaceable snapshots. The development host continues publishing its app
archive and retains the last valid deployment when current sources fail to build.

### Decisions still requiring an explicit contract

| Decision | Fixed requirement and remaining choice |
| --- | --- |
| Placement eligibility and capacity provider | Decided: Control's frozen app and enroller zones, read under the placement locks, and a declarative per-zone target applied by an injected provider. Providers that start processes remain, pending the production orchestrator. |
| Archive acknowledgement | The direct Control source provides bounded convergence under original observation validity. Define any stronger execution-quiescence evidence separately from calendar acknowledgement or lease expiry. |
| Complete job envelopes | Operation-specific deployment prerequisites, frozen manager restart targets and linked management outcomes are implemented. Collection, topic fanout and dependency propagation have durable pages, receipts and delivered consumers. |
| Receipt retirement | Define admissibility fences and publication/settlement watermarks before deleting job deduplication state. Retain it until that proof exists. |
| Dispatch fairness and persistent failure | Per-app dispatch tickets rotate successfully claimed jobs behind waiting work without changing due times. Define cross-app host fairness, management priority and observable parking/retry policy for failures before claim without deleting accepted work. |
| Snapshot restore | Define restore epochs, fenced admission and cross-owner reconciliation with the storage owners; process-restart recovery alone cannot protect lost receipts or resurrected authority. |

These are design decisions, not unspecified permission to improvise in separate
implementations. They do not reopen the database boundary or require another
broker/service. Mid-run upgrade has its own unresolved semantic contract and is
outside the queue cutover.

### End-to-end path

The local CLI host composes the native libraries, as slice one below describes;
the production executables do not yet. Remaining work proceeds as vertical
slices. Each slice ends with an executable path that its owning native suites
and the workflow examples exercise, instead of adding further library breadth
first. Slices that touch disjoint crates may proceed in parallel; the numbering
is the merge order.

1. **Local host on the native manager (implemented).** `zeroship serve` and
   the Vite development host compose the manager `Coordinator`, queue,
   scheduling, recovery and `Driver` over the local platform metadata file on a
   manager thread, and the ordinary `JobConsumer` over the app's creator
   storage on the workflow host thread. A CLI-owned `JobTransport` calls native
   coordinator operations for a trusted local worker, with real delivery
   grants and no enrollment ceremony. `LocalPlatform` is the one explicit
   bootstrap holding the deployment catalog and manager metadata together.
   Publishing a bundle registers and activates its schedules through the
   native manager, so creator activation arrives as a delivered job. Startup
   establishes the app's recovery responsibility before the host accepts
   requests. `WorkflowWorker` polling, its maintenance loops and the host's
   direct activation are gone. Proof: the CLI `workflow::` contracts and the
   local tier of both example suites run through delivered jobs, including
   restart with a sleeping run, a lost acknowledgement and a republished
   bundle.
2. **Bounded dependency delivery.** Cascading cancellation and failure, and
   parent notification, become paged jobs with durable progress in the creator
   journal, following the fanout and collection pattern. This touches only the
   creator engine and manager job contracts, so it can proceed alongside the
   next slice. The native contracts are implemented; see
   [delivered dependency propagation](#delivered-dependency-propagation). The
   local host delivers these pages once slice one's consumer runs there.
3. **Normal deployment publication (implemented).** Control's deploy
   transaction records an idempotent command receipt and a lifecycle intent
   together. A Control publisher delivers register, activate and disable to the
   manager and confirms only exact receipts. Pending activations keep their
   bundle until confirmed. Archive and restore use the same intents, and the
   CLI and `@zeroship/control` carry the command identity. The legacy schedule
   reconciler is deleted. Proof: Control's deploy HTTP contracts for replay,
   conflict, concurrent duplicates, rollback and refusal; its publication
   contracts against the signed manager routes; and the CLI and SDK command
   contracts.
4. **Worker executable.** The production worker runs `WorkerHost` on a
   dedicated thread with a trusted creator-resource provider, the enrolled
   instance signer and joined shutdown. `WorkerHost` publishes an app's backend
   to request isolates only after assignment preparation passes its final
   authority checks, and retires it synchronously on removal; an unknown or
   unready app receives a retryable refusal. `WorkflowBinding` uses that ready
   registry instead of the old Control backend. The worker enrolls and
   registers as [enrollment](#enrollment-bootstrap-and-revocation) describes,
   and releases an app it cannot serve as refused.
5. **Ingress responsibility and capacity.** The ingress epoch gates every
   creator acceptance, hosts establish it at startup and after a fenced
   refusal, and the closing lane retires idle or archived responsibility and
   abandons deleted apps, as
   [ingress epochs and scope retirement](#ingress-epochs-and-scope-retirement)
   describes; the local host runs it end to end and the worker library carries
   it for slice four. The capacity half ships: the manager owns placement, the
   old Control-driven assign, worker-listing and recovery routes are deleted,
   Control names an app's zone at creation and narrows its host app reads to
   the calling instance's zone, each zone's declarative target is configured
   and applied through an injected provider, and scale-down drains before
   anything becomes removable, as
   [placement eligibility and capacity](#placement-eligibility-and-capacity-provider)
   describes, so due work with no eligible owner gets one. A provider that
   starts real processes waits for the production orchestrator.
6. **Atomic legacy removal and private-zone proof.** One change deletes worker
   claim, provisioning and advance paths, Control and gateway advancement,
   Control's creator-journal access, the cross-zone grants and posture checks,
   and the runtime workflow bridge entry once no executor uses it. The decisive
   process contract runs separate creator and Control PostgreSQL containers,
   ordinary app ingress, manager delivery, revocation of retained handles and
   joined shutdown, and both examples' deployed tier run through it.

Integrate shared ORM changes from their owner rather than introducing
workflow-specific replacements. Report the production cutover complete only
after slice six's process contract and both example suites pass.
