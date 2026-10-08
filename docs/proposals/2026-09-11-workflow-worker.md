# Workflow manager, durable job queue and workers

**Status:** Agreed architecture, implemented in the workflow service, the
native manager, the runner and the worker, with the decisions still open listed
under [remaining decisions](#decisions-still-requiring-an-explicit-contract).
Workers pull claimable jobs from their execution zone, and nothing assigns an
app to a worker. The local CLI host runs the same manager, claim and consumer in
one process.

The manager owns when work becomes runnable and how it is delivered. An ordinary
app worker pulls a claimable job of its zone, executes a bounded operation, has
the workflow service commit the result to the journal, and acknowledges closed
metadata. A workflow can outlive the worker, isolate and delivery attempt that
advanced it, and any worker of the app's execution zone may take its next job.

The principal rule is process ownership: **workers run creator code and access
only authorized creator databases; the workflow service holds the journal and
the queue in its own platform schema; Control owns the control-plane catalog.**
Neither side receives the other side's database credentials. A worker reaches
the journal only through authenticated calls the service answers, and the
service reaches no creator database.

This proposal supersedes the earlier
[control-plane design](2026-07-05-durable-workflows-design.md),
[scheduler registration design](2026-07-08-durable-workflows-scheduler-worker-design.md)
and [implementation plan](2026-07-05-durable-workflows-implementation-plan.md).
The [journal relocation](2026-09-19-workflow-journal-relocation.md) records why
the journal lives in the workflow service. The
[workflow reference](../reference/workflows.md) describes creator-facing APIs;
statements marked as target or required work here are not claims of shipment.

Read by concern:

- [Components and private zones](#components-and-private-zones),
  [worker identity and zone-scoped pull](#worker-identity-and-zone-scoped-pull),
  [policy bindings](#policy-bindings-and-the-service-side-binding).
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
| Execution zone | An operator-declared set of worker deployment units sharing creator-side connectivity. Every app and every enrolled worker instance belongs to exactly one, frozen at creation and at join. It is the isolation unit for workflow execution. |
| Scope | An app and its frozen execution zone: the `queue_scopes` row whose lock every queue change for the app takes. |
| Zone claim | A worker's request for up to its free execution slots of claimable `advance` jobs among its own zone's apps, continuing from a cursor. |
| Run | A durable invocation of a workflow, independent of the process executing it. |
| Generation | A run's execution incarnation, with its own pinned code and replay history. Restart creates another generation. |
| Frontier | The current committed point from which execution may advance; its revision changes when the journal advances. |
| Job | A durable instruction to perform a bounded operation. A run can require successive jobs. |
| Attempt | A particular delivery of a job. Redelivery changes the attempt, not the job's meaning. |
| Give-back | Returning a claimed, unusable delivery to `ready` in the same request, with a back-off before it is claimable again. |
| Fence | An identity or revision checked by a write so an obsolete execution cannot modify current state. |
| Receipt | A durable record of an accepted operation or committed outcome, used to answer retries consistently. |
| Publication intent | A journal transaction's durable instruction to publish queue metadata after commit. |
| Recovery responsibility | The manager's obligation to revisit an app even when it cannot see unpublished journal work. |
| Deployment hold | A durable reference preventing normal bundle reclamation while queued work or journal history still needs its code. |
| Policy binding | A trusted host's immutable association between an app handle and a local authority generation; replacement retires existing handles. |

## Components and private zones

```text
PLATFORM ZONE

  Creator management / normal deployment
                  |
                  v
            +-----------+   lifecycle intents   +-------------------------+
            | Control   |---------------------->| Workflow service        |
            |           |<-- scoped hold API ---|                         |
            | Authz     |---- app facts ------->| Cron and timers         |
            | Deploys   |                       | Zone queue / recovery   |
            | Retention |                       | Zone capacity targets   |
            | Enrollment|                       | Management commands     |
            +-----+-----+                       | Journal + maintenance   |
                  |                             +-----------+-------------+
                  v                                         |
          Control database                     workflow_manager schema
                                               (queue, scopes, journal)

=================== authenticated service API ========================
          Workers pull claims and report; nothing is pushed to them

CREATOR EXECUTION ZONE (one per execution zone)

  Normal app request                  Zone claim / heartbeat / settle
          |                                          |
          v                                          v
  +------------------------------------------------------------------+
  | Ordinary zeroship-worker                                         |
  |                                                                  |
  | env.workflows -> RemoteWorkflows    Batch claimer + slots        |
  |   (any app of the zone)             PreparedApps (bounded cache) |
  | Trusted app context -> pinned app code in a fresh V8 isolate     |
  +-----------------------+-------------------------+----------------+
                          |                         |
                          v                         v
                 Creator databases           Object storage
                 (env.db)                    payload objects
```

`zeroship-workflow-server` is the deployable workflow service. It hosts the
native manager over its platform schema, the workflow journal in the same
schema, and the maintenance lane that runs every job kind except creator code.
It introduces no broker service or separate workflow-worker executable.
Deployment infrastructure starts ordinary `zeroship-worker` processes; the
manager computes a capacity target per execution zone and hands it to an
injected provider.

A worker retains one batch claimer, execution heartbeats and bounded transport
retry. It has no cron evaluator, due-work scanner or maintenance scheduler, and
it never claims a sweep: reconciliation, collection, fanout, propagation,
activation, cron acceptance, management, hold release and closure run in the
service, beside the journal they touch. Waiting for a signal, child or deadline
releases the execution slot and isolate.

| Component | Owns | Excludes |
| --- | --- | --- |
| Control | Creator authorization, enrollment and execution zones, normal deployment publication, policy inputs, routing metadata and deployment retention APIs. | Journal access and workflow advancement loops. |
| Workflow service and manager | Calendar evaluation, durable jobs and deadlines, the zone claim, delivery attempts and give-backs, command delivery, recovery responsibility, per-zone capacity targets, the journal and its maintenance sweeps. | Customer code and creator database connections. |
| Worker | Pulling `advance` jobs of its own zone, preparing each claimed app on demand, executing pinned code under resource limits, and serving `env.workflows` for any app of its zone over the service. | Platform tables, the journal and scheduling discovery. |
| V8 adapter | Creator-facing handles and execution of the selected app deployment under resource limits. | Queue credentials or direct manager persistence. |
| Gateway | Normal routing and request authentication. | A workflow scheduler, journal reader or private workflow-advance control channel. |
| Deployment host | Starting workers in the proper execution zone and supplying trusted app context. | Deciding customer workflow transitions. |

The workflow subsystem checks app authority. Existing creator and app-request
authorization still applies at their entrypoints; the queue adds no end-user
permission model. A worker acts only for apps of its own execution zone, which
Control froze on its instance row; an app-scoped native handle cannot select
another app's journal rows, schema or object namespace. Follow the
[data-system contract](../architecture/data-system.md). Table naming and schema
visibility are not platform authorization: creator-owned rows cannot widen a
worker's zone, turn admission on or manufacture a platform deployment hold.

## Worker identity and zone-scoped pull

A worker asks for work; nothing is assigned to it. This is the pull model of a
Temporal task queue, with the zone's `advance` rows as the queue, the
service's zone claim as the matcher, the delivery lease and its heartbeat as
the liveness signal, and the app as the fairness key. Nothing records which
worker serves which app, and there is no sticky affinity: correctness never
depended on locality, because the journal drives replay, the pinned bundle is loaded by
hash, and every execution builds a fresh isolate.

```text
Deployment host          Worker                 Control            Workflow service
      |                     |                       |                      |
      |--- start ---------->|                       |                      |
      |                     |-- join token, key --->|                      |
      |                     |<-- instance id -------|  zone frozen on row  |
      |                     |                       |                      |
      |                     |---- ClaimJobs {max, wait_ms, after, exclude} --->|
      |                     |                       |  zone from the       |
      |                     |                       |  verified instance;  |
      |                     |                       |  page the zone's apps|
      |                     |<--- ClaimedJobs {deliveries, after, lap_complete}|
      |                     |                       |                      |
      |                     | prepare app on demand (Control app reads)    |
      |                     | execute in a fresh isolate                   |
      |                     |---- heartbeat / settle (journal decides) ----->|
```

| Authority | Record or capability | Meaning |
| --- | --- | --- |
| Enrollment | `zeroship.worker_instances`: the instance signing key, frozen execution zone and lease. | Control authorized this instance identity in that zone; the presented key must match an active, unexpired enrollment. |
| Zone | `VerifiedWorker::zone`, read by `WorkerRegistry::active_instance` (`crates/zeroship-workflow-server/src/auth.rs`) from the same row as the key. | The apps this worker may claim jobs for and make run calls for: exactly those whose frozen zone equals it. |
| Delivery | A leased `advance` job: `Delivery { job, worker_id, attempt, deadline }` with remaining lease and attempt durations. | This instance may process this job until its lease lapses or its attempt cap ends. |
| Creator execution fence | Journal task claim, generation and frontier revision. | This execution may commit the next customer transition. |

A worker uses its enrolled instance key for service assertions. The receiver
verifies issuer, audience, permitted endpoint, assertion lifetime and replay
protection before reading the buffered request. A worker ID in JSON is only a
selector, and a body never names a zone: the service takes both the worker and
its zone from the verified instance row. A generic worker-role signer cannot
substitute for an enrolled instance key. Every transport retry uses a fresh
assertion and the same durable operation identity.

**The claim.** `WORKFLOW_JOB_CLAIM` takes a closed `ClaimJobs`
(`crates/zeroship-core/src/workflow_jobs.rs`): the caller's free execution
slots, how long it will wait for the reply, the last app its previous claim
visited, and the apps it failed to prepare recently. It names no app. The
server (`claim` in `crates/zeroship-workflow-server/src/api/jobs.rs`) passes
the verified worker and zone to `Coordinator::claim_in_zone`
(`crates/zeroship-workflow-manager/src/coordinator/jobs.rs`), which pages the
zone's apps holding a claimable row in app id order after the cursor, takes at
most one job per app per lap, and keeps lapping while the previous lap delivered
something, until the slots are filled or its deadline passes. It starts no
per-app attempt after `workflow.claim_budget_ms` or after the request's wait
less the share kept back for the reply (`REPLY_RESERVE_DIVISOR`), whichever ends
first, and every give-back it makes ends by half that reserve later without
waiting for an app's lock (`Coordinator::claim_deadline`), so the reply leaves
inside the wait. The worker waits exactly the wait it stated: its client bounds
the claim exchange by `wait_ms`, not by its generic exchange bound. A request
above `ClaimJobs::MAX_DELIVERIES` deliveries or `ClaimJobs::MAX_EXCLUDE`
exclusions is refused before any work. A request without a cursor starts at a
worker-stable rotation of the first page, so workers spread across apps rather
than all starting at the same one. Per app it skips, naming the reason in its
`ClaimReport`: an unavailable policy observation, an app the zone rule refuses,
policy with admission or dispatch off or `max_running` zero, an app already at
`max_running` live leases (counted before the app's lock and again under it), a
scope another session holds locked, and an app with nothing claimable after
management barriers and the delivery ceiling. The reply, `ClaimedJobs`, carries the deliveries, the cursor the next
claim continues from, and whether this claim reached the end of the zone.

**Every claim re-authorizes.** The zone comes from the verified instance row and
is frozen there; the app's zone is frozen on its scope and its policy
observation; and `Queue::claim_authorized`
(`crates/zeroship-workflow-manager/src/queue.rs`) rechecks the worker's
enrolled key after taking the app's scope and again before commit. Freshness
checks belong around the work they authorize: authenticate at ingress, then
recheck enrollment after waiting for locks and before admitting the mutation to
commit, reusing the verified request context rather than consuming its
assertion replay identity again. An unavailable enrollment source is a
retryable infrastructure failure, not a durable customer rejection.

**The journal half rides the claim.** For each grant the service accepts the
job into its own journal (`admit` in
`crates/zeroship-workflow-server/src/api/jobs.rs`) before it builds the
reply, and only then measures each lease, so the authority the worker receives
is what remains after the service's own I/O. A grant the worker cannot use is
given back in the same request instead of being handed out: a journal deferral
returns the row to `ready` until the run is due, until the observation that
switched dispatch off lapses, or after a short pause when the app is at its
concurrency cap; a journal that could not be reached and a grant the journal
work exhausted are given back with the growing back-off. None of these fails the
batch, because an error reply would strand every delivery the request already
committed. The reply holds at most `ClaimJobs::MAX_REPLY_BYTES`, room for one
delivery at its largest, which both ends read: the first delivery is always
sent, and one that would pass the bound goes back unsent and ends the batch.

**The worker side.** One claimer per worker, not one per slot
(`JobConsumer` in `crates/zeroship-workflow-runner/src/consumer.rs`): it asks
for exactly its free slots, up to `ClaimJobs::MAX_DELIVERIES`, continues from
the cursor the previous reply returned, claims again at once when a reply did
not reach the end of the zone, and waits its idle interval only when a reply
reached the end without filling what it asked for - less if a slot frees
meanwhile, because the settlement that freed it may have published the run's
next job. A worker that gives up before a reply arrives strands what the service
committed for it: those deliveries are leased, their journal tasks hold a
`max_running` unit, and they lapse and redeliver after one lease window,
uncounted because they never renewed. So no stop cancels a pending claim: it
runs to its reply, and what it brings goes back unstarted
(`GiveBackReason::Unsent`).

On graceful shutdown the worker stops claiming, lets its running executions
finish and settle within its drain, cancels and releases what remains, then
retires its instance; see [shutdown](#shutdown-and-crash-recovery). A crash
expires delivery authority while durable work and scope responsibility remain,
and the instance's identity lease lapses on its own.

### Enrollment bootstrap and revocation

A worker joins with a SIGNED JOIN TOKEN it was handed, not with a standing
credential of its own. The trust anchor is a set of JOIN SIGNERS Control records
in advance: an issuer id, an Ed25519 public key, and the execution zones that
signer may mint for. One signer covers many deployment units, so adding a unit
stops being a Control-side operation, and the signing key sits with whoever
decides a worker should exist rather than on the machine that runs creator code.

Control learns signers from its own configuration, on the discipline the
operator import already had: it inserts signers it has not recorded, never
reactivates a revoked one, and refuses the whole file, writing nothing, when any
entry disagrees with what is recorded - a recorded id under another key, a
recorded key under another id, or a different zone set. A signer's zones are its
authority, so widening them is provisioning a new signer, not editing a line.

A JOIN TOKEN is a JWT the signer mints. `iss` names the signer, `aud` is the
control plane, `exp` bounds it, `jti` identifies it, `zone` names the execution
zone it admits into, `uses` says how many workers it may admit, and `cnf` carries
the joining key's thumbprint when the issuer knows that key in advance. A scaled
service shares one token and each use is consumed separately.

A worker generates its instance keypair in memory at boot and posts the token,
the public half and its listening port, with the request itself signed by that
new key. Control checks, in this order: the token's signature under the trusted
signer key; the audience; expiry under the existing skew bounds; that `zone` is
one that signer may mint for; one use of that `jti`, consumed atomically, where a
token with uses remaining is not spent and an exhausted one is refused; the
request's self-signature, which binds the presented public key and must equal
`cnf` when `cnf` is present; and the advertise address, derived from the observed
peer and held to the operator's enrolment envelope. Then it mints the instance
identity exactly as before, idempotent on the instance public key, so a
lost-reply retry returns the row the first attempt committed. The zone is the
token's claim and never a field of the request, and there is no such field to
add.

Proof of possession is what bounds a captured token. Presenting one without the
matching private key proves nothing, so a captor can admit workers it controls
and nothing else: up to the uses that remain, until the expiry, in one zone.
`cnf` removes even that, because a token minted for a known key admits only that
key.

The instance row records which signer and which token id admitted it, so "who
vouched for this worker" is a stored fact rather than an inference. The
execution zone is recorded on the instance as well, because there is no longer a
per-unit row to read it from.

Revocation is not configuration, and a signer has TWO operator verbs that do not
imply each other. ROTATE removes the signer's key: no further token can be
minted under it, outstanding tokens die at their own expiry - which is what a
short `exp` buys and what a long one gives up - and the fleet that signer
admitted keeps running. That is the hygiene path, and it must not retire the
fleet, because one signer covers many units and retiring a key should not take
all of them down. PURGE removes the signer AND retires every instance it
admitted, in one transaction: it is the incident path, for a key believed to
have leaked, where an operator must not be retiring instances one at a time
while an attacker's workers keep serving. The recorded signer id on the instance
row is what makes that set enumerable. Rotate when the key is merely old; purge
when you believe it leaked.

Retiring one instance is unchanged - a worker that exits gracefully retires
itself once its server has drained - and observed liveness never writes
enrollment status.

**The instance identity expires, and the worker renews it.** An instance is
admitted with an expiry, and Control's instance verification refuses an expired
instance exactly as it refuses a retired or revoked one. Revocation therefore
stops being the only way a credential ever stops working: an abandoned worker's
credential dies on its own, and a crashed worker's row stops being live without
anyone sweeping it. Nothing observes liveness to make that happen; the row
simply stops satisfying the read.

Renewal is authenticated by the INSTANCE KEY and by nothing else. No join token
is involved, and requiring a fresh one would defeat the point of a use-capped
token: the worker proved possession of its key at join, and that proof is what
renewal rests on. Control extends the expiry only for an instance that is
active, unretired and not already expired, so expiry is terminal in the same way
retirement is - a worker that let its identity lapse rejoins, which needs a
token, rather than reviving a row.

The worker renews on a schedule DERIVED from the lease, not on a constant chosen
beside it. The renewal interval is the lease divided by a stated factor, so
several attempts fall inside one lease and a renewal that fails is retried well
before the identity lapses. The two quantities move together by construction;
they are not two settings an operator can put out of order.

What this replaces is one sentence: "an instance row is live until an operator
says otherwise". The instance lease is also the only liveness record a worker
has: there is no workflow registration, and what lapses when a worker stops is
each delivery lease it held, which any other worker of the zone may then claim.

Every enrollment reader reads the authoritative row: Control on each internal
request, the workflow service at ingress and again after lock waits and before commit,
and the CDC relay on its session recheck. Revocation therefore stops new
admissions at the next check; leases already issued keep their original
deadlines while creator fences stay authoritative. An unavailable registry is a
retryable infrastructure failure. Local development composes a trusted
in-process worker and performs no enrollment.

**Where the signer key lives, and who mints.** `zeroship dev init` provisions the
OPERATOR SIGNER and nothing worker-side: it generates the keypair, leaves the
private half where the operator runs the CLI, and records the public half and
its permitted zones in Control's trusted-signer configuration. There is no
worker-side credential to write, and no long-lived bearer file anywhere - a
standing token mounted into every worker would be the shape this change exists
to remove, wearing a different name.

A single-host deployment (compose, `deploy-remote.sh`) makes CONTROL THE MINTER
for its own zone. Control mints a short-lived, zone-scoped, use-capped token at
startup and again before that token expires, and writes it to a path the worker
containers share, owner-only. A worker reads the CURRENT token at boot, so a
container restarted days after provisioning gets a token minted minutes ago
rather than one minted at install time.

Three quantities hold that together and they are related, not independent. The
rotation interval is how often Control replaces the file. The TTL is how long a
token stays valid. The boot margin is the longest a worker may take between
reading the file and presenting what it read. TTL must exceed the rotation
interval by at least the boot margin, or a worker that reads the file an instant
before rotation presents an expired token. Because a JWT already minted stays
valid until its own `exp`, rotation overlaps by construction: the token a worker
read before the file changed is still live for at least `TTL - rotation
interval`, which is why that difference is the quantity the boot margin has to
fit inside. The file is replaced atomically, so no worker ever reads half a
token.

Minting is a LEASED ROLE, not something every replica does. Deployments run
several Control replicas against one database, and two of them rotating the same
volume would write over each other. The minter is elected with a database
advisory lock: the holder rotates, the others stand by, and a holder that dies
drops its lease with its session so the next tick elects a successor. A replica
that is not the minter writes nothing at all.

**Concurrency, because there is more than one Control.** Consuming a use is a
guarded write, never a read followed by a write, so a token with N uses admits
exactly N workers however many present it at once. The instance public key is
UNIQUE, so two replicas racing a lost-reply retry converge on one instance row
rather than minting two identities for one key. Renewal extends monotonically -
it takes the later of the recorded expiry and the new one - so a slow replica's
in-flight renewal cannot shorten a window a newer one already extended.

The signer import converges for a different reason: it only ever ADDS, so any
order of replicas reaches the same recorded set. A configuration that
CONTRADICTS what is recorded refuses only the replica that read it, which during
a rolling deploy means replicas fail one at a time with a message naming the
offending entries, rather than a fleet that half-believes a new file.

**What the shared file is, stated plainly.** It is a bearer artifact: whoever can
read that volume can join a worker in that zone, for the TTL, up to the uses that
remain. Rotation bounds the window and the use cap bounds the blast radius, but
the trust boundary is "whatever can read the volume", which is not the same
boundary as "the worker". The stronger anchor for a single host is for Control
to read the peer's credentials off a Unix domain socket and mint nothing at all,
which removes the artifact rather than shortening its life. That is the named
follow-up. This change does not do it.

`zeroship join-token --zone --ttl --uses` stays for multi-host deployments,
signed by the operator's own signer, defaulting to minutes, and minted per
provisioning rather than mounted as a standing file.

**Implementation boundary:** a worker reads a join token
(`worker.join_token_file`) at boot and holds nothing else on disk - no signing
key, no `svc/worker` role key, and the peer document publishes none. Control
refuses a `svc/worker` assertion minted at role arity. Control imports signers at
startup from `control.join_signers_file` on the discipline above, and verifies
the join token itself rather than routing it through the service-assertion
allowlist: a join token carries its own `typ`, so it can neither be presented as
a service assertion nor accept one in its place. Control's minter is configured
separately (`control.join_token_signer_file`, `control.join_token_file`,
`control.join_token_zone`) and is inert when unset, so a multi-host Control
verifies without holding a signing key. A joined worker renews its own instance
through `CONTROL_WORKER_RENEW`, a grant `svc/worker` holds at instance arity and
which takes no selector, so no worker can renew another's identity.
`zeroship dev init` provisions the
signer credential and the import file; `zeroship join-token` mints from that
credential; compose mounts the import file and the signer credential into
Control and the minted-token volume into the workers; and
`docs/runbooks/worker-join-signers.md` holds the operator procedure with the two
signer verbs side by side and instance retirement. A gracefully stopped worker retires
its own instance through `CONTROL_WORKER_RETIRE`. The worker's version poll still
authenticates with the shared control key rather than a worker credential
(`version_poll_authorization` in `crates/zeroship-worker/src/sync.rs`), so revoking
a signer does not take that credential from a process that already holds it; the
poll is on the POLLED tier that `docs/proposals/2026-09-05-app-metadata-distribution.md`
replaces. Every workflow call, `env.workflows` included, is signed with the
instance key through `WorkerCoordinator`. The instance's zone is recorded and
foreign-keyed to `zeroship.execution_zones`, and the workflow service reads it
beside the key on every authenticated call (`WorkerRegistry::active_instance` in
`crates/zeroship-workflow-server/src/auth.rs`).

### Zone eligibility and capacity from backlog

Each app belongs to exactly one execution zone, named by Control in
`zeroship.apps.execution_zone_id` when the app is created and frozen by the
`apps_frozen_execution_zone` trigger
(`db/migrations-ts/20260914000600_app_execution_zones.ts`). An execution zone is
an operator-declared set of deployment units that share creator-side
connectivity. Control names an app's zone rather than letting a column default
decide: it resolves the zone the creator asked for, or the deployment's one
declared zone when none is named, and refuses to create an app in a deployment
that declares several without saying which. A worker's zone is the `zone` claim
of the join token Control verified when it joined, recorded on the instance and
frozen there. Nothing a worker sends can change either fact.

The service learns an app's zone twice, from Control and never from a worker.
Every register, activate and disable message Control's lifecycle publisher
delivers names it (`RegisterSchedules`, `ActivateSchedules`, `DisableSchedules`
in `crates/zeroship-core/src/workflow_schedules.rs`), and `register_scope_in`
(`crates/zeroship-workflow-manager/src/queue.rs`) records it on the app's
`queue_scopes` row, refusing a later message naming another zone. Control's
app facts carry it as well (`AppSourceFacts::execution_zone_id` in
`crates/zeroship-core/src/workflow_app_facts.rs`), so every `PolicyObservation`
(`crates/zeroship-workflow-manager/src/policy.rs`) retains the app's zone and
its deletion marker from the same source read as its policy. Both copies can be
cached because neither moves: the zone is frozen and deletion is terminal.

**One zone rule serves both paths.** `PolicyObservation::admits_zone` refuses a
deleted app to every zone and serves a live app only to its own zone. The run
path (`bind` in `crates/zeroship-workflow-server/src/api/runs.rs`) calls it with
the verified worker's zone before binding the journal, so start, status, signal,
transition, restart, step output and output are served for any app of the
caller's zone, whether or not that worker ever ran one of its jobs, and refused
for any other. The claim path calls it for each app it visits. A run call issued
after an app is deleted is refused within one observation's validity, the same
bound as any policy revocation.

Observing an app reads Control and writes the policy ledger, and Control answers
an unknown app differently from a known one, so the run path fences on the queue
first: the app must hold a queue scope in the caller's zone
(`Queue::require_scope_in_zone`), the queue's own frozen copy of the app's zone.
An app with none - unknown, or of another zone - is refused `PermissionDenied`
with no Control read and no ledger row, the same answer either way.

Control's host app reads are narrowed to the calling instance's zone as well.
The app metadata, environment, data-key and binding endpoints verify which instance
signed the call and answer only for apps in that instance's zone
(`zone_scoped_app_read` in `crates/zeroship-control/src/internal.rs`, through
`instance_serves_app` in `crates/zeroship-control/src/worker_join.rs`), because
an app's environment is its decrypted secrets and its data key is a decryption
capability. The workflow reach of a worker therefore equals the credential reach
its zone already grants; see the [trust statement](#observability-and-trust-limits).

**Capacity follows each zone's backlog.** `capacity_targets` holds one
declarative target per execution zone, in execution slots, written by the
driver's capacity lane (`capacity::Capacity` in
`crates/zeroship-workflow-manager/src/capacity.rs`) and upserted whenever a
scope is registered in a new zone. Each visit reads the zone's apps that hold
an unsettled creator row, an idle scope costing nothing, through the lane's own
policy source - the server composes one for the driver whether or not it runs
its maintenance lane - outside any transaction lock. An
admitted app's demand is its live `advance` leases plus as many claimable rows
as its `max_running` leaves room for, and the claimable count runs the claim's
own candidate query, so demand and claimability cannot disagree. Rows exhausted
by `max_delivery_attempts`, rows inside a give-back back-off and every row of an
app whose policy withholds dispatch or which Control deleted never count toward
demand; a complete visit records them on the target row as `exhausted_jobs`,
`backed_off_jobs` and `withheld_jobs`, beside `backlog_depth` and
`oldest_available_at`. The target is computed whether or not any worker is
claiming, so it does not oscillate at a zero floor.

The target's revision advances only when the desired slots change. Operator
configuration bounds it between a floor and a ceiling and sets a hold-down: a
rise applies at once, a fall only after the zone's demand stayed below the
target for the hold-down. A visit cut off by its deadline leaves the zone's cycle
where it stopped, and the next visit continues it, so a zone too large for one
visit is measured over several; a cut visit may raise the target and never
lowers it, and the visit that reaches the end completes the cycle, whose
census has measured every app once and may lower it too. An unavailable policy
observation freezes the target. The injected `CapacityProvider` receives
`CapacityRequest { zone, revision, desired_slots }`, a function of the revision
alone, and answers `Accepted` or a closed, durable, retryable refusal
(`pool_exhausted`, `no_signer`, `unavailable`); a reply applies only to the
revision it answered. A request is sent when the target changes, while a
request is outstanding, and again after the retry interval as an idempotent
resynchronization. Provider failure keeps jobs and the target pending.

Scale-down is the orchestrator's. A lower target names no instance: the
provider lowers its units, the orchestrator stops workers, and each stopping
worker drains as [shutdown](#shutdown-and-crash-recovery) describes. Work cut
off by a termination grace shorter than the drain runs again on another worker
of the zone once its lease lapses, and the creator fences keep that
at-least-once delivery safe.

A provider holds only scale authority over worker units in one zone. It never
receives creator credentials, secret-mount authority or queue messages. The
local host injects `LocalCapacity`, which accepts every target, for its trusted
in-process worker. A deployment whose workers are started outside the platform
uses `StaticPool`, which refuses exactly a target above its configured
`workflow.static_pool_slots` and starts nothing.

A workflow-only app - one that runs workflows and applies no creator
migration - is provisioned with the same creator database objects the apply
path would give it: its schema and migrator role, and the per-app runtime role
its host opens that database under.

**Implementation boundary:** providers that start processes wait for the
production orchestrator; `LocalCapacity` and `StaticPool` ship. The gateway's
`HashRing` (`crates/zeroship-gateway/src/proxy.rs`) selects among one static
worker list, so a deployment with more than one execution zone must route each
app's requests only to its own zone's workers; the service refuses a run call
from a worker of another zone, and Control refuses that worker the app's
environment.

## Policy bindings and the service-side binding

**Implemented.** The creator engine accepts trusted `PolicySnapshot` values
through immutable `PolicyBinding` capabilities and ordered `PolicyRefresh`
tickets. App handles, queued backend calls and delivered execution retain the
original authority across asynchronous work. The workflow service is the one
production host that binds policy: it observes each app's policy over Control's
facts capability and installs it into its own registry on every call that
reaches the journal. Workers hold no policy binding and receive no policy.

### Native binding identity and policy revision

Keep these identities independent:

| Identity | Authority and change rule |
| --- | --- |
| Source policy revision | The policy ledger's monotonic revision for the complete effective app policy. A revision identifies immutable policy values. |
| Host binding generation | An opaque, process-local identity allocated by the trusted host when it replaces an app binding. It is unrelated to run generation or policy revision. |
| Refresh ticket | A local ordering token for an install under a particular binding. It grants no journal authority by itself. |
| Ingress epoch | The manager's recovery responsibility the binding carries, obtained by establishment and fenced by the journal's closed epoch at use time. |

`HostPolicies` issues a `PolicyBinding` capability containing its registry identity,
app identity and opaque generation. Explicit replacement retires the previous
generation before the replacement can admit work. Revocation retains a tombstone;
neither a late refresh nor a clone of the retired capability can recreate it.
An operation through a retired capability also cannot revoke its replacement.
Generation and ticket identities must not wrap or be reused. The customer engine
does not store these capabilities in creator tables or reconstruct them from SQL.

Construct `AppWorkflows` with `WorkflowService::register_app(&binding)` after
installing a snapshot, or use `bind_app(&binding)` for an existing journal. An
Activate job enters its app into the journal inside its own first transaction,
so a host whose apps arrive by activation, as the workflow service does, binds
with `bind_app` alone. The handle retains that exact binding; cloning it does
not resolve current authority by app ID. `AppBackend`, runtime contexts and
queued backend calls preserve the same capability. A retained handle cannot
start a fresh mutation by borrowing a replacement's policy. An app selector in a
signal capability likewise cannot create a binding; the trusted ingress host
selects an existing authorized handle.

Keep policy revision and content high water across local binding replacement and
revocation. Installing a lower source revision fails; changed policy under an equal
revision conflicts. Equal revision with identical values is valid for a newly
authorized binding, which permits unchanged policy to serve a replacement without
allowing the retired handle to refresh itself. After a process restart, the host
must obtain fresh trusted authority; customer state and earlier serialized
metadata cannot restore a binding.

Local configuration uses the same capability lifecycle with an explicitly
nonexpiring configuration snapshot: the local CLI host installs its configured
`AppPolicy` once. Leased and configured authority are distinct binding modes.
Missing or expired leased authority never falls back to configured defaults.
Refreshing the current binding is different from replacing it, so normal refresh
does not force healthy handles to acquire a new local identity.

### Ordered refresh and captured operations

Create a refresh ticket before installing. It captures the current binding
generation. Beginning a newer refresh supersedes older tickets. Installation
atomically verifies the current binding, ticket, policy revision/content and
remaining validity, then consumes the ticket. Ticket comparison and snapshot
installation occur under the same host-state synchronization; a ticket already
consumed or superseded cannot apply a snapshot.

```text
Host binding                 Install                           Journal handle
     |                          |                                    |
     |-- capture refresh ticket |                                    |
     |-- replace / revoke binding                                    |
     |                          |                                    |
     |<-- delayed install ------|                                    |
     |    reject retired ticket |                                    |
     |                          |                     old mutation --|
     |<---------------------------------- check retained binding ----|
     |----------------------------------- unavailable; no new authority ->|
```

Within the current binding, a fresh install with unchanged policy may extend the
deadline for subsequent operations. A newer accepted install may also shorten it.
An older one must not reverse that shortening, restore a prior policy or undo
revocation. Cancellation and replacement still require the ticket fence because an
outstanding install can outlive its initiating call.

Expiry alone does not replace a binding. A fresh observation may admit new
operations under the same still-current binding after an earlier snapshot
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
acknowledgement use their own current metadata authority; reading a journal
receipt alone does not authorize a queue write.

### The service-side binding

`RunService::app` (`crates/zeroship-workflow-server/src/runs.rs`) is where a
binding is made and refreshed. It observes the app's policy from the trusted
`PolicySource`, takes the app's current binding or binds a new one, and installs
the observation as a lease snapshot whose deadline is the observation's own
validity. Installing on every call is deliberate: an unchanged revision with an
unmoved deadline is inert, so a reinstall does not disturb operations already in
flight, and there is no revision to read back and compare. The observation says
nothing about recovery responsibility, so the binding's ingress epoch is carried
forward across the reinstall rather than reset by it, and `require_open_epoch`
rechecks the journal's closed epoch inside each mutation, which fences a
carried-forward epoch the moment the journal closes it.

Every route that reaches the journal binds through that call - the run calls
after the zone rule, the claim for each grant it accepts, the heartbeat and
settlement for the delivery they name - so the service re-applies current policy
to each delivery exchange. A heartbeat under policy with admission or dispatch off
extends nothing (`AppWorkflows::heartbeat_job` in
`crates/zeroship-workflow/src/service/delivery.rs`), and the worker turns that
into an interruption at that renewal. The journal's own dispatch check,
`tasks::assign` in `crates/zeroship-workflow/src/service/tasks.rs`, refuses a
task with admission or dispatch off, with `max_running` zero, or at
`max_running`, and the claim gives the row back with the matching
`DeferredReason`. Observing an app is policy I/O whose refusals differ by whether
the app exists, so each route first proves against the queue that the caller
holds the delivery it names, or for the task routes a live delivery of the app,
before the policy source is asked.

### Authoritative source

Control owns the complete effective policy, including app lifecycle, entitlement,
operator switches and applicable limits. Its policy revision must describe a
consistent observation of all contributing values. Changes that affect policy
advance that revision atomically with their authoritative state, or use an
equivalent durable revisioned projection whose original source validity is explicit.
A content hash without ordered source authority cannot distinguish a delayed
observation from a new desired state.

The service obtains this policy through a trusted provider in the platform zone.
The provider consumes authenticated Control facts; it never reads a creator
database. A `PolicyObservation` contains the exact app, immutable source
revision and values, the app's frozen execution zone, its deletion marker, and a
finite original validity bound. Cached values retain that bound. Repeated
requests, service restart or rereading an unchanged projection cannot refresh
source authority. Only a new authoritative source observation may issue a new
validity bound. Unknown source freshness, inconsistent revisions and unavailable
source storage fail closed with retryable infrastructure failure.

`zeroship_workflow_manager::policy::PolicySource` expresses this trusted provider
contract. `PolicyObservation` validates the raw policy, retains its original
monotonic deadline and carries an opaque observation identity. Cached reads clone
the retained observation. Equal values, revision and deadline do not make a new
observation identical to an invalidated predecessor. The provider's nonblocking
revalidation must reject that predecessor permanently across shortening,
revocation and restoration.

`policy::control::ControlPolicyStore` holds Control's app-facts capability and
this service's own publication schema. It binds no `zeroship` schema: the app and
plan facts arrive over `POST /v1/app-facts`, and the only `zeroship` read left in
the service is the worker registry lookup that authenticates each call. It reads
no creator database. Its inputs are:

| Input | Authority and contributing writers |
| --- | --- |
| `AppSourceFacts` - plan id, workflows enabled, archived, deleted, execution zone | Registry app lifecycle and plan changes, billing plan-change transactions, and operator enablement, served by Control. The database constraint makes deletion imply archive, and the zone is frozen by trigger. |
| `PlanSourceFacts` - the canonical policy | A complete canonical `AppPolicy`, written by the operator `set_plan_policy` API on `PlanPolicyStore` or equivalent operator provisioning. Missing or malformed JSON is unavailable, including for a disabled plan. |
| `PlanSourceFacts` - entitlement and archival | Operator entitlement and catalog archival. Pricing updates omit the workflow policy column; startup seeding preserves archival and workflow authority. |
| `workflow_manager.workflow_rollout_config` | The required global row contains dispatch/ingress switches and positive `source_validity_ms`. The `set_rollout` operation on `ControlPolicyStore` writes these together. Missing settings never select defaults. |

The publication transaction explicitly requests read-committed isolation. It
first performs an ID-only upsert on `workflow_policy_ledger`, creating an
unpublished row or locking the existing publication without resetting it. Only
after that wait does it read the rollout row on the publication handle and
observe the app and plan facts over the capability. Those are two authorities
rather than one snapshot, and what orders them is `SourceWatermark`: `publish`
refuses an observation carrying one below the watermark the ledger already holds.
It validates the complete policy, masks enablement and operator
switches, and publishes changed policy or source validity under an advanced
revision. Unchanged values preserve the revision; the zone and the deletion
marker move no revision, because the zone cannot change and deletion requires an
archive that already masked admission. The ledger retains its app ID as the sole
primary key and has no cascading app deletion. Its unpublished state cannot
issue authority. The host refuses unsupported isolation instead of silently
substituting a different transaction contract.

Source validity begins before acquisition and ends at that original instant plus
the observed `source_validity_ms`; publication and commit waits consume it. Only
successful settlement produces a `PolicyObservation`. A ledger row alone is not
a renewable source: every refresh rereads the contributing authoritative inputs.
The ledger orders observations, not every intermediate writer transition. An
unobserved disable followed by restore need not change its revision. Writer
acknowledgement therefore promises bounded convergence, never immediate
revocation or execution quiescence.

`ControlPolicies` caches exact observations until their original expiry. A
per-app refresh reservation prevents competing requests from independently
refreshing the same entry; callers arriving during that read receive a retryable
infrastructure failure. Cancellation, timeout, source failure, invalidation or
capacity eviction removes the reservation. Opaque entry identity prevents a late
completion or its cleanup from replacing a later entry. Revalidation accepts
only the current exact observation and performs no I/O. Cache capacity is bounded
by `workflow.policy_cache_entries`. The server composes one provider for each HTTP
thread, another for its maintenance lane, and another for the driver's capacity
lane (`connect_policies` in `crates/zeroship-workflow-server/src/server.rs`).

### Archive acknowledgement and verification

Calendar disable acknowledges the manager's durable calendar fence. Policy
publication acknowledges a desired source revision. Neither result proves that
running executions observed revocation, stopped creator mutations or joined.
Control must not report execution quiescence from either acknowledgement.

With finite observations, revocation reaches execution within bounded time: the
next heartbeat of an execution under revoked dispatch extends nothing and
interrupts it, and a worker partitioned from the service loses its delivery
lease at the lease's deadline without renewing it. Expiry is an eventual
admission fence, not an acknowledgement from an unreachable worker or proof that
a dispatched commit rolled back. An explicit quiescence acknowledgement requires
a separate protocol that accounts for affected executions, joins their active
work and resolves uncertain outcomes. That protocol remains open; archive
responses must distinguish desired state, manager acknowledgement and any later
verified quiescence result.

Native regressions cover retired `AppWorkflows` and `AppBackend` handles
starting fresh calls, equal-policy-revision replacement, delayed refresh tickets,
shortened grants, revocation during lock waits, and exact receipt replay without
new authority. The service's own contracts cover the carried-forward epoch
(`an_established_epoch_survives_the_next_requests_policy_reinstall` and
`closing_the_journals_epoch_retires_the_carried_forward_one` in
`crates/zeroship-workflow-server/tests/integration/http_runs.rs`), a dispatch
switch observed by a claim (`a_claim_finding_dispatch_off_waits_out_the_observation`
in `tests/integration/http_claims.rs` of the same crate) and an interruption at
a renewal that extends nothing
(`a_renewal_that_extends_nothing_interrupts_the_execution_at_that_renewal` in
`crates/zeroship-workflow-runner/src/delivery/tests.rs`).

## Storage inventory and schema ownership

### Platform metadata

The physical manager schema is generated by
[`workflow-manager/schema/schema.ts`](../../crates/zeroship-workflow-manager/schema/schema.ts).
Rust models declare their ORM metadata natively in
`crates/zeroship-workflow-manager/src/models/schema_definition.rs`; parity tests
compare those declarations with the migration artifact.
The production namespace is `workflow_manager` in the platform database, the
same schema the workflow journal is installed into.

| Table | Stored authority and purpose |
| --- | --- |
| `workflow_manager.schema_version` | Generated schema fingerprint. Runtime roles read it; provisioning owns changes. |
| `workflow_manager.queue_scopes` | One row per app: its frozen execution zone, indexed with the app id for the zone claim's page; the lock every queue change for the app takes; and the persistent dispatch cursor. Control's lifecycle messages and recovery activation register it, and a message naming another zone is refused. |
| `workflow_manager.capacity_targets` | One declarative target per execution zone in execution slots: its revision, desired slots, provider state, closed refusal, retry pacing and hold-down start, and, from the last complete visit, `backlog_depth`, `oldest_available_at`, `exhausted_jobs`, `backed_off_jobs` and `withheld_jobs`. |
| `workflow_manager.management` | Job-linked authorized command, original request provenance, per-run revision, provisional execution barrier and reported closed outcome. |
| `workflow_manager.management_scopes` | Accepted and settled management revisions per app/run; independent of the creator's run existence. |
| `workflow_manager.jobs` | Immutable job specification, checked operation/run/request projections, availability, dispatch ticket, current attempt and holder, lease deadline, `leased_at` (the start the attempt cap measures from), counted executions, `deferred_until` and the consecutive `deferrals` of a give-back back-off, and settlement digest/outcome. It also supplies submission and settlement deduplication. |
| `workflow_manager.recovery_scopes` | Trusted activation provenance and revision for durable maintenance responsibility, its ingress epoch and state, the current closing attempt's watermark and Close job, its latest activity and closing pacing. The row outlives retirement and abandonment as the epoch's tombstone. |
| `workflow_manager.recovery_duties` | Independent app/kind deadlines and retained pending reconciliation or collection jobs, linked to their owning scope and queue. |
| `workflow_manager.schedule_deployments` | Immutable allowlisted schedule descriptors and the calendar interpretation for a normal deployment. No business input. |
| `workflow_manager.schedule_activations` | Stable activation job, deployment, app revision and activation instant. Job settlement determines dispatch readiness. |
| `workflow_manager.schedule_disables` | Historical Control disable commands in the shared activation revision sequence. Exact replay cannot change a newer scope state. |
| `workflow_manager.schedule_scopes` | App lifecycle revision, calendar-enabled state and optional selected activation; historical receipts remain independently replayable. |
| `workflow_manager.schedules` | Logical schedule identity across deployments, active descriptor and persisted due/catch-up frontier. |
| `workflow_manager.schedule_occurrences` | Stable occurrence request, run and job identities bound to a schedule revision, instant and activation prerequisite. |
| `zeroship.worker_instances` | Control-owned enrollment, public key, frozen execution zone, admitting signer and token id, identity expiry and revocation state. The workflow service reads the id, status, key, zone and expiry it authenticates each call with, through column grants. |
| `zeroship.execution_zones`, `zeroship.worker_join_signers` | Control-owned zones and the trusted signers that may mint join tokens for them. |
| `zeroship.app_deploys` | Control-owned immutable deployment metadata and reclamation state. Out of the workflow role's reach: Control resolves deployments and names them on the wire. |
| `zeroship.app_deploy_holds` | Control-owned app/deployment/holder generation and retention state. |
| `zeroship.apps` | Control-owned lifecycle and each app's frozen execution zone. The workflow role holds no grant on it: the policy inputs, the zone and the deletion marker reach the service over Control's app-facts endpoint. |
| `workflow_manager.workflow_rollout_config` | Operator dispatch/ingress switches and the finite source-validity bound. |
| `workflow_manager.workflow_policy_ledger` | Durable per-app ordered policy publication. The service locks and updates this row, without writing its app or plan inputs. An unpublished row grants nothing. |

The capacity census lives on `capacity_targets`, beside the queue it describes,
and give-back state lives on the job row it delays. Prefer extending the owning
manager models over adding another store; an in-memory map is not a substitute.

A delayed job's `available_at` is sufficient for a simple timer. Separate timer
rows are warranted only where calendar cursors, cancellation or coalescing need
additional durable state. Do not create a scheduler database alongside the queue.

### Creator journal

The journal is generated by
[`workflow-schema/schema/schema.ts`](../../crates/zeroship-workflow-schema/schema/schema.ts).
Its Rust ORM models use native declarations in
`crates/zeroship-workflow/src/service/models/schema_definition.rs`, with migration
metadata parity checked by tests. Runtime model construction does not read the
generated JSON artifact.
The journal is installed into the workflow service's `workflow_manager` schema by
`db/migrations-ts/20260919000000_workflow_journal.ts`: one journal for every app,
with its `app_id` columns as the tenant discriminator, reached only by the
service. Every name in this table has the `__zeroship_workflow_` prefix.

| Table suffix | Customer-owned content and target treatment |
| --- | --- |
| `schema_version` | Journal schema fingerprint, installed by the platform migration. |
| `app_state` | App serialization, journal counters, the highest ingress epoch a delivered Close fenced, and the app's two paged sweeps. Trusted admission policy remains outside customer SQL. |
| `deploys` | Locally accepted immutable app deployment and availability state. |
| `deployment_holds` | Customer dependency intent and observed hold generation; not the platform hold ledger. |
| `activations` | Immutable readiness per manager activation job, app revision and deployment; committed with the logical job receipt. |
| `activation_scopes` | Highest manager activation revision selected for new work. Older readiness remains independently replayable. |
| `runs` | Run identity, lifecycle state, current generation, relationships, the logical frontier revision a dispatch is authorized at, and the journal revision its replay history stands at. Both are independent of the task lease epoch, and of each other: the frontier revision is pinned for the life of one dispatch's authorization, while a durable wait settled against the database clock moves the journal revision with nothing published. |
| `generations` | Pinned deployment, input, output/error references and generation lifecycle. |
| `steps` | Replay history, checkpoints and compensation state. |
| `tasks` | Customer execution claims, frontier and lease fences, the journal revision the dispatch was minted against, job/delivery identity and exact task completion receipts. |
| `waits` | Recorded sleep, signal and child waits and their execution scope. |
| `topics`, `broadcasts`, `signals`, `subscriptions` | Customer event bodies, accepted/completed topic ordering, app-scoped signal delivery order, targets, subscription state and fanout cursors. |
| `fanout_pages` | Exact delivered broadcast page, committed cursor transition, semantic outcome and successor specifications linked to the retained job receipt and publication. |
| `propagations` | Dependency propagation obligations: kind, source run and generation, cursor, next page revision and finished state. An unfinished cascade obligation fences its source generation's cascading children. |
| `propagation_pages` | Exact delivered propagation page, cursor transition, closed result and successor specifications linked to the retained job receipt and publication. |
| `requests` | Durable app-operation request identity, body digest and original result. Age alone cannot retire an accepted request. |
| `management_receipts` | Exact delivered job identity, requested run, management revision and durable lifecycle outcome, independently scoped from app requests. Its app/run/revision uniqueness carries the run's applied revision as the highest it holds, so the ordering fence reads the history itself. |
| `schedules`, `occurrences` | Existing customer schedule definitions and accepted occurrences. Calendar discovery belongs to the manager; customer acceptance, overlap state and input references remain customer-side. |
| `payloads`, `payload_refs` | Prepared upload metadata, ownership, integrity and committed references. |
| `outbox` | Customer events and their payloads; distinct from manager queue metadata. |
| `job_publications` | Closed immutable Advance, Fanout or Propagate specification and manager confirmation time. The row's key is derived from the work the specification names, so the primary key is also the deduplication key. Publications survive history removal. |
| `job_receipts` | Immutable logical job specification and committed semantic outcome, retained independently of run history and delivery attempts. Its run identity is present only for the kinds that name a run. |
| `collection_pages`, `reconciliation_pages` | Immutable bounded plan and reserved item offset for a paged sweep, scoped to its logical job receipt. |

The two app-wide sweeps are columns of `app_state` rather than tables of their
own. `reconciliation_*` holds the scan revision, publication/hold phase,
ordering cursor and captured upper boundary; `collection_*` holds the collection
revision, expiry cutoff, ordering cursor and captured upper payload identity.
Each is a per-app singleton read and compare-and-set under the lock the sweep
already holds, and neither schedules work nor grants ingress authority.

### How a job kind extends its receipt

A column lives on `job_receipts` if and only if code that has not yet determined
the job's kind reads it. All kind-specific state lives in that kind's own table,
keyed `id` (the job id), with `(app_id, id)` unique and a foreign key to
`job_receipts(app_id, id)`. A kind with no extension state gets no table; that is
the rule returning zero columns rather than an exception to it.

The rule exists because the alternative cannot be adopted. Folding kind state
inline would make `fanout_pages` and `propagation_pages` part of `job_receipts`,
and both carry a foreign key into `job_publications` on columns that are never
null, so the constraint could not be skipped for the kinds that are never
published. Inlining would have to delete two foreign keys that both dialects
enforce today, replacing a write-time refusal with a read-time validator on a
journal that creator code can reach.

Both job tables answer this question the same way. `reconciliation_pages` holds
the reconciliation sweep's plan and reserved offset in the shape
`collection_pages` already stores, so `job_receipts` keeps only `run_id`: the one
column a kind-blind scan reads. `job_publications` keeps no kind-specific column
at all, because a publication intent has no kind-specific state to keep: its
identity is its key, and the specification it stores is what a reader decodes.
Neither table carries a validator that matches every operation to assert which
columns are null.

Publication intents use dedicated journal records whose key is derived from the
work they name: `publication_id` in `crates/zeroship-core/src/workflow_jobs.rs`
binds an Advance to its deployment, run, generation, frontier revision and due
time, a Fanout to its broadcast and page revision, and a Propagate to its
obligation and page revision. `id` is the sole primary key and therefore the
deduplication key, and a reader re-derives it from the specification it decoded
rather than comparing a second copy of those columns. A transition that advances
an idle run invalidates its older
frontier without retargeting already committed jobs. Task claim, heartbeat and
release do not advance that logical revision. A new generation starts a fresh
frontier. Advance-job acceptance binds journal claims to the logical job and
delivery attempt. Superseded frontiers produce a durable
rejection; a committed job replays its original semantic outcome across delivery
attempts. Receipt records have no run/history foreign key and survive collection.
The queue reads none of these tables. A reconciliation job reads them in the
service's maintenance lane, bound to one app, and no worker reaches them.

Creator object storage holds large inputs, signals, results and prepared uploads.
The normal bundle store holds `.zship` manifests and modules. Neither is a place
to copy customer data for scheduling. An opaque payload ID in a job is not a
signed download URL, database URL, object key or credential.

### Models, migrations and transaction domains

Every table has `id` as its sole primary key. Composite domain identities use
unique indexes: for example app/request and app/run/generation. Scoped foreign
keys retain the app identity. IDs and cursors preserve bytewise ordering. New
rows receive typed storage IDs; upserts preserve existing storage IDs, revision
tombstones and immutable receipts.

An app scope uses its `AppId` directly as `queue_scopes.id`, and dependent
`app_id` foreign keys reference that primary key. Do not duplicate the same
identity in another uniquely indexed parent column: competing first-registration
upserts must share the same conflict arbiter, which is what lets two concurrent
registrations of one app converge on one scope (`register_scope_in` inserts on
conflict and decides from whichever row won). Records with a composite domain
identity retain their own storage ID and composite unique index. A worker has no
row in the manager schema: its identity is the enrolled instance, and a delivery
names it in `jobs.worker_id`.

The canonical migration DSL generates SQL and ORM descriptors. Both platform
and journal persistence use native `zeroship-data-orm` models and collection
operations, including `schema!`, `FromRow`, `Insertable` and `Changeset` where
appropriate. PostgreSQL/SQLite selection belongs to the ORM. Do not restore
separate workflow backends or add workflow-specific ORM access exceptions.

Provisioning applies each schema in its own authorized zone before execution.
Claiming a job performs ordinary DML and creates no schemas, roles or tables.
Platform startup still verifies restricted role membership, required grants,
read-only fingerprints and lack of customer authority. Connection authority is
a binding mode, not proof that the supplied role is appropriately restricted.

The coordinator and queue use the same physical namespace and callback
`Database` transaction. Authorization callbacks receive that existing handle, so
an enrollment recheck runs inside the transaction it authorizes; opening another
transaction from a queue callback could wait on its own app lock and would
separate authorization from mutation. The app's scope row is the one lock a
queue operation takes, and fenced writes validate the rows they affect. Policy
is observed before any app lock, never inside a queue transaction. Recovery
pagination filters owners before ending a result page, so an owned page cannot
conceal later missing owners.

| Transaction | Operations that commit together |
| --- | --- |
| Manager scope registration | The app's scope and its frozen zone, refused when a stored zone differs, and the zone's capacity target row. |
| Manager submission | Stable job specification, submission deduplication and owning scheduling/recovery metadata. |
| Manager claim | Under the app's scope lock: enrollment rechecked, management validated, the delivery ceiling applied, the attempt numbered, `leased_at` and the lease deadline written, the job moved behind waiting work in the dispatch order, recovery re-armed for an intent-producing job, and enrollment rechecked again. |
| Manager give-back | The live delivery's row returned to `ready`, its holder cleared, its back-off written, and a back-off counted toward the next one's length. |
| Manager calendar turn | Selected occurrences/jobs and the schedule cursor/revision that produced them. |
| Manager settlement | Exact delivery fence, the outcome the journal decided, and associated scheduling, recovery and barrier changes. A settlement publishes no successor. |
| Manager abandonment | A deleted app's duties removed, its closing attempt cancelled and its unsettled creator jobs settled `Rejected`. |
| Journal acceptance | Input/event reference, run or signal acceptance, request result and publication intent. |
| Journal execution | Frontier transition, history, waits/children, promoted payload references, committed job outcome and successor intents. |
| Journal management | Lifecycle transition or explicit refusal, request identity, durable outcome and resulting publication intents. |
| Control retention | Deployment reclamation fence and hold mutation under the same deployment lock. |

There is no distributed transaction across these owners, even where the queue
and the journal share a schema: durable intents, idempotency and reconciliation
connect their commits. Object uploads likewise cannot join a database
transaction; reference promotion supplies that boundary.

Database clocks decide local expiry and due work. The manager uses its own
independent clock connection to the same database, sampled after lock waits; it
does not query through the transaction's held pool connection. Raw clock reads
and host privilege inspection are narrow public-API gaps, not alternate SQL
backends.

A monotonic caller budget includes lock acquisition, authority checks and waiting
for commit. Clock conversion includes elapsed clock-query and transport time;
later checks can shorten an attempt's budget but cannot restore elapsed time.
The deadline expires execution and new mutation admission. Cancellation before
terminal dispatch must roll back. A timeout after COMMIT dispatch is ambiguous:
settlement may finish, and the caller must read its durable receipt. It is never
proof that the transaction rolled back.

Delivery authority crosses processes as `DeliveryLease.remaining_ms` and
`DeliveryLease.attempt_remaining_ms`. The manager captures monotonic deadlines
for the lease and for the attempt when issuing the database lease, anchored
before the database clock query and reduced by the clock sample's resolution.
Later samples can only shorten them. After commit, `DeliveryGrant::lease`
subtracts all intervening elapsed time and refuses an exhausted grant. The client
captures its own monotonic instant before starting the request and adds the
returned remaining durations to that instant, conservatively charging the entire
exchange. It never compares the manager's `Delivery.deadline` with a worker wall
clock or a database clock.

A heartbeat commits under the previously stored delivery lease and the original
transaction budget. Its successful new grant has its own deadline, never later
than `leased_at` plus the attempt cap; capping that grant by the old
transaction budget would prevent renewal. The client still rejects the reply if
its earlier confirmed local grant expired while waiting. Renewal also cannot
restore a cancelled execution or extend the executor's original hard deadline.
Journal frontier fencing remains required independently of these delivery
leases.

The journal consumes the trusted Rust `JobLease` contract, implemented by the
native `DeliveryGrant` and the authenticated client's `LeasedJob`. Native grant
identity is private; a mutable wire `Delivery` alone is not execution
authority. This trait is a trusted host composition seam, not a cryptographic
boundary against arbitrary Rust implementations. Journal acceptance captures
the grant and policy before opening its transaction. It translates remaining
time into the journal clock conservatively and returns a private `DeliveredTask`
capped by the actual task deadline. Full-operation timeouts bound database waits
and commit acknowledgement. Expired calls can recover committed receipts through
bounded reads, while fresh mutations still require live captured authority.

## Durable job protocol

### Identities and allowed metadata

The current closed contract lives in
[`workflow_jobs.rs`](../../crates/zeroship-core/src/workflow_jobs.rs):
`JobSpec`, `JobOperation`, `Delivery`, `DeliveryLease`, `ClaimJobs`,
`ClaimedJobs`, `JournalSettlement` and `SettlementReceipt`.
It defines activation, advance, cron, management, fanout, propagation,
hold release, closure, reconciliation and collection operations.
Executable operations carry their deployment prerequisite inside the operation;
journal-only commands, fanout, propagation, reconciliation and collection carry
none. Management names
its lifecycle revision and a closed resolved command, including the immutable
target for a latest restart. Shared restart validation derives the effective
policy before that target is selected.
`JobOutcome` is a closed tagged object: `Completed`, `Waiting`, `Rejected`,
`Management` containing the closed lifecycle outcome, or `Closed` with its
drain evidence. Generic results belong to non-management jobs; a management job
requires its lifecycle result. Unknown fields are refused. Activation names its
platform revision; cron names the logical schedule identity and bundle
declaration name alongside the occurrence's request, run, revision and instant.
The input-free registration and activation envelopes live in
`workflow_schedules.rs`, and each names the app's execution zone.

Keep the following identities distinct:

| Identity | Retry rule |
| --- | --- |
| App request | Reuse for retried start/signal/management acceptance; changed body conflicts. |
| Logical job | Reuse across publication retries and delivery attempts. |
| Host policy binding/source revision | Binding replacement retires existing handles; policy refresh preserves source revision only for identical values and cannot reinstall a retired binding. |
| Delivery attempt | Changes on every claim of the job; a stale attempt cannot heartbeat, settle or give back the new one. |
| Run generation/frontier | Fences customer history and identifies the next admissible transition. |
| Wait/event/occurrence | Identifies the semantic trigger even if several delivery attempts observe it. |
| Hold generation | Fences retention acquisition/release independently of task and delivery leases. |

Bodies and nested variants are closed and bounded. The manager receives only
allowlisted scheduling metadata: app, job, deployment, run/reference identity,
revision, operation, deadline and closed outcome. Workflow export identities,
cron expressions, timezone and declared scheduling policy can be allowlisted
deployment metadata. Arbitrary inputs, result JSON, signal bodies, stack traces,
customer connection details and free-form error messages never enter the queue.

An identifier parsed from a body grants no authority. Host authentication selects
the worker and its zone; all referenced jobs and deliveries must agree with
what the queue recorded for that worker. Unknown or cross-app references must not
become a probe into another tenant's state through differing payloads or
diagnostics.

### Authenticated delivery boundary

The server and typed client expose the following worker requests through the
bounded, authenticated metadata transport (`crates/zeroship-workflow-server/src/api/jobs.rs`,
`crates/zeroship-workflow-client/src/jobs.rs`).

| Operation | Request metadata | Authority and reply checks |
| --- | --- | --- |
| `POST /v1/jobs/claim` | `ClaimJobs`: free slots, the caller's wait, cursor and exclusions; no app. | The verified instance's frozen zone decides the apps paged; enrollment is rechecked after each app's lock and before commit. Each delivery names the caller, carries remaining lease and attempt durations measured at reply time, and an acceptance exactly for `advance`. A grant the caller cannot use is never returned: one with lease left is given back, and a spent one lapses. |
| `POST /v1/jobs/heartbeat` | Delivery identity and, for a task, the journal task. | Fence `(job, worker, attempt)` on a live lease plus enrollment; the new lease never passes the attempt cap; the journal extends nothing when policy has admission or dispatch off. |
| `POST /v1/jobs/settle` | Delivery, and the execution to commit when the holder has one; never an outcome or successors. | The signing worker must be the delivery's and the delivery the queue's latest for the job. The journal decides the outcome, from the execution it commits or, with no execution, from the receipt it already holds. Active delivery authority for writes, or original-worker enrollment for an exact stored receipt; reply matches app, job and attempt, and its outcome is a family the operation admits. |
| `POST /v1/jobs/release` | Delivery, the journal task when there is one, and a closed `GiveBackReason`. | The signing worker must be the delivery's and the delivery the queue's latest. The journal releases the task and the queue returns the row to `ready` with the growing back-off. |
| `POST /v1/jobs/receipt` | The logical job. | Served only to the worker the queue last delivered the job to; absence is a null reply. |
| `POST /v1/tasks/payload`, `/v1/tasks/executable`, `/v1/tasks/payload/reserve` | App, task id and task token. | Served only to a worker holding a live delivery of that app; the task token is the authority within it, and the worker identity is substituted from the credential. |

Derive worker identity from the verified instance signer. An echoed worker must
match it. A request cannot supply its own lease or attempt deadline. Perform
revalidation inside the queue transaction after the app lock and before commit.
The enrollment check retains the originally verified key identity; a replacement
key under the same worker ID must not keep an earlier key's pending request
authorized. Revalidation queries registry state without consuming the signed
assertion's replay token again. Every check that reads the queue comes before any
journal is asked, because observing an app's policy is I/O whose refusals differ
by whether the app exists.

A worker's claim is execution authority, not scheduling authority. No worker
request path publishes a job: committed journal intents reach the queue through
the service's own publication, and a settlement names no successors at all.
Operation provenance is additional to the queue's app and identity checks.

An exact settled receipt may outlive the lease that produced it. Do not reject it
in a live-lease preflight before the queue can select its receipt-replay branch.
That branch checks current enrollment of the original worker, compares the stored
complete settlement identity and admits no new writes. Changing the job, attempt
or outcome is a conflict.

Control scheduling uses the same bounded authenticated transport through
`POST /v1/schedules/register`, `POST /v1/schedules/activate` and
`POST /v1/schedules/disable`. The server checks the exact `svc/control` issuer
before buffering each request. Instance credentials grant none of these
operations. Registration returns the accepted typed declaration, which the
client compares in full, the app's zone included; native storage canonicalizes
its ordering independently. Activation returns the stable job, whose app,
deployment, operation and revision must match the original request. These routes
require no enrolled worker. Control's lifecycle publisher is their production
caller, delivering the durable intents described under
[normal deployment publication](#normal-deployment-publication).

### Delivery, execution and settlement

```text
Manager job:

  durable ready -- available, not backed off + zone claim --> leased
       ^      ^                                            /   |   \
       |      |      give-back: deferral, journal refusal,     |    \
       |      +------ preparation failure, release ----+       |     |
       |                                                       |     |
       +------ lease lapse (no heartbeat, spent grant) -------+      |
                                                      exact settle   |
                                                                     v
                                                                  settled
                                                                     |
                                                  immutable replay <-+

Journal transition:

  delivered job -> existing committed receipt? -> settle with that outcome
                        |
                        no
                        v
              claim expected frontier
                        |
              worker executes bounded turn
                        |
              journal commits state + receipt + intents
                        |
              queue settles with the journal's outcome
```

```text
Workflow service (queue + journal)              Worker
      |                                             |
      |<---- ClaimJobs (free slots, cursor) --------|
      | per app of the caller's zone: lock scope,   |
      | recheck enrollment, lease, accept into      |
      | the journal; measure leases at reply time   |
      |---- ClaimedJobs (leases + acceptances) ---->|
      |                                             | prepare app; load pin;
      |                                             | execute bounded turn
      |<---- heartbeat (queue lease + task) --------|
      |---- renewed lease and task, or nothing ---->|
      |                                             |
      |<---- settle (delivery + execution) ---------|
      | journal commits frontier, receipt, intents; |
      | queue settles with that outcome             |
      |---- settlement receipt ------------------->|
      | publication wake drains the new intents     |
```

The queue provides at-least-once delivery. It deduplicates immutable submission
and settlement content. A heartbeat can update the stored deadline, never past
the attempt cap; the mutable echoed deadline is not part of immutable delivery
identity. Retrying after a lost heartbeat reply must remain possible.

Renewal cannot revive expired execution. A retry can observe a still-live stored
manager lease, but the worker rejects renewal after its local guard has expired
or been cancelled. A later lease never extends the original execution deadline:
the hard bound is the smaller of the worker's local ceiling and the attempt
remainder the delivery carried (`DeliverySlot::run` in
`crates/zeroship-workflow-runner/src/delivery.rs`), and the guard is also held
to the current lease, which each renewal moves. Redelivery requires a fresh claim
and admission, including a check for an outcome an earlier attempt committed.

The journal checks a committed receipt before running app code. A job that
already committed returns that result. Otherwise the task claims the expected
app/run/generation/frontier, captures trusted policy and pins execution
authority. Competing jobs for the same frontier cannot both advance it. A new
attempt must not reuse another attempt's write authority.

The manager's lease alone cannot stop an old process writing a creator database
through its own `env.db`. Journal transactions check the execution fence and
current frontier, and a replacement serializes with any earlier transaction on
that frontier. If an older COMMIT was already dispatched, recovery reads its
result after settlement; it cannot assume the older execution did nothing merely
because delivery expired. Synchronous JavaScript is interrupted through the
execution budget. Quarantine prevents isolate reuse and initiates cancellation;
it does not prove that native work has stopped. The executor must join native
operations through runtime shutdown before releasing the slot or publishing
completion. Rejection of a JavaScript promise alone is insufficient. Already
dispatched COMMIT remains supervised until its outcome is settled or explicitly
uncertain.

A fresh delivery may find an outcome committed by an older attempt. It settles
that semantic outcome using its own current delivery fence. The stored journal
outcome must therefore be independent of the old delivery envelope. Conversely,
exact replay of an already settled manager attempt returns its immutable receipt
after the lease lapsed, but still requires current enrollment of the original
worker. Changed content, another worker or a superseded unsettled attempt cannot
use receipt replay to admit new writes.

### Publication and receipt retention

Successor identities and content are generated once in the journal transaction.
The service's publication wake and delivered reconciliation use the same IDs and
immutable specifications. They may race; manager submission deduplicates them.
Neither path substitutes a fresh ID after a lost response.

The delivery API settles the semantic outcome the journal committed and
publishes no successor through settlement. Committed successor intents use the
independent publication path, and a settlement neither captures nor confirms
them. After every mutating commit the service wakes one coalescing drain for the
app (`PublicationWake` in `crates/zeroship-workflow-server/src/publication.rs`),
which publishes pending intents through `AppWorkflows::publish_pending_jobs`
into its own queue; the manager's reconciliation duty catches any drain that
was cut off.

Journal state commits before its acknowledgement. If the queue is unavailable or
full, intents remain pending. Marking publication confirmed happens only after a
manager receipt is validated against app, job and content. A lost confirmation
write merely causes another idempotent publication attempt.

`AppWorkflows::pending_jobs` pages journal-owned advance intents under trusted
app policy. `publish_job` reads and commits locally before calling its
host-bound `JobPublisher`, validates the entire immutable specification, then
confirms it under the app lock in a new journal transaction. Concurrent
publishers may submit the same job; a confirmed intent remains as a durable
receipt. The service's publisher is `LanePublisher`
(`crates/zeroship-workflow-server/src/sweeps.rs`), which refuses a job naming
any app but its own and submits through `Queue::submit`; no worker publishes.
These methods do not discover apps or schedule work. Delivered reconciliation
uses the same confirmation path with additional captured delivery and policy
checks around publication, lock waits and commit.

Starts, child/continuation creation, task checkpoints, restart and runnable
lifecycle/signal/dependency wake-ups record their advance intent in the journal
transaction. A checkpoint publication failure rolls back history and its task
receipt together. Pending intents prevent journal deployment-hold release even
if run history has been removed. Confirmed records do not retain that hold by
themselves; the manager's queue and retention fences then own delivery
dependencies.

App request receipts and delivery receipts have separate identities. Both retain
their deduplication state until an explicit retirement protocol proves that the
relevant submissions and redeliveries cannot be admitted again. The journal's
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
reconstruct or overwrite journal history.

| Object and owner | State and transition responsibility |
| --- | --- |
| Worker instance, Control | Active while its lease is renewed; retired on graceful exit or by purge, expired when its lease lapses. Its zone is frozen. It holds no workflow state of its own: losing a worker loses only the leases it held. |
| Job, manager | Pending work becomes claimable at its availability once any give-back back-off has passed, receives a leased attempt, and settles against that attempt. A lapsed lease permits redelivery of the same job; a give-back returns it to `ready` at once. A job whose counted executions reached the delivery ceiling stays unsettled and unclaimed. |
| Run generation, journal | The journal owns `queued`, `running`, `sleeping`, `waiting`, `paused`, `stalled`, `compensating`, `completed`, `failed` and `cancelled`. The existing lifecycle rules determine legal transitions. |
| Publication intent, journal | A committed pending intent remains recoverable until a matching manager receipt confirms publication. An unknown remote result remains pending. |
| Lifecycle intent, Control | A committed pending intent blocks later revisions of its app and, for activation, retains its deployment until the manager's exact receipt confirms it. |
| Management request, both owners | Manager acceptance creates delivery responsibility. Journal application/refusal produces the durable outcome; manager settlement reports that outcome and settles the matching barrier. |
| Deployment, both owners | Platform activation selects code for new work; journal activation confirms local prerequisites. A run's existing pin changes only through an explicit lifecycle operation. |

The run-state vocabulary lives in
[`workflow_coordination/lifecycle.rs`](../../crates/zeroship-core/src/workflow_coordination/lifecycle.rs).
The customer lifecycle engine owns its transition matrix. Scheduling in the
manager does not give queue handlers a parallel run-state machine. In
particular, job `Completed` means the bounded operation committed; it does not
necessarily mean the workflow returned a terminal result.

Terminal completion and collection are separate. A terminal generation can still
retain history, restartable checkpoints, dependent child results, payloads and
deduplication receipts. Management acceptance likewise does not promise immediate
quiescence: its journal transition and fence determine when cancellation or pause
has actually taken effect.

## Trigger lifecycles

Each trigger follows the commit and delivery rules above. The origin determines
which owner first has durable work and which database can contain its data.

| Trigger | First durable write | Manager receives | Journal commits before settlement |
| --- | --- | --- | --- |
| Explicit start | Creator run, input reference, request receipt and intent. | Stable advance job and run identity. | Next frontier, receipt and successor intents. |
| Cron/interval | Manager occurrence and job under activated schedule revision. | Normal deployment schedule metadata. | Occurrence acceptance and run creation, or a durable overlap refusal. |
| Sleep/retry | Creator checkpoint, wait/retry state and timed successor intent. | Delayed job with expected generation/frontier. | Resume transition or stale-wait no-op receipt. |
| Signal/topic event | Creator event body, target/cursor and intent. | Opaque event or fanout reference. | Consumption/fanout page, affected frontiers and intents. |
| Child/continuation | Creator relationship, child/continuation acceptance and intent. | Stable run/event references. | Child progress or parent notification consumption. |
| Management | Manager authorized command and delivery barrier/job. | Closed command with stable request and provenance. | Lifecycle outcome, durable receipt and resulting intents. |
| Reconciliation/collection | Manager scope obligation or maintenance deadline. | Known app and bounded operation/cursor. | Publication/collection progress and continuation intent. |

### Explicit start and input acceptance

`start(input)` runs through the app-scoped native handle in its ordinary
runtime. On a worker that handle is a `RemoteBackend`, which carries the call to
the workflow service; on the local host it is an in-process `AppBackend`.
Platform services do not import either to write customer data.

```text
App code         Worker (any of the zone)      Workflow service (queue + journal)
   |                     |                                  |
   |-- start(input) ---->|                                  |
   |                     |-- /v1/runs/start (app id) ------>|
   |                     |                                  | zone rule; bind policy
   |                     |                                  | establish ingress epoch
   |                     |                                  | stage input; transaction:
   |                     |                                  | run + request result + intent
   |                     |                                  | COMMIT
   |<-- run handle ------|<-- started run ------------------|
   |                     |                                  | publication wake ->
   |                     |                                  | Queue::submit (advance)
   |                     |                                  |
   |           any worker of the zone:                       |
   |                     |-- zone claim ------------------->|
   |                     |<-- advance job + acceptance -----|
   |                     | execute bounded turn             |
   |                     |-- settle (execution) ----------->| journal decides; queue settles
```

Journal commit is durable acceptance. The returned handle means accepted work,
not completion or immediate queue visibility. Request retries return the same
accepted run; a changed input under the same identity conflicts. If input staging
or the journal transaction fails, there is no accepted run. If the response is
lost after commit, the request receipt resolves the uncertainty.

Before accepting a start or signal, the service holds a manager-recorded scope
responsibility covering unpublished intents: the binding's ingress epoch, which
the service establishes through `Recovery::establish` before the first
acceptance and again above an epoch the journal refused. This obligation is
established before customer acceptance, so a failure between the two can leave
extra reconciliation responsibility but cannot leave accepted work
undiscoverable.

The worker that served the request need not be the one that executes the run:
the `advance` job is claimable by every worker of the app's zone, and a worker
that never claimed a job of the app serves its starts and reads. If the zone has
no worker free to claim, the accepted job waits in the queue, where the zone's
capacity target counts it as demand. A request that never reached the service is
not accepted.

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
the app's first failure ends its turn and defers its next attempt by a retry
delay that doubles with each consecutive failure up to a cap and resets after
a success. The driver shares Control's bounded catalog threads with the
deploy, archive and restore transactions, and Control refuses to start when
it could not publish: without its service signer or with an unusable
coordinator origin. Activation registers the deployment's
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
A delivery cannot use those references outside its own app. Target generation
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
within one app; cross-app workflow calls are ordinary authenticated app
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
execution barrier cannot delay the cancellation it must finish. The service
publishes it from the journal's publication intents like Fanout. `Waiting` confirms a committed successor page;
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
an expired policy observation is not equivalent to valid policy with admission disabled.

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
| Creator `management_receipts` | Job identity linked to the exact job receipt, app/request uniqueness, required requested-run identity and management revision, unique app/run/revision. Retains the closed outcome; the linked job receipt holds the immutable specification those projections are re-derived from. Requested-run identity has no run foreign key, so `NotFound` needs no invented run. The app/run/revision unique index also orders the applied-revision lookup, so the highest revision it holds is the fence a creator run applies against. |

Creator application advances its management revision for both applied commands
and durable lifecycle refusals. Gaps, substituted identities and unknown older
commands cannot advance it. Completed job replay must find matching management
history, while allowing the applied revision to have advanced since that
receipt. Missing
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
| Restart using the latest deployment | Control names the deployment when it mints the command; the manager validates that the queue holds it and that the wire hash matches. Persist it in the immutable command identity and job. Creator preparation uses that exact deployment, and verifies its journal hold before the final fenced transaction. |

The default full restart uses the latest deployment. A restart from a task uses
the started deployment, and explicitly requesting the latest deployment for a
partial restart is invalid. Derive these effective policies before constructing
the delivery prerequisite; checking only an explicit deployment option would
misclassify a default restart.

The manager does not resolve the latest deployment. `ManageRun` carries the
deployment Control names - `ManagementOperation::Restart` holds a
`RestartDeployment` exactly when the effective policy is Latest - because the
endpoint is authorized to Control alone and Control is the authority for
`zeroship.apps.deploy_hash` and `zeroship.app_deploys`. Re-deriving the answer
in the manager would answer it a moment later than the caller decided.

What the manager validates about the named deployment is in
`Coordinator::manage` (`crates/zeroship-workflow-manager/src/coordinator/management.rs`):
the queue must hold it, with acquisition running inside Control's row lock so a
deployment belonging to another app finds no row and one whose retention state
has left `available` is a conflict; and the wire hash must equal the hash
Control minted when it granted the hold. What it does not validate is that the
named deployment is the app's current one.

Control selects the target before it issues the command, so acceptance does not
promise the app pointer is unchanged at the later manager commit. Under the
manager app lock, replay the exact original request first. For a new request,
keep its named target through hold preparation outside queue locks, then
reacquire the lock and repeat receipt matching before atomically inserting the
resolved command, job, order and barrier. Require the confirmed hold for that
same target before accepting. Retention failure before acceptance is retryable;
a committed receipt is replayed rather than re-resolved. Preserve raw
request identity separately from effective restart normalization. The manager
acceptance path composes the named target with queue retention. Command
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
transport. Settlement preflight checks the outcome family and pairs a
management result with the command that asked for it, and creator receipts
also enforce the operation's supported result. Manager settlement now validates
the authoritative command/job/order linkage and commits its lifecycle outcome,
settled revision and queue receipt together. Exact settled replay validates the
retained linkage before its final enrollment check; it cannot clear a newer
barrier. Public queue submission and successors cannot create management jobs.
Only authorized manager acceptance creates those jobs, and the service's
maintenance lane claims and applies them; there is no separate polling or
acknowledgement route. The journal's management handler carries its closed
outcome through the exact receipt and ordinary queue settlement without starting
an executor. The manager must never query the customer
journal to fill a deployment gap.

## Recovery responsibility and execution capacity

Journal intents cannot publish themselves if every drain that would carry them
is cut off. The manager therefore owns a durable obligation for every
ingress-enabled app, established before accepting customer data. It survives
worker loss, deployment replacement and service restart.

```text
Journal commit exists; its publication was lost
                    |
                    v
Manager scope obligation + reconciliation deadline
                    |
                    v
Reconcile job in the queue -> claimed by the service's maintenance lane
                    |
Lane reads the app's pending intents -> Queue::submit with stable identities
                    |
Commit confirmed progress -> settle + continuation or next recovery deadline
                    |
                    v
Advance jobs claimable by every worker of the app's zone;
the zone's capacity target counts them as demand
```

Each obligation has a manager-owned deadline even when no job is visibly pending.
Healthy heartbeats cannot postpone it indefinitely because the manager cannot
observe an unpublished journal commit. The publication wake is an optimization;
periodic manager-issued reconciliation supplies correctness.
The native `recovery::Recovery` ledger supplies independent deadlines and pending
jobs for reconciliation and collection. `ensure` registers a trusted activation,
records the app's scope in its zone, and establishes both duties atomically.
Repeated activation validates the complete retained pair without postponing
either duty; a newer activation changes only provenance. Both duties remain
app-scoped and require no retained executable. `dispatch` serializes with queue
operations under the app lock and commits the chosen duty's job identity with its
next deadline. A pending job is returned unchanged across retries and replicas
until it settles. A fresh `Waiting` settlement advances only its matching duty's
deadline to manager time without postponing an earlier deadline. A completed scan
preserves the periodic deadline. Receipt replay and unrelated jobs cannot modify
the current obligation. Scoped pending-job foreign keys prevent deleting a job
that still carries a duty or substituting another app's job.

`due` pages each duty kind by app identity using manager database time and includes
healthy owners. The scheduler resumes after the last returned app, then begins
another sweep after an empty page; newly due work behind the cursor joins that sweep.
Job publication failure leaves the prior deadline and pending identity intact.
The server drives these duties through independently bounded lanes; its
maintenance lane claims every duty job, and the advance jobs they publish are
claimable by any worker of the app's zone, while the capacity lane turns that
backlog into each zone's target. Ingress epochs tie this responsibility to
journal acceptance, and the closing lane retires it once an app idles or is
archived; see [ingress epochs and scope retirement](#ingress-epochs-and-scope-retirement).

A reconciliation job processes an app-scoped page without loading app code.
The persisted scan alternates between publication intents and deployment-hold
intents. Its immutable job receipt captures the phase, selected IDs, current scan
revision and a stable upper boundary before external I/O. These cursors stay in
the journal; the queue receives only a closed outcome. Selection reads IDs
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
open, closing, retired or abandoned. Activation opens it. An establishment
request names the epoch the journal refused, or none when the binding holds no
epoch and the journal closed none, as at startup. The manager returns an open
epoch above the named one, reopening a retired scope or advancing a closing
one, and recreates reconciliation and collection duties under the app lock.
Establishment follows the admission policy; archive masks admission, so an
archived app cannot be reopened by ingress. An epoch the manager never issued is
a conflict.

Hosts establish an epoch before they accept ingress, and attach their
establishment to the app handle (`AppWorkflows::with_ingress` with
`IngressEpochs`). The workflow service owns the recovery scope in its own
process, so `ServiceIngress` in `crates/zeroship-workflow-server/src/runs.rs`
calls `Recovery::establish` directly with the observed admission, and installs
the epoch into the binding `RunService::app` carries forward. The local host
establishes after registering its startup activation's responsibility and before
it announces readiness, through its manager client, because it is its app's
platform authority. An acceptance the journal refuses for a closed epoch
establishes an epoch above the refused one and retries once under the same
request identity, capturing the binding's newly installed authority rather than
the authority the request captured. Concurrent establishments serialize, and one
that finds a newer epoch already installed does not ask again. Hosts report
accepted ingress as activity through `Recovery::note_ingress`, coalesced so a
burst of acceptances costs one report.

Every creator ingress acceptance captures the epoch with its policy and, under
the app state lock before commit, requires it to exceed the journal's closed
epoch: start, direct signal, broadcast, signal ingestion to a run or a topic,
the pause, resume and cancel transitions, and restart. The fence runs after the
authority, admission and lifecycle checks, so an expired observation or disabled
admission reports its own refusal; a missing or closed epoch is a retryable
refusal. Issuing and revoking signal capabilities commit no run, publication
intent, payload or hold, so they are not fenced; redeeming a capability is.
Delivered jobs are not fenced by the epoch. Their claims and publications meet
the manager's watermark and re-arm below, and payload uploads run under a task
claim that the drain predicates count.

The manager driver's closing lane visits attempts in progress and open scopes
past their backoff that are idle or whose calendar Control disabled, as archive
does. A scope is idle after `recovery::Options::idle_after` without activity:
its epoch's opening, reported ingress, a publication or an intent-producing
claim; maintenance and closure never count. The manager begins closing only
when no job for the app is leased, no maintenance job is pending and no earlier
Close is unsettled. It records the app's dispatch cursor as the closing
watermark, suspends the scope's periodic duties for the attempt, and delivers a
manager-origin Close job for the current epoch. An attempt whose Close has not
settled within `closing_timeout` returns the scope to open. Each attempt defers
the next until its timeout plus `closing_backoff` have passed, the backoff
doubling per consecutive attempt up to `closing_backoff_max`; live work that
refuses an attempt defers the next by the backoff alone, and reopening resets
the pacing. The server maps the `workflow.closing_*` settings and the local host
its `[manager]` settings into these options.

Under the same app state lock the journal raises the closed epoch and evaluates
the drain predicates in one transaction: no unconfirmed publication intent, no
hold in transition, no payload in preparation or deletion, no live task claim,
and no deletion tombstone still owed its final resweep. Settlement retires the
scope only when it is still closing at that epoch, the result is drained and no
job was published or claimed above the watermark. Claims during closing do not
cancel the attempt; the watermark refuses its retirement at settlement.

Claiming an intent-producing job or publishing one reopens a retired scope before
execution. Only the manager publishes Reconcile, Collect or Close, and Close is
never a settlement successor. Worker loss, release, empty claims, healthy
heartbeats, completed scans and calendar or policy acknowledgements never retire
responsibility. A snapshot restore must reopen responsibility for the restored
apps.

Deletion abandons responsibility instead of closing it. For each candidate page
the closing lane reads Control's terminal deletion marker over the app-facts
capability (`FactsLifecycle` in
`crates/zeroship-workflow-manager/src/lifecycle.rs`), and abandons each deleted
candidate (`Recovery::abandon` in `crates/zeroship-workflow-manager/src/recovery.rs`):
its duties are deleted, a closing attempt is cancelled, its unsettled creator
jobs are settled `Rejected` in the same transaction, and the scope row stays as
the epoch's tombstone. No worker may run a deleted app, so those rows would
otherwise stay unsettled and be passed on every claim lap and capacity visit; a
holder of a live lease on one finds it settled at its next heartbeat or
settlement. Maintenance kinds stay with the lane that owns them. Nothing reopens
an abandoned scope; establishment and activation are refused, and claims and
publications leave it abandoned. A page whose deletion state cannot be read
visits nothing. The local host has no Control catalog (`lifecycle::Undeletable`),
so its app is never abandoned; idleness still retires it.

Native PostgreSQL and SQLite contracts cover this protocol in
`crates/zeroship-workflow-manager/tests/integration/closing.rs`,
`recovery.rs` and `retirement.rs`: a still-valid lease fenced after Close,
retirement refused in both orders of a racing acceptance and Close, a scope kept
open when a job is claimed or published above the watermark, reopening exactly
once across retries and racing replicas, lost acknowledgements and redelivery
converging on one retirement, archived apps drained, duties that fall due during
closing held, the idle and archive triggers, expiry, the doubling backoff and its
reset, deferral by live work, abandonment that nothing reopens, and a driver pass
that closes an idle scope while abandoning a deleted one. The service's
`an_established_epoch_survives_the_next_requests_policy_reinstall` and
`closing_the_journals_epoch_retires_the_carried_forward_one`
(`crates/zeroship-workflow-server/tests/integration/http_runs.rs`) cover its
establishment. The local host retires an idle app through a delivered Close, and
its next start is fenced, establishes a newer epoch and completes.

**Remaining before production:** Control does not publish deletion as a
lifecycle intent, so the lane learns of a deletion only for the candidates it
visits: a deleted app whose scope had already retired is abandoned only after a
re-arm reopens it and the next pass visits it. Snapshot restore still needs its
reopening contract.

Manager maintenance schedules journal reconciliation, payload collection and
customer retention checks as explicit jobs, and the service's maintenance lane
runs them beside the journal. Their failure retains responsibility and
references; it does not permit a creator process to read the journal or declare
the app drained.

### Delivered payload collection

The manager duties, the journal handler and the service's maintenance lane
implement this contract, and the local host's maintenance lane runs the same
delivered jobs.

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

The journal owns the `collection_*` columns of `app_state` and immutable
`collection_pages`, both under its `__zeroship_workflow_` table prefix. Each page has a scoped foreign
key to its job receipt and no run reference. The manager's `recovery_scopes`
retains activation provenance; `recovery_duties` owns independent per-app/kind
deadlines and pending jobs, with a sole typed `id` primary key and scoped uniqueness.
A scan captures an expiry cutoff and upper payload identity under the app lock,
then pages candidate identities within that fixed range. Its revision, cursor
and cutoff prevent later uploads or expiry changes from extending a sweep
indefinitely. A page stores its exact job linkage, immutable plan and reserved
item offset. Collection does not reuse reconciliation's fields or expose object
identities and cursors to the manager.

Before attempting an item, the handler advances the durable offset under the app
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
manager's Collect duty delivers collection to its maintenance lane.

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
app. A body cannot choose another holder. The workflow service holds each app's
stable journal holder, and its replicas share the stable logical queue holder.
Releasing a queue dependency cannot release a journal dependency, or vice versa.
Both hold pairs accept the workflow service role alone (`asserted_caller` in
`crates/zeroship-control/src/deployment_hold_api.rs` for the journal pair):
the queue pair derives `HoldScope::for_queue` and the journal pair
`HoldScope::for_app`. Requests carry the app, deployment and generation, without
choosing a holder; a worker holds neither pair.

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
maintenance job closes admission for that deployment, checks all customer dependencies
under its app lock, and commits release intent; the
[journal hold release policy](#journal-hold-release-policy) is how that job is
asked for and answered. Generation tombstones reject stale release/reacquire
messages. Queue release similarly accounts for every referencing schedule/job
under manager serialization.

A lost acquire reply causes an idempotent retry before admission. A lost release
reply retains intent until reconciliation; it must not turn into a new release
generation. Reclamation never reads the journal, and worker loss or an empty
queue never proves that code is unreferenced.

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

#### Journal hold release policy

Both holder classes release, and each requires something different. The queue
holder answers to the manager alone, on the conditions above. The journal holder
answers only to the creator engine, which is the one process that can read the
customer journal, so the manager cannot decide a journal release: it asks.

The same retention lane carries the request. The manager's hold row records the
journal release duty for its deployment alongside its own hold, and a deployment
whose queue hold is released becomes a candidate again on the terms the queue
hold used: the app's enabled calendar does not select it, no unsettled job
projects it, and it is older than `hold_grace`. `Queue::maintain_deployment`
then publishes one `JobOperation::ReleaseHold` for it through the ordinary
durable job path. That operation names a deployment but reports no
`JobSpec::deployment_id`, because a release is exactly the case where no hold
remains to confirm; no worker publishes any job, so only the manager publishes
one.

The creator engine answers it in `AppWorkflows::release_hold_job`, which runs
`WorkflowService::release_deployment_hold` under the app state lock: close
admission on the deployment record, then refuse while any live run or retained
generation names it, or any unconfirmed publication projects it. Unfinished
continuations and prepared payloads are restricting references to those
generations, so the generation check covers them. A refusal settles `Waiting`,
which returns the duty to pending; nothing is forced and the hold stays held.
Success settles `Completed` and discharges the duty. A deployment this journal
never held is already given back and settles `Completed` without a platform
call. Committed receipts replay without repeating the release.

Two things fence a late reply. The duty records the job identity it published
and applies only that one, and reacquisition of the queue hold clears the duty,
so a release already in flight cannot report the fresh journal hold released.
The hold row also records the manager time of the latest release publication, so
a refused release waits out another grace before the lane asks again rather than
republishing every sweep.

Terminal history still pins code: a completed run's retained generation keeps
its deployment's journal hold, because a partial restart replays against it.
Reclaiming that deployment needs journal history retention, which is a separate
contract, not a release decision.

The manager's `tests/integration/hold_release.rs` contracts run on PostgreSQL and SQLite:
replacement, archive and restore through holder-aware reclamation, an activation
reacquiring its hold while another replica's lane passes, stale candidate pages,
a hold without a recorded time, one release publication per grace with its
settled reply applied, and the reacquisition tombstone. The creator engine's
`service::tests::hold_release` contract refuses the release of a deployment its
journal still needs and gives back a superseded one it never used, after which
the platform reclamation fence commits. The CLI's `workflow_local` contract
completes a run on one bundle, republishes, and observes the superseded
deployment's queue hold released, in the manager and in the ledger, once every
job pinned to it has settled. The server's driver contract releases an aged,
unselected hold through its Control client.

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
| `zeroship-core` | Closed workflow metadata and transport-independent capability contracts; canonical entity identities, `ZoneId` among them, are supplied by `zeroship-id`. No ORM, HTTP implementation or customer replay envelopes. |
| `zeroship-workflow-calendar` | Shared schedule definitions and pure cron/interval calculations with an explicit interpretation identity. No clock owner, ORM, runtime or scheduling loop. |
| `zeroship-workflow` | `WorkflowService`, bound `AppWorkflows`, journal transitions, replay, payloads, bounded job acceptance and the maintenance operations, and publication intents. |
| `zeroship-workflow-manager` | `Queue`, `coordinator::Coordinator` and its zone claim, scheduling, recovery, capacity, the policy source and the platform `deployments` ledger. No creator engine, V8 or listener. |
| `zeroship-workflow-client` | `WorkerCoordinator`, `ControlCoordinator` and the bounded authenticated transport. No ORM or scheduler. |
| `zeroship-workflow-runner` | The worker's half: `JobConsumer` and `DeliverySlot` over `JobTransport`, `PreparedApps`, `WorkerHost`, `RemoteWorkflows` and `RemoteBackend`. |
| `zeroship-workflow-v8` | `WorkflowBinding`, V8 argument conversion, trusted app binding and executor shutdown barrier over the customer engine. |
| `zeroship-workflow-server` | HTTP routes, enrollment and zone authentication, configuration, readiness, the journal it serves, its maintenance lane and manager lifecycle composition. |
| `zeroship-worker` | The executable hosting normal requests and the workflow host, its creator-resource provider and `AppResidency`. |

```text
Normal Cargo dependencies; arrows are not network calls

workflow-server --> workflow-manager --> data-orm [platform binding]
       +----------> workflow         --> data-orm [platform binding, journal]
       +----------> workflow-client  --> authenticated metadata transport

worker ----------> workflow-runner --> workflow-client
   |                      +---------> workflow [no store opened]
   +-------------> workflow-v8 -----> workflow + runtime
   +-------------> workflow-client

Control ---------> workflow-client
   +-------------> workflow-manager::deployments [authorized platform binding]

CLI -------------> workflow-manager [local platform metadata]
   +-------------> workflow         [local journal]
   +-------------> workflow-runner
   +-------------> workflow-v8

All workflow contracts use core / canonical typed identities
Schedule calculation uses workflow-calendar without either persistence owner
```

Manager and customer engine do not depend on each other. The client depends on
neither persistence implementation. Hosts inject metadata capabilities; local
composition invokes native manager operations with a trusted local caller, while
a worker uses the client. Customer replay envelopes, inputs, payload types and
detailed execution errors remain in the customer library and V8 adapter.
Owner-specific errors are mapped to closed boundary failures rather than making
the manager depend on `WorkflowServiceError`. The manager links neither the
runner nor payload storage; the server holds the payload store its sweeps write
and delete through, and never the runner; the engine, the runner and the worker
never reach the manager or the server.
`workflow_process_dependencies_follow_crate_ownership`
(`xtask/tests/workflow/mod.rs`) holds those boundaries.

The target keeps queue, scheduling, management, recovery and capacity as modules
of the manager. The bundle crate continues to own artifact formats and
verification, not ORM deployment persistence. A generic deployment service or
additional ledger crate is outside this restructuring.

`WorkflowService` is a library handle, not a new deployable service. Runtime-loader
interfaces should match actual I/O; constructing an already loaded runtime can
remain synchronous. `async-trait` is not an architectural requirement. Shipped
I/O stays on compio; no additional async runtime is introduced.

The delivered-job slot (`DeliverySlot` in
`crates/zeroship-workflow-runner/src/delivery.rs`) retains the current manager
grant, journal `DeliveredTask` and executor handle together. One heartbeat
renews the queue lease and the journal task in one exchange, and the slot updates
the execution guard only when both succeed. The hard execution bound - the
smaller of the local ceiling and the delivered attempt remainder - stays fixed
through code loading, input reads, execution and the executor's payload
preparation. Joined shutdown and final journal writes have a bounded finalization
budget with live lease checks. Cancellation drains native operations before the
task is released or the slot reused. After a durable journal outcome, the slot
retries immutable settlement metadata without running app code again. A release
gives the delivery back: the journal releases its task and the queue returns the
row to `ready` with a back-off, so an interrupted execution is redelivered
without waiting out its lease.

Update Cargo declarations, configuration registration, schema generation,
container build inputs, xtask selection and dependency gates with each move.
The existing client's `cyper` carrier follows the client crate; do not retain
duplicate clients or add Tokio as a normal dependency to avoid updating a gate.
Use main's shared ORM API and coordinate its changes with the ORM owner.

### Executable host composition

`WorkerHost` (`crates/zeroship-workflow-runner/src/host.rs`) composes one
`JobConsumer`, its execution slots and a `PreparedApps` cache, and
`zeroship-worker`'s `workflow_host` module constructs it on a dedicated compio
thread with the enrolled instance signer, a creator-resource provider and the
version feed. A worker starts exactly one host: the slot count is the process's,
and independent hosts would each claim a full batch for it. Its manager origin,
slots and prepared-app bound arrive through the configuration contract
(`worker.workflow_manager_url`, `worker.workflow_slots`,
`worker.workflow_prepared_apps`, which must be at least the slots); with no
manager origin no host runs and `env.workflows` refuses retryably.

**Apps are prepared when a job arrives.** For each claimed delivery the slot
calls `PreparedApps::get_or_prepare`
(`crates/zeroship-workflow-runner/src/prepared.rs`) for the delivery's app. A
miss runs under the delivery's own remaining lease, never longer than the host's
operation bound, and calls `CreatorFactory::open` for the app:
`WorkflowCreatorFactory` (`crates/zeroship-worker/src/workflow_creator.rs`)
resolves the app's metadata and environment from Control, which answers only
for apps in this instance's zone, and builds the payload store, the loader and
the V8 executor around a `RemoteBackend` for that app. The entry keeps the app's
journal type (`()` on a worker, because the journal is the service's), its
executor and a residency guard as one object. The cache is a bounded LRU that
evicts only idle entries: an entry an execution holds is shared with it, so
evicting it would free nothing. An app the version feed stops listing leaves the
cache on the next claim cycle, while an execution holding it keeps it alive until
it finishes. Nothing is designed around warmth; the cache keeps reusable objects,
and one fresh isolate per execution is built either way.

**A preparation failure is a failure of that attempt, not of the app.** The slot
gives the claim back - journal task included, with `GiveBackReason::PreparationFailed`
- and the queue returns the row with a back-off that grows with each consecutive
back-off. The claimer adds the app to a local skip list whose expiry doubles per
consecutive failure up to a ceiling, and sends the live entries as
`ClaimJobs.exclude`, bounded by `ClaimJobs::MAX_EXCLUDE`. Nothing is written
server-side; the list only narrows this worker's offers and dies with the
process, and the given-back row is excluded from capacity demand while its
back-off runs.

**Credentials are resident only while something holds the app.** `AppResidency`
(`crates/zeroship-worker/src/residency.rs`) counts the holders of each app on
the worker: every prepared app and every execution running from it, every HTTP
isolate and the requests in flight on it, and a reconcile swap. A holder takes
its `Residency` before checking whether the app's material is supplied, and a
refresh that holds nothing asks `reside_held`, so a refresh racing the last drop
cannot resurrect what that drop withdrew. Dropping the last holder withdraws the
app's project key, database bindings and `SharedEnvs` entry under the registry's
lock. A worker therefore holds decrypted environment and data keys only for the
apps it currently holds, not for every app of its zone it ever served.

**The request path needs no preparation.** `RemoteWorkflows`
(`crates/zeroship-workflow-runner/src/remote.rs`) holds the enrolled
`WorkerCoordinator`, the payload object store and the read limit, and answers
`backend(app)` for any app. The host builds it before it claims anything and
hands it to every HTTP thread, whose isolates bind through
`WorkflowBinding::remote_workflows`, selecting the backend by the runtime's own
trusted app identity. The first `env.workflows.start` on a worker that never
claimed a job of the app is one authenticated call admitted by the zone rule.
Workflow-execution isolates bind through `WorkflowBinding::remote`, fixed to the
one app being executed.

The creator-resource provider (`ProductionResources` in
`crates/zeroship-worker/src/workflow_host.rs`) supplies the process's own
capabilities: the `env.db` service's connection and project keys, the
`env.storage` object store, the artifact store and the app metadata Control
serves this enrolled instance. A claimed app's id selects them; it cannot select
credentials or a schema. Context refresh may supply environment and runtime
limits under explicit freshness, while the workflow backend remains fixed. The
retention hold on a pinned artifact is the service's: `resolve_task_executable`
takes it where the pin is resolved.

Shutdown is joined and ordered (`drain` in
`crates/zeroship-worker/src/workflow_host.rs`). On SIGTERM HTTP and the workflow
host drain side by side, both within `worker.shutdown_timeout`. The host stops
claiming at the signal; a claim already pending runs to its reply and gives back
unstarted what it brings, beside the executions already running, which are
driven throughout and finish and settle until one deadline:
`worker.shutdown_timeout` less one operation bound after the signal. Whatever is
still running then is cancelled, its native work joined and its delivery released
within that last bound, and the host drops its prepared apps and their
residency. Only then does the instance retire, because the host's last exchanges
are signed with the instance key. A host that stops on its own stops the request
server with it and exits non-zero, so the orchestrator replaces a process whose
workflow host has stopped serving the durable work it accepted.

The local host uses the same `JobConsumer` with a CLI-owned native `JobTransport`
(`LocalTransport` in `crates/zeroship-cli/src/workflow/manager.rs`) over the
manager coordinator's real delivery grants and its zone claim, with
`ConfiguredPolicies` as its policy source and `LocalCapacity` as its provider.
Its `PreparedApps` serves exactly its one configured app, whose journal is the
local `AppWorkflows`. The manager, its driver and the platform metadata file live
on a dedicated manager thread; the consumer, journal and V8 executor live on the
workflow host thread and reach the manager through a `Send` client, and the host
runs its own maintenance lane for the sweeps. This keeps thread-local state of
app isolates, such as the ORM usage meter an `env.db` isolate stamps on its
thread, away from platform metadata. `zeroship_workflow_manager::local::LocalPlatform`
is the one explicit combined bootstrap for the local platform file: it installs
the deployment catalog and manager schemas together and refuses any other stored
DDL, including a deployment-only catalog.

Worker database posture refuses a login that can reach the platform schema at
all. Control holds no journal access and there is no Control or gateway advance
transport. The decisive process contracts use isolated creator and platform
databases, ordinary app ingress, zone-pulled delivery and joined shutdown.
Native library availability alone does not prove this boundary.

### Service operation inventory

This inventory defines responsibility and success semantics. Coordinator routes
are registered in
[`workflow-server/src/api.rs`](../../crates/zeroship-workflow-server/src/api.rs);
job, claim and task routes are in
[`workflow-server/src/api/jobs.rs`](../../crates/zeroship-workflow-server/src/api/jobs.rs),
run calls in
[`workflow-server/src/api/runs.rs`](../../crates/zeroship-workflow-server/src/api/runs.rs),
and Control schedule preparation, activation and disable in
[`workflow-server/src/api/schedules.rs`](../../crates/zeroship-workflow-server/src/api/schedules.rs).
Control's lifecycle publisher is the durable handoff for normal deployment
publication.

| Operation | Authorized caller and receiving owner | Successful result |
| --- | --- | --- |
| Join, renew and retire instance | Worker to Control. | An instance identity bound to its enrolled key and frozen execution zone, with a renewable lease. |
| Register/activate/disable deployment schedules | Control to the workflow service. | Idempotent immutable schedule metadata and monotonic activation state for the app's scope in its zone; dispatch readiness remains distinct. |
| Apply capacity target | Driver's capacity lane to the injected zone capacity provider. | `Accepted`, or a durable, retryable refusal for that target revision. |
| Establish/close ingress scope | The workflow service's own run path, or the local host, through the manager's recovery ledger. | Durable recovery responsibility or an explicit fenced drain result. A worker cannot create authority for an app. |
| Claim jobs | Enrolled worker to the workflow service's zone claim. | Up to its free slots of leased `advance` deliveries of its own zone with their journal acceptances, the next cursor and whether the zone was exhausted; an unusable grant with lease left is given back, and a spent one lapses. |
| Heartbeat delivery | Its worker. | Current bounded lease, never past the attempt cap, and the journal task renewed, or no extension when policy withholds dispatch; cannot revive a replaced attempt, expired execution or elapsed execution budget. |
| Settle delivery | Its worker. | The journal commits the reported execution, or answers from its receipt, and the queue settles with that outcome atomically with its scheduling and barrier changes; or replay of the matching receipt. |
| Release delivery | Its worker. | The journal task released and the row returned to `ready` with a growing back-off. |
| Read job receipt | The worker the queue last delivered the job to. | The journal's committed outcome, or none yet. |
| Submit/read management command | Creator-authorized Control to the workflow service. | Durable command acceptance or closed delivery outcome; no customer history or result body. |
| Acquire/release deployment hold | The workflow service, as queue holder or journal holder, to Control. | Generation-fenced retention result for that holder class and app. |
| Start/signal/transition/restart/read run | App code through the worker's `RemoteBackend`, admitted by the worker's zone. | Journal acceptance or detailed state. Customer bodies stay on this path. |

The worker requests work through outbound authenticated calls. Claims are
bounded by the caller's own wait and back off only when a claim reached the end
of the zone without filling its slots; transport reconnection never changes a
logical operation's identity. The manager does not reach into V8 through a
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
| Workflow service host | `WorkflowSettings` (`crates/zeroship-workflow-server/src/config.rs`) supplies listener, service peers, platform DB binding, body/page bounds, the payload store and the queue, claim and capacity settings below. `workflow.database_url` is a platform credential. |
| Native queue | `Options::{max_connections, lease, max_attempt, transaction_timeout, max_metadata_bytes, defer_backoff, defer_backoff_max}` bounds storage concurrency, delivery, one attempt across heartbeats, metadata transactions and the give-back back-off. `Options::validate` refuses an attempt cap shorter than the lease. The server maps `workflow.delivery_lease_ms` and `workflow.max_attempt_ms`. |
| Native coordinator | `coordinator::Options::{batch_limit, max_pending_management, claim_budget}` bounds the claim's page and exclusion list, pending commands, and the work a claim may start inside the wait its request states. The server maps `workflow.batch_limit` and `workflow.claim_budget_ms`. |
| Native manager driver | `driver::Options::{page_limit, lane_timeout, hold_grace, scheduling, recovery, capacity}` bounds each calendar, recovery, retention, closing and capacity lane; `hold_grace` is the minimum age before the [queue hold release policy](#queue-hold-release-policy) may release a hold, and `recovery::Options::{idle_after, closing_timeout, closing_backoff, closing_backoff_max}` pace closing. The server owns cadence through `workflow.driver_interval_ms`, derives the grace from `workflow.database_command_timeout_ms` so that it exceeds that budget, bounds each lane's complete turn by `workflow.driver_lane_timeout_ms`, and maps the `workflow.closing_*` settings to the closing bounds. |
| Capacity | `capacity::Options::{min_slots, max_slots, idle_hold_down, request_timeout, retry_interval}` from `workflow.capacity_min_slots`, `workflow.capacity_max_slots`, `workflow.capacity_hold_down_ms`, `workflow.capacity_request_timeout_ms` and `workflow.capacity_retry_interval_ms`; `workflow.static_pool_slots` is the static pool's size. |
| Metadata client | Client `Options::{timeout, max_request_bytes, max_journal_request_bytes, max_response_bytes}` bounds the complete exchange. Each call uses the host signer. |
| Worker host | `worker.workflow_manager_url`, `worker.workflow_slots` (deliveries executing at once) and `worker.workflow_prepared_apps` (prepared apps kept, at least the slots). The host's execution ceiling, operation bound, idle poll and error back-off are constants of `crates/zeroship-worker/src/workflow_host.rs`; `worker.shutdown_timeout` is refused below `MIN_SHUTDOWN_TIMEOUT`. |
| Local CLI host | `--workflow-config` TOML: `[consumer]` maps to `ConsumerOptions` and `DeliveryOptions`; `[manager]` maps to the queue lease, the claim budget, driver cadence and lane bound, the hold release grace (`hold_grace_ms`, which must exceed the queue transaction timeout), the recovery interval, and closing idleness, timeout and backoff; `[payloads]` maps to `TaskPayloadLimits`. Unknown keys, including database or bundle settings, are refused. |

Policy snapshots are host-owned and revisioned. Expired metadata does not become
self-renewing authority through a retry, a customer row or a mutable `APP_ID`
environment value. Runtime identity comes from the immutable trusted app
context. Enrollment, policy observation, code availability and recovery
responsibility are separate readiness concerns.

```text
zeroship serve / Vite local host
       |
       +--> manager thread: manager + queue + driver
       |          |
       |          +--> .zeroship/platform/metadata.sqlite
       |               (deployment catalog + manager metadata)
       |
       +--> workflow host thread: zone claim consumer + maintenance lane
                  |                 + journal + V8 executor
                  +--> local journal + app storage
                  +--> same V8 executor and normal app bundle

               shared protocol, separate storage bindings
```

The CLI composes these libraries in process with normal app configuration. Local
calls omit network and enrollment ceremony while preserving scope checks, zone
claims, receipts, fences and recovery. The CLI owns setup, startup and
shutdown; it contains no bespoke cron evaluator, workflow bundle loader,
deployment watcher or journal scheduler. Supporting multiple app deployments in
the CLI is a separate concern.

Startup opens the journal, starts the manager thread and mints a worker identity
for the local consumer. The local host is its app's platform authority:
`ConfiguredPolicies` (`crates/zeroship-workflow-manager/src/local.rs`) answers for
exactly the configured app, in the default zone, never deleted, and its claims
use the same `claim_in_zone` as a deployed worker's. Serving an archive ingests
it into the retained store, records the normal deployment in the catalog,
prepares its schedule descriptors, which carry no creator input and name the
default zone, and activates it at the next revision unless it is already the
enabled selection. The host's maintenance lane applies that Activation as a
delivered job; the CLI establishes recovery responsibility for the selected
activation, establishes and installs its ingress epoch, and accepts requests
only after the journal has committed the activation receipt. A request the
journal later fences establishes a newer epoch through the local manager and is
retried once. Serving a plain script keeps the existing selection. Direct
creator activation is not used.

The host reports the ingress it accepted on its driver cadence. The HTTP and
workflow isolates share one app backend whose commit hint wakes a publication
pass after every start, signal, transition or restart; the transport wakes it
after every settled delivery, and startup wakes it once for intents a previous
process left behind. A failed pass leaves intents pending for the manager's
reconciliation job. Shutdown joins execution and stops the manager thread after
its in-flight operations and current pass.

The local journal is the dev-local file the CLI binds. The manager uses the
normal local platform metadata/catalog binding alongside deployment identities
and holds, not the customer binding. There is no workflow database environment
variable or `--workflow-bundle`. Creators need not supply `APP_ID`. Local
co-location does not change the production database boundary.

Restart preserves queue metadata, journal receipts, pending intents and retained
bundles. Vite's hot reload republishes the archive and restarts the CLI, which
activates the new immutable deployment while existing generations keep their
pins. Local durability and failure behavior match production; replacing the
queue with an in-memory shortcut would defeat that parity.

## Operations, security and backpressure

### Startup and readiness

The workflow service validates configuration, schema fingerprints,
least-privilege grants, authentication/replay storage and required host
capabilities before admitting new work. It recovers durable jobs, scheduling
cursors and responsibility state from its own schema. Startup does not scan
creator databases or depend on a running worker. A health endpoint reports
process liveness; readiness reports whether the host can perform its required
authenticated metadata operations, including the worker-registry columns its
authentication reads.

A worker obtains its trusted runtime identity, joins with its token, and starts
its workflow host, which claims from the zone queue on its own; a claim against
a service that is not yet reachable is retried rather than fatal. There is no
registration and nothing to be assigned before the first claim. A workflow whose
artifact is unavailable remains observable and durable; it does not silently
run another deployment. Authentication or migration failure is a
startup/readiness failure, not permission to fall back to elevated credentials.

### Shutdown and crash recovery

On SIGTERM a worker drains HTTP and its workflow host side by side, both within
`worker.shutdown_timeout`. The host stops claiming at once; a claim already
pending runs to its reply, and every delivery it brings goes back unstarted, so
its row is claimable at once. Beside that claim the executions already running
are driven throughout, and finish and settle until one deadline counted from the
signal: `worker.shutdown_timeout` less one operation bound. Whatever is still
running then is cancelled, its native work joined through the executor shutdown
barrier and its delivery released within that last bound, so its row returns to
the queue at once rather than after its lease lapses; then the instance retires
itself at Control. Quarantine closes isolate admission; runtime shutdown must
still join native operations before capacity is reused or completion is
published. Committed outcomes and intents remain publishable even if shutdown
cannot reach the service.

The orchestrator's termination grace is a deployment requirement: it must cover
the drain and the retirement call. The worker refuses at boot a
`worker.shutdown_timeout` shorter than `MIN_SHUTDOWN_TIMEOUT`: one delivery
claimed just before the stop - its app's preparation, its execution at the
host's ceiling and its settlement - or, if longer, a claim pending at the stop
and the give-back of what it brings, plus the operation bound that releases
what the grace could not finish (`validate_shutdown_timeout` in
`crates/zeroship-worker/src/workflow_host.rs`). The worker states the grace it
needs as `termination_grace_secs` in its `--check-config` report, and
`deploy/compose/docker-compose.yml` sets the worker's `stop_grace_period` to
cover it. Work cut off by a shorter grace re-runs on another worker of the zone
once its lease lapses; that at-least-once delivery is the contract creator steps
already carry through their step idempotency keys
(`crates/zeroship-workflow/src/execution.rs`).

A crash expires delivery authority while durable work and the app's recovery
duty remain: unrenewed leases lapse and their rows are claimed again, and the
instance's identity lease lapses on its own. Service replicas stop new
admission, stop initiating capacity work, and allow owned transactions to settle
or become explicitly uncertain. Timers, job leases, submission receipts and
obligations live in storage, so another replica can continue. Neither shutdown
path deletes durable work merely to make a process exit cleanly.

### Backup, restore and disaster recovery

Each database owner backs up its own state. Platform recovery includes the
queue, schedules, scope responsibility, the journal and its payload references,
enrollment, deployment metadata and holds; creator recovery includes the
creator's own data and objects. Retained bundles must remain available for every
recovered pin. Backup access does not give a platform workflow process
permission to read a creator database.

A process restart is different from restoring an older database snapshot. An old
snapshot can resurrect already settled jobs, lose receipts or reinstate
withdrawn leases. At-least-once delivery and ordinary lease expiry alone do not
repair that loss. Independent database restores also cannot be treated as a
consistent snapshot of both zones.

Restore therefore closes admission and fences prior process and lease authority
before replay resumes. Operators must reconcile manager obligations and journal
receipts through delivered maintenance jobs, validate payload and code
availability, and retain deployment holds while dependencies are uncertain.
Never advance a manager cursor or mark an app drained merely because restored
metadata is empty. If receipts were lost, external-effect duplication must be
handled by the destination's durable idempotency contract or explicit operator
remediation.

The restore-epoch and reconciliation handshake is a required operational protocol
still to be defined and tested with the storage owners. Routine crash-recovery
tests are not evidence for snapshot-restore safety. No operator recovery path may
replace the private-zone boundary with direct platform access to creator storage.

### Backpressure and fairness

Bound request/response metadata, claim batches, payload preparation, live runs,
replay/frontier size and worker slots through their owning options. A worker
claims at most its free execution slots, and `worker.workflow_prepared_apps`
bounds the apps it keeps prepared. Queue depth is not permission to overcommit
a creator database.

**Fairness across apps is a per-worker round robin.** Each worker visits its
zone's apps with claimable work in app id order from its own cursor, one job per
app per lap; within an app, the persistent dispatch ticket orders jobs FIFO.
That is a task queue's fairness with the key fixed to the app, equal weights and
one partition. A hot app alone may fill a whole batch; beside a quiet app it
takes one job per lap like every other. The cursor gets liveness by
construction, because it passes an app that was skipped, and it keeps policy
checks off the database and adds no mutable state to `queue_scopes`; fairness is
per worker rather than global, which still bounds every app's wait by one lap
per worker. A stored global least-recently-served order was considered and
rejected: a skipped app would never move in it and would pin the head of every
page without a park state of its own, and every claim would write the scope row.

**Per-app concurrency is `max_running` across the zone.** The claim counts an
app's live `advance` leases before it locks the app and passes an app at its
cap; the journal's `tasks::assign` remains the authoritative fence, and a claim
that loses that race is deferred and given back with a short pause. A busy app's
claims, heartbeats and settlements contend on its scope row: claims skip that
contention, heartbeats and settlements wait as any lock does.

**Work that cannot run now goes back instead of sitting leased.** A journal
deferral, a journal that could not take a live grant and a preparation failure
each give the row back in the same request, with `deferred_until` set: exactly
the instant a run is due, the remaining validity of a policy observation that
switched dispatch off, a short pause at the concurrency cap, or a back-off that
doubles with each consecutive back-off up to a ceiling. Only a back-off counts
toward the next one's length, and the count resets when an attempt is counted:
on its first renewal or its interrupted release. A give-back never counts toward
`max_delivery_attempts`, so pressure cannot exhaust a job's budget.

**A spent grant is neither returned nor given back.** A grant whose lease the
claim's own work used up - the journal's acceptance, or the rest of the batch
before the reply - stays leased, and its row lapses together with any task
accepted under it that was not released. It is redelivered once the stored
deadline passes, counting no execution attempt and no back-off. The grant's
monotonic expiry is anchored before the clock read its stored deadline came
from, so it ends first; a give-back at that point would land or be refused
depending on how far behind the stored deadline it ran, and the lapse is the
outcome that does not depend on it.

**Work that will never deliver stays durable and is excluded, never deleted.**
Rows exhausted by `max_delivery_attempts` stay unsettled and are never claimed
again unless policy raises the budget; rows of a deployment the journal keeps
reporting unavailable and of an app no worker can prepare keep backing off; and
every row of an app whose policy withholds dispatch is passed. None of them
counts toward capacity demand, and each zone's target row counts them as
`exhausted_jobs`, `backed_off_jobs` and `withheld_jobs`. Live apps resolve them
through management (cancel or restart), a redeploy or an environment fix; a
deleted app's unsettled creator jobs are settled `Rejected` when the closing lane
abandons it. Silent deletion on retry exhaustion is not acceptable.

Reject new unaccepted work before its durable journal commit when local admission
limits require it. After acceptance, queue or transport pressure leaves publication
pending; it cannot erase the run. Manager rejection must distinguish invalid
metadata, capacity pressure, contention and temporary storage or authentication
unavailability. Scheduling and recovery process bounded pages and yield between
app scopes. Management and reconciliation run in the service's maintenance lane,
so they progress while execution admission is paused or saturated.

Within an app, the queue issues a persistent dispatch ticket when publishing a
new job and on each successful claim. Both use the app's `dispatch_cursor` under
the app lock and in the transaction that inserts or leases the job. Candidate
selection filters due times, live leases, give-back back-offs and calendar
prerequisites before ordering by `dispatch_order` and storage identity. A
delivered job whose lease expires therefore retries behind already waiting work,
and later arrivals receive later tickets so they cannot continually displace that
retry. `available_at` remains immutable eligibility metadata; it is not rewritten
to rotate work. Exact submission replay, heartbeat and settled acknowledgement
replay allocate no ticket. Failed or cancelled claim transactions roll back the
cursor and job together; a lost response after commit preserves the rotation
across host restart. Counter exhaustion refuses the operation rather than
wrapping or reusing tickets.

### Observability and trust limits

Correlate app, zone, deployment, job, delivery attempt, run generation and
command identity where relevant. Emit closed reason codes for authentication,
capacity, stale fences, contention, unavailable artifacts, publication backlog
and recovery. Each claim names every app it skipped and why in its
`ClaimReport`, which the service logs at debug level. Each zone's capacity target
row records its backlog depth, the oldest claimable row's availability and the
undeliverable counts. Measure queue age, retries, give-backs, publication
progress, capacity demand, execution budget termination and hold/collection
progress through native metrics. Identifiers belong in appropriately scoped
traces; avoid unbounded metric labels.

Platform logs and metrics contain no customer inputs, outputs, history, signals,
raw database diagnostics, credentials, signed assertions or remote response bodies.
Detailed workflow errors stay in the journal and authorized creator views.
Metering remains trusted runtime infrastructure, never a customer-supplied result.

Remote service transport uses authenticated TLS and validates the intended
endpoint and audience. The native client admits HTTPS, literal loopback, and the
exact origins an operator lists in `plaintext_peers`, which is empty by default;
it rejects redirects, bounds streamed responses and returns closed failures.
Internal TLS is the end state that allowlist defers rather than replaces. Body
closure and response identity checks apply to nested metadata as well as
top-level envelopes.

**The execution zone is the isolation unit.** Crate boundaries make authority
reviewable; database grants, network isolation, trusted runtime bindings and
transaction predicates enforce it. A compromised worker in zone Z can read the
environment (decrypted secrets), project data key, bindings and metadata of every
Z app, because Control answers those reads for any live instance of the zone,
though it holds them in memory only for the apps it currently holds. It can reach
the creator databases and payload store the process is configured with; claim
`advance` jobs of any Z app and report forged executions, which the journal
validates before committing (fences, frontier, sizes, payload references), so a
forged report can make a run appear to have done anything its creator code could
have done and no more; start, signal, transition and restart any Z app's runs and
read their outputs; and hold Z leases up to the attempt cap to delay runs.

It cannot reach another zone: the claim pages only the zone frozen on its
instance row, the run path refuses an app of another zone, and Control refuses
it another zone's app reads. It holds no Control or journal credential. It
cannot claim a maintenance kind, because `Claimant::Worker` refuses them in the
query; publish anything, because no worker path publishes and a settlement
carries no outcome or successors; settle, renew or release another worker's
delivery; read a receipt of a job it does not hold; or outlive revocation,
because every call re-reads the instance row and `PURGE` retires a signer's
fleet. Its journal reach therefore equals the credential reach the zone already
grants. If deployment policy requires isolation from another app's compromised
host, place those apps in separate execution zones. A job or a body must never
expand a process's authorized app set or give it platform database credentials
or another holder's platform authority.

## Failure behavior and verification

| Failure or race | Required behavior |
| --- | --- |
| Input staging or acceptance transaction fails | Return failure without an accepted run; retain collectible preparation state where needed. |
| Acceptance commits but publication or response is lost | Request receipt preserves the run; its intent and pre-existing scope duty recover publication. |
| Publication wake keeps failing | The manager's reconciliation deadline still produces recovery work in the service's maintenance lane. |
| No worker of the zone is free | Accepted jobs wait in the queue; the zone's capacity target counts them as demand and asks the provider for slots. |
| A worker of another zone claims or calls for an app | The claim never pages that app and the run path refuses it `PermissionDenied`; Control refuses that worker the app's environment. |
| A deleted app's handle is still held | Run calls are refused to every worker within one policy observation's validity; claims pass the app; the closing lane settles its unsettled creator jobs `Rejected`. |
| Manager crashes after submitting a job but before replying | Same job identity returns the stored submission result. |
| Worker crashes before the journal commits | The lease lapses and any worker of the zone claims the job again, reclaiming the frontier without assuming a result exists. |
| Worker gives up on a claim before its reply arrives | The deliveries the service committed stay leased and lapse after one lease window, uncounted because they never renewed. |
| The journal defers a claimed job | The claim gives the row back in the same request with a `deferred_until` matching the reason; the attempt counts nothing. |
| The worker cannot prepare a claimed app | The delivery is given back with a back-off that grows per consecutive back-off, and the worker excludes the app from its own claims until a local expiry; the row counts toward no attempt and no capacity demand while it backs off. |
| Another session holds an app's scope locked during a claim | On PostgreSQL the claim skips the app as contended and serves the next one inside the same batch; on SQLite the database-wide write lock serializes the claim. |
| A claim's grant is exhausted by the journal work before the reply | The grant is neither returned nor given back: its row lapses with any unreleased task accepted under it and is redelivered once the stored deadline passes, counting no attempt and no back-off. |
| An execution runs toward its attempt cap | Heartbeats never extend the lease past `leased_at` plus the cap, and the worker's hard bound is the smaller of its local ceiling and the delivered attempt remainder. |
| Dispatch is switched off while an execution runs | Its next heartbeat extends nothing and the execution is interrupted at that renewal; its delivery is released back to the queue. An attempt that renewed before the interruption was counted at that renewal. |
| The last holder of an app on a worker drops | The app's project key, bindings and environment are withdrawn from the worker under the residency lock; a holder still executing keeps them. |
| Journal COMMIT is uncertain | Read the journal receipt after settlement; never infer rollback from timeout. |
| Worker's execution commits then loses its acknowledgement | Redelivery settles from the journal's receipt without executing the committed turn again. |
| Publication races reconciliation | Shared immutable IDs deduplicate; changed successor content conflicts atomically. |
| Manager COMMIT is uncertain | Retry exact settlement; the stored receipt determines whether it committed. |
| Enrollment is revoked during a lock wait | Fresh authorization rejects new mutation; database clock and original budget remain binding. |
| An old app handle or delayed install survives binding replacement | The retained binding/ticket fails; neither captures the replacement's authority. |
| The service's cached policy source expires | Further admissions fail closed until a new observation; a worker's requests cannot refresh stale source validity. |
| Old heartbeat reply is lost | Retry can observe/renew a still-live stored lease; it cannot revive locally expired execution or restore elapsed budget. |
| Old delivery tries to settle or give back a replacement | The `(job, worker, attempt)` fence rejects it; settled-receipt replay admits no new writes. |
| Concurrent cron replicas or delayed activation | Occurrence/cursor transaction and activation revision prevent duplicate or retargeted work. |
| Signal races timer, restart or child completion | Journal wait/generation predicates select the valid transition and preserve durable events. |
| Pause is rejected or an old command acknowledgement arrives | Only the matching provisional barrier changes; newer lifecycle authority survives. |
| Payload promotion races collection | Serialized collection state prevents deletion of committed references. |
| Artifact reclamation races hold acquisition | Deployment fence decides admission; pending jobs/history retain their distinct holds. |
| Capacity provider or creator database is unavailable | Demand and accepted jobs remain durable; no cross-zone database fallback occurs. |
| A worker is stopped mid-execution | It cancels, joins and gives back the execution within its drain; one cut off by a shorter termination grace re-runs after its lease lapses. |
| A database is restored to an older snapshot | Fence prior authority and reconcile both owners' durable state before reopening admission; missing receipts require explicit duplicate-effect handling. |

Native Rust tests exercise behavior through the owning crate, with PostgreSQL and
file-backed SQLite for ORM contracts. Required external environments belong to
Testcontainers using major image tags. Unavailable required infrastructure fails
tests; it does not turn them into optional checks. Examples own their Vitest,
TypeScript fixtures and Playwright suites. Use the workflow xtask/native test
selection rather than adding bash orchestration or source-text gates.

| Test owner | Required evidence |
| --- | --- |
| Core/identity | Closed nested variants, malformed IDs/revisions, stable wire identity and no customer fields, including `ClaimJobs`/`ClaimedJobs`. |
| Manager | The zone claim on PostgreSQL and SQLite: only the caller's zone, one job per app per lap from a cursor, a null cursor starting at an existing app, exclusions, skips for disabled, capped, deleted, exhausted and barrier-blocked apps, a cap counted again under the app's lock, contention reported as `Contended` on PostgreSQL and never on SQLite while a maintenance claim waits for the lock, the delivery and exclusion bounds, an enrollment check that cannot be answered skipping only its app, and the batch deadline with give-backs bound by it and never waiting for a lock. Give-back back-off counted only by back-offs and reset, an elapsed deferral claimable at once, the attempt cap, capacity census over the apps with creator work and its cycle across passes, target rules, sparse recovery pages, atomic command/queue changes, stable receipts and storage IDs, authority after lock waits, shortened budgets and uncertain-commit retry. |
| Scheduling | Calendar/DST parity, catch-up and overlap semantics, activation without a running worker, schedule replacement/disable, stable occurrences and due-work recovery. |
| Customer engine | Acceptance/outbox atomicity, duplicate delivered jobs, stale frontiers, deferral reasons, a run no clock will wake settled rather than deferred, receipt retention, policy expiry, signal/child races and generation-safe restart. |
| Worker/runner/V8 | Batch claims for the free slots, up to the delivery bound, continuing from the cursor, a stop that lets a pending claim reach its reply and gives back what it brings, prepare on first delivery and reuse, idle-only eviction, preparation bounded by the lease, give-back and exclusion on preparation failure, residency withdrawal, pinned complete module graph, bounded execution at the delivered attempt bound with its settlement reserved inside it, a namespace that refuses every call without a manager, and a stopped-native-work barrier. |
| Client/server | Real authenticated HTTP, instance-versus-role keys and zones, replay protection, enrollment changes during waits, zone refusals on run calls with an unknown app refused like a foreign one before any observation, give-backs in the claim request, a claim exchange bounded by its stated wait, a reply at the protocol bound received, TLS, streamed bounds and reuse after cancellation. |
| Retention/payloads | Upload failure, corrupt reads, uncertain reference promotion, concurrent collection, queue-versus-journal holders and delayed generation messages. |
| Host integration | Private disjoint database access, no forbidden grants, a worker that never ran an app serving and executing it, and restart across both commit boundaries. |
| Local/examples | Same protocol and durability with local native composition; each example's creator-facing behavior through its own tests, on both tiers. |

Lock/cancellation tests observe the actual blocked operation and its storage
result rather than sleep and assume progress. Preserve the distinction between
rollback before COMMIT and uncertainty after terminal dispatch. Dependency gates
inspect normal Cargo edges so workers cannot import manager/server persistence,
platform services cannot reach the runner or V8, and clients cannot reach either
ORM store. Test fixtures may compose both zones.

## Implementation progress and remaining decisions

### Implemented foundations and current gaps

**Zone-scoped pull, end to end.** A worker claims batches of its zone's
claimable `advance` jobs, prepares each claimed app on demand and executes it;
the service authorizes run calls by the worker's zone and runs every other job
kind in its own maintenance lane. The decisive process contract is
`a_worker_that_never_ran_the_app_serves_and_executes_it`
(`crates/zeroship-control/tests/workflow_two_worker_e2e.rs`): with W1 stopped and
no claimable `advance` row left for the app, a second worker that never held a
job of the app reads the first worker's run and has a fresh start admitted and
completed, which only the zone can authorize.
`a_worker_host_runs_a_run_started_through_ordinary_app_ingress`
(`crates/zeroship-control/tests/workflow_worker_host_e2e.rs`) completes a run
started through ordinary app ingress, and
`the_two_zones_run_a_workflow_without_reaching_each_other`
(`crates/zeroship-control/tests/workflow_private_zones_e2e.rs`) holds the
private-zone boundary with the journal in the platform zone.

**The workflow service.** `crates/zeroship-workflow-server/tests/integration/http_claims.rs`
drives batch claim, heartbeat and settle through real authentication
(`a_batch_claim_heartbeat_and_settle_round_trip`), gives a deferred claim's row
back in the same request, backs a preparation failure off further with each
give-back, passes an app whose scope another session holds locked, passes a
deleted app, keeps an exhausted grant out of the reply and leaves its row to
lapse, and waits out the observation that switched dispatch off.
`tests/integration/http_runs.rs` pins
the zone rule (`a_worker_that_never_claimed_the_app_serves_its_zones_apps`,
`a_run_call_from_another_zone_is_refused`,
`a_deleted_app_is_refused_to_every_worker`, `a_retired_instance_is_unauthenticated`,
`an_unknown_app_and_a_foreign_app_are_refused_alike_before_any_observation`) and
the carried-forward ingress epoch. `tests/integration/worker_zone.rs`
verifies two instances in two zones with their own zones and fails readiness when
the zone column grant is revoked. `tests/integration/maintenance_lane.rs` covers
the lane's claims, its staged objects, its bounded turns and the process cadence
reaching it; `tests/integration/publication_wake.rs` publishes starts, signals,
settlements and child completions before reconciliation. `apps_are_out_of_reach`
and `deployment_catalog_is_out_of_reach`
(`tests/integration/platform_schema.rs`) hold the role's reach into `zeroship`
to the instance columns authentication reads, and `tests/e2e/config.rs` refuses
an attempt cap shorter than the lease. All of these are in the
`crates/zeroship-workflow-server` test target.

**The manager.** `crates/zeroship-workflow-manager/tests/integration/` holds the
queue contracts on PostgreSQL and SQLite (`queue.rs`: competing claims,
concurrent scope registration, redelivery, receipts, revocation rollback, a
refused kind at the head not hiding the rows behind it, sweeps left to the
claimant owning the journal, and the delivery budget), dispatch fairness within
an app (`dispatch_fairness.rs`), the capacity rules (`capacity.rs`: demand up to
`max_running`, undeliverable rows counted and never demanded, a disabled app
without demand, a frozen target on an unavailable observation, a partial visit
that raises and never lowers, a zone measured across visits whose completed
cycle lets the target fall, an idle scope never observed, hold-down, the steady resynchronization and the
static pool refusing exactly above its slots), closing, recovery and
retirement, management, retention and scheduling. `storage_classification.rs`
reports lock contention as `Contended`.

**The runner and the worker.**
`crates/zeroship-workflow-runner/src/delivery/tests/consumer.rs` asks for the
free slots and continues from the cursor, gives back and excludes an app that
failed to prepare until its expiry, and keeps an abandoned host's execution and
app until the drain finishes; `src/prepared/tests.rs` prepares on first delivery
and reuses the entry, evicts only idle entries, keeps a pruned entry resident
while an execution holds it and bounds preparation by the delivery lease;
`src/delivery/tests.rs` cuts an execution at the delivered attempt bound under a
longer local bound and interrupts at a renewal that extends nothing;
`src/host/tests.rs` claims the zone for every free slot.
`crates/zeroship-worker/src/residency/tests.rs` withdraws the key, bindings and
environment when the last holder drops, keeps a request reading encrypted data
after the cache drops the app, and lets a refresh hold only an app something
already holds; `src/workflow_host/tests.rs` refuses a shutdown timeout below one
execution at the ceiling plus its settlement.

**The customer engine** keeps its native PostgreSQL and SQLite contracts:
ordered management with atomic receipt and order application, delivered
collection with fixed cutoffs and a final resweep, topic fanout pages, dependency
propagation pages, activation and cron acceptance against retained bundles,
restart against the journal's own deployment holds, publication intents and
their exact confirmation, and delivered advance acceptance with retained
semantic replay. The service runs every one of those maintenance operations
through `AppWorkflows::maintenance_job`; a worker runs only `advance`.

**Local host.** `crates/zeroship-cli/src/workflow/tests.rs` starts the host on a
real compiled archive: the delivered Activation selects the archive, a run
started through the host's ingress completes through the zone claim while the
workflow thread runs metered `env.db` isolates, a sleeping run resumes after a
restart from queue metadata alone, a republished archive activates at the next
revision while existing runs finish on their pinned code, and an idle app retires
through a delivered Close and reopens on its next start.
`crates/zeroship-cli/tests/e2e/workflow_local.rs` repeats restart after process
death through the real `zeroship serve` binary.

**Gaps, each a contract still to write or a decision still open:**

- Providers that start processes wait for the production orchestrator, and the
  gateway routes over one static worker list, so a deployment with more than one
  execution zone needs zone-aware routing.
- Control does not publish deletion as a lifecycle intent, and snapshot restore
  still needs its reopening contract.

### Decisions still requiring an explicit contract

| Decision | Fixed requirement and remaining choice |
| --- | --- |
| Zone eligibility and capacity | Decided: an app's and an instance's frozen execution zone authorize claims and run calls through one rule, and each zone's declarative target follows its policy-filtered backlog through an injected provider. Providers that start processes remain, pending the production orchestrator. |
| Dispatch fairness and persistent failure | Decided: a per-worker cursor visits a zone's apps in round robin, one job per app per lap, FIFO within an app. Work that cannot run now is given back with a back-off; work that will never deliver stays durable, is excluded from claims and demand, and is counted per zone on the target row; a deleted app's unsettled creator jobs are settled `Rejected`. Management is a maintenance kind the service claims, so it never competes with worker claims. |
| Scale-down and termination grace | Decided: scale-down is the orchestrator's, and the termination grace is a stated deployment requirement; work below it re-runs under the at-least-once contract creator steps carry. Deriving the grace from the attempt cap and validating it where deployment configuration is generated remains. |
| Archive acknowledgement | Control's facts capability provides bounded convergence under original observation validity. Define any stronger execution-quiescence evidence separately from calendar acknowledgement or lease expiry. |
| Complete job envelopes | Operation-specific deployment prerequisites, frozen manager restart targets and linked management outcomes are implemented. Collection, topic fanout and dependency propagation have durable pages, receipts and the service's delivered handlers. |
| Receipt retirement | Define admissibility fences and publication/settlement watermarks before deleting job deduplication state. Retain it until that proof exists. |
| Snapshot restore | Define restore epochs, fenced admission and cross-owner reconciliation with the storage owners; process-restart recovery alone cannot protect lost receipts or resurrected authority. |

These are design decisions, not unspecified permission to improvise in separate
implementations. They do not reopen the database boundary or require another
broker/service. Mid-run upgrade has its own unresolved semantic contract and is
outside this design.

### End-to-end path

The production executables and the local host compose the same native
libraries, and each path ends in a process contract rather than library breadth:

1. **Local host on the native manager.** `zeroship serve` and the Vite
   development host compose the manager, its driver and the zone claim over the
   local platform metadata file, with `ConfiguredPolicies` and `LocalCapacity`,
   and run the ordinary `JobConsumer` and their own maintenance lane beside the
   journal. Proof: the CLI `workflow::` contracts and the local tier of both
   example suites.
2. **Normal deployment publication.** Control's deploy transaction records an
   idempotent command receipt and a lifecycle intent together, and its publisher
   delivers register, activate and disable, each naming the app's zone, and
   confirms only exact receipts. Proof: Control's deploy and publication
   contracts against the signed manager routes.
3. **Workers pull from their zone.** The production worker runs one `WorkerHost`
   on a dedicated compio thread with the enrolled instance signer, prepares apps
   on demand under `AppResidency`, serves `env.workflows` for any app of its zone
   through `RemoteWorkflows`, and drains on SIGTERM. Proof: the two-worker,
   worker-host and private-zone process contracts above.
4. **Capacity from the zone backlog.** The driver's capacity lane keeps each
   zone's target from its policy-filtered backlog and applies it through
   `StaticPool` in deployments that start their own workers. Proof: the
   manager's capacity contracts and the server's driver contracts. Both example
   fleets start a workflow service (`examples/workflow-probe/tests/fixture/settings.ts`,
   `examples/workflows-order/tests/fixture/settings.ts`); their deployed tier passing
   under zone pull is the proof this path still owes.

Integrate shared ORM changes from their owner rather than introducing
workflow-specific replacements.
