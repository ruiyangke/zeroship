# Runtime DB binding: the decision log

The history of `2026-08-26-runtime-db-binding-design.md`. Every decision and
every correction, newest first, with the evidence that settled it.

**Why this is a separate document.** The design accumulated forty-three
retraction markers - `RETRACTED`, `SUPERSEDED`, `CORRECTED`, and struck-through
prose - each appended beside the claim it corrected. Individually each was
defensible; the error was instructive and the retraction preserved it.
Collectively they meant a reader had to reconstruct the current design by
mentally applying forty-three corrections in order. The specification now states
only what is true; this document holds what it took to get there, and it is
entered on purpose rather than met by accident.

**Entry shape.** Each entry says what was believed, what is true, the evidence
that settled it, and - where it generalises - what class of mistake it was. The
last part is the reason this file is worth keeping: several of these mistakes
recur in the tree under different subjects.

**Citations here are historical.** They were correct against the tree on the
date of their entry and are deliberately not re-pointed. Three that the design
carried had already drifted by 2026-08-28 (`Pool::connect(&url, 8)` from
`lib.rs:862` to `lib.rs:998`, the per-deprovision pool from `lib.rs:904` to
`lib.rs:698`, and the "~200 isolates per OS thread" note from `exec.rs:1273` to
`exec.rs:1321`), which is itself the lesson: a citation is a measurement, and
measurements age.

---

## 2026-08-30

### Three operator decisions that delete a design rather than add one

Taken in conversation, in response to "why do we need the internal api for db migration?" - a question
whose honest answer was that we did not, and that the argument the proposal gave for it did not hold.

**11. The control plane does not forward migrations, and is not responsible for them.** "no forward,
and the control service is not responsible for the migration."

The now-DELETED `crates/zeroship-control/src/migrations_api.rs` (214 lines) authorized the caller for the app
and re-issues the request to `zeroship-migrate-server`. That proxy is deleted; the CLI calls the
migration service directly.

**This costs almost nothing to remove, because the migration service never trusted control's
authorization in the first place.** `crates/zeroship-migrate-server/src/api.rs:98` calls
`verify_action(token, app_id, Action::AppsDeploy, ...)`, backed by `ControlPlaneAuthenticator`, which
holds its own `control_pg` client, its own `PolicySet` and its own `BearerVerifier` (`auth.rs:42-46`)
and reads the creator's bearer directly. The forward added a hop and an authorization that was
already being done one service later. Removing it is deletion, not redesign.

It also retires a hazard this page raised the same day: that an id-bearing "internal" route might one
day accept the control key and hand platform authority to anyone holding a `dbs_...`. There is no
internal route to confuse, and the surviving route already demands the caller's own bearer.

**12. `zeroship-migrate-server` is re-keyed from app id to database id.** "we need to refactor the
migrate-server as well to use the latest database id rather than app id."

116 occurrences across seven files - `schema_apply_store.rs` (35), `policy.rs` (32), `apply.rs` (19),
`auth.rs` (11), `api.rs` (9), `publication.rs` (6), `provisioning.rs` (4). Four routes move from
`/v1/apps/{app_id}/migrations/{apply,plan,status,rollback}` to `/v1/databases/{database_id}/...`,
though only `apply` is live; the other three are `stub_phase2`.

**The count is not the work.** `auth.rs:79` builds `Resource::App { id }`, and that becomes
`Resource::Database { id }` with the policy moving from "may deploy this app" to "owns this database".
That is the change that needs care. The rest is mechanical.

**13. A database is addressed BY ID, always. There is no `(project, name)` resolution.** "we should
not use project + name to resolve the db, always use database id."

**This REVERSES operator decision 6 of 2026-08-29**, which held that the database id is internal and
never exposed to creators. That decision is withdrawn, and the two-route split it justified -
`POST /v1/projects/{project}/databases/{name}/migrations/apply` for creators against
`POST /v1/databases/{database_id}/migrations/apply` internally - dies with it. One route remains, and
it takes the id.

The reversal is coherent rather than a change of mind about the same facts. Decision 6 rested on "a
route that takes the id in its URL is an exposure", which was an argument about the FORWARD: while the
control plane stood in front, hiding the id was free. Decision 11 removes the thing in front, and with
it the premise. What is left of the id-hiding case is ergonomic - keeping `dbs_...` out of error text
and generated types - and that never justified a service boundary.

It also matches the pattern already shipped for apps rather than inventing a second one:
`schema/project-v1.json:33` defines `app` as "The deploy target's app id (uuid) or name. Absent on a
fresh project: the first `zeroship deploy` auto-creates the app and appends its id here." A database
now works the same way, in the same file.

**Two open tasks are dissolved by this, not deferred:** keeping the database id out of every
creator-facing surface, and the finding that "the database id is internal" was asserted rather than
enforced across three leaking channels. Neither describes a defect any more; the id is public by
decision.

**What does NOT dissolve is ownership.** The authorization policy is still "principal may migrate N
iff principal owns N", so the project/ownership row is still required - it was the ADDRESSING that
depended on `(project, name)`, not the authorization. Read this decision as unblocking the route
shape, never as unblocking authz.

**14. The CLI reuses the control URL; the EDGE routes `/v1/*` to the migration service.**
"can we reuse the control url?" Yes, and investigating it surfaced a gap decisions 11-13 had left
open.

**THE MIGRATION SERVICE IS NOT EXPOSED AT THE EDGE AT ALL.** `deploy/ops/Caddyfile` routes
`auth.<domain>` to `auth:9092`, `control.<domain>` to `control:9090`, and three host blocks to
`gateway:8000`. There is no `migrate-server` block. `deploy/compose/docker-compose.yml:450-457`
publishes it on `127.0.0.1:9091`, which the comment at `:361` describes as the loopback "an operator
tunnels to", while control reaches it on the compose network at `http://migrate-server:9091`
(`:365`).

So the CLI cannot reach the migration service today by any URL. Deleting the forward does not merely
leave the CLI without a config key - it leaves it without a ROUTE. That gap was not in the task this
page filed for decision 11, and would have shipped in the brief.

**The fix is path routing at the edge, following the precedent one block above it.** `auth.<domain>`
already splits by path - `handle /oauth2/*` and `handle /.well-known/*` before its catch-all - so the
control host gains the same shape:

```
http://control.{$ZEROSHIP_DOMAIN} {
	handle /v1/*           { reverse_proxy migrate-server:9091 }
	handle                 { reverse_proxy control:9090 }
}
```

The CLI then reuses `control_url` verbatim: no new key, no new flag, no new environment variable, no
new absent-key error path, and nothing new for a `zeroship.jsonc` to omit.

**A SHARED HOSTNAME IS NOT THE CONTROL PLANE BEING IN THE PATH, and this needs saying because it
reads like the forward decision 11 just deleted.** The request never reaches control: Caddy hands it
straight to `migrate-server`. No control code runs, no second authorization happens, nothing is
re-issued. Decision 11 is about a SERVICE in the path, not a DNS name. Anyone who reads "migrations
go to control.<domain>" and concludes control should proxy them is rebuilding what was deleted.

**Two costs, accepted deliberately:**

- **The edge config becomes load-bearing.** Deploy without that rule and `zeroship migrate` receives
  control's 404. Combined with the empty-apply ledger write (task #74), a migrate that reaches nothing
  can still look like it did something, so the two failure modes compound.
- **A path-collision invariant appears.** The control plane must never define a `/v1/*`
  route, and nothing enforces that. It gets a gate arm rather than a convention.

`deploy/ops/Caddyfile` is the LOCAL edge; production may be Kubernetes, so the same rule has to exist
in whatever ingress ships there. That is a second place to get it wrong and is stated here so it is
not discovered in production.

**15. `control.<domain>` KEEPS its name for now.** Raised as "I don't like the control.domain, I want
it to be api.domain", investigated, and declined on evidence rather than on preference. Recorded
because a rejected rename returns every few months, and the reason it was rejected is the part that
gets lost.

**`api.<domain>` is already the gateway, and it is load-bearing in a way a hostname usually is not.**
`deploy/ops/Caddyfile:79-80` routes it to `gateway:8000`, and `https://api.zeroship.ai` is the
gateway's `--public-url` default (`docs/reference/env-vars.md:345`) - which is the **`iss` claim of
the tokens the gateway signs**. It is hardcoded as the issuer in three shipped harnesses
(`tests/e2e_auth_rpc.sh:143`, `tests/e2e_dev_vs_deployed_auth.sh:555`, `tests/golden_path.sh:4067`)
and one integration test (`crates/zeroship-gateway/tests/oidc_rp_e2e.rs:43`). Repointing it is not a
routing change; it moves a cryptographic identity, and every already-issued token's `iss` stops
matching.

**What is behind it, enumerated rather than assumed.** Nine registered endpoints, of which exactly
ONE is a proxy: `/__zeroship/internal/workflow-advance`, which looks up the route, enforces account
and spend state, and forwards to a worker over the hash ring
(`crates/zeroship-gateway/src/router/dispatch.rs:105-160`). The other seven TERMINATE at the gateway -
`/healthz` and `/readyz` (`health.rs:22-25`; `healthz` is a constant 200 so a control-plane outage
cannot kill every container), and the browser identity surface under `/__zeroship/auth/*`, which mints
and re-signs session cookies with zero outbound calls in `auth_token.rs`, `browser_auth.rs` or
`backchannel_logout.rs`. The remaining two registrations are dispatch catch-alls, not endpoints.

So `api.<domain>` is the END-USER identity and session plane. Moving creator resource management onto
it would put the console's operations on the origin end-user browsers authenticate against, sharing
cookies. That is an origin-boundary change, not a rename.

**Two counting errors made while establishing this, both worth the warning.** The endpoint count was
first reported as seven: `.configure(health::configure)` and `.configure(backchannel_logout::configure)`
register indirectly and do not match a `web::resource("...")` grep. And `/healthz` and `/readyz` are
not under the `__zeroship` prefix at all, so "all endpoints are under `/__zeroship/`" was wrong twice
over. Enumerating a route table by grepping literal path strings in one file misses every
`configure`-style registration.

If `api.` is wanted for creators later, the honest move is renaming the GATEWAY's host and freeing the
name - a token-identity change deserving its own task, never a line item inside a database refactor.

**16. THE MIGRATION SERVICE IS REACHABLE FROM OUTSIDE.** "the migrate-server is accessable to outers."
This confirms decisions 11 and 14 against the objection raised on 2026-08-30 and settles it.

**THE OBJECTION, RECORDED BECAUSE IT WAS SERIOUS.** The deleted forward's own header
(`crates/zeroship-control/src/migrations_api.rs`, now DELETED) stated the opposite topology outright: "`migrated`
binds loopback in every deployment we ship (`ports: 127.0.0.1:9091:9091`) because it holds the
SUPERUSER provisioning DSN - it is the one service that may `CREATE SCHEMA` and `CREATE ROLE`", and
quotes the compose comment: "nothing outside the compose network should reach it. Creators drive it
through control." The brief that ordered the deletion asserted the reverse - that the service was
exposed nowhere as an oversight to fix - and was written without reading that header. The isolation
was deliberate.

The decision overrides it knowingly. The service authorizes every request against the caller's own
bearer with its own `PolicySet` and `BearerVerifier`, so reachability was never what protected it.

**THREE PROTECTIONS DIE WITH THE HOP AND MUST BE REPLACED, NOT NOTED.** They were found by the agent
that performed the deletion, arguing against its own change:

- **A per-IP rate limit on the one endpoint that runs `CREATE SCHEMA` and `CREATE ROLE` as superuser.**
  `migrations_api.rs:90` called `admin_rate_limit`, which is `http_util::rate_limit` over the `admin`
  bucket at a BURST OF 30 WITH A STEADY 60 PER MINUTE (`crates/zeroship-control/src/main.rs:999`,
  `Quota::per_minute(30, 60)`). THIS PARAGRAPH SAID "30 requests per minute" UNTIL 2026-08-31, and so
  did every task record and status report that quoted it. The two constructor arguments are
  `per_minute(burst, per_minute)`, which becomes `capacity: burst` and `refill_per_sec: per_minute /
  60.0` - so the pair is a capacity of 30 refilling at one token per second, not a rate of 30. The
  error was caught by the agent that rebuilt the limiter, arguing against the brief that carried the
  wrong gloss. Postgres-backed and proxy-aware. Direct applies are otherwise
  unthrottled. The limiter is portable - it takes a request, a PG handle, a bucket name, a quota and a
  trust-proxy flag, and the migration service already holds its own `control_pg` client.
- **Source-IP propagation.** The migration service sets `request_ip: None`, losing IP-policy input and
  audit context. Behind the edge it sees Caddy, so `trust_proxy` must be got right or the platform
  rate-limits itself as one client, or trusts a spoofable header. This is the risky part, not the
  limiter.
- **First-seen platform CLI grant materialization.** Direct UUID applies can stay on the default grant
  fallback until some other control request materializes rows.

Exposure without those replacements is a downgrade, not a refactor. The rate limit in particular
guards a superuser DDL path that is now publicly routable.

### 2026-08-30, later: the lifecycle home, settled by two independent reviews rather than by a ruling

**17. DATABASE LIFECYCLE LIVES ON THE MIGRATION SERVICE. CONTROL IS NEVER IN THE PATH AND HOLDS NO
PROVISIONING DSN.** `create`, `list`, `delete` and `bind` are creator-facing routes under
`/v1/databases/*` on `zeroship-migrate-server`, reached at the edge under the `control.<domain>`
origin per decision 14. The migration service executes every piece of privileged DDL. Control's
relationship to a database narrows to READING lifecycle state for the deploy gate and the runtime
binding injection.

This is not a new ruling. It is what decisions 11, 14 and 16 already imply taken together, and it was
reached independently by two reviewers from different starting points, which is why it is recorded as
settled rather than proposed.

**THE ALTERNATIVE THAT WAS TESTED AND FAILED.** "Control owns the lifecycle API and calls the
migration service to execute the DDL" was put to both reviews as the leading candidate, with the
argument that it differs from the deleted forward because the payload is a database id rather than
creator SQL. Both rejected it, and the argument that killed it is not about payloads:

- The forward was deletable because *the migration service never trusted control's authorization in
  the first place* (decision 11). The test is what the intermediate CONTRIBUTES, not what it carries.
  Control forwarding a bearer contributes reachability and a cheap rejection and no authority - that
  is the forward, whatever the body holds.
- The other fork is worse. If control acts on its own behalf with `control_key`, it re-creates by
  name the hazard decision 11 retired: an id-bearing internal route that hands platform authority to
  whoever holds a `dbs_...`.
- Of the four lifecycle verbs, three (`create`, `delete`, `list`) reduce to "authorize, write a row,
  ask the migration service to run the DDL", and the rows live in a schema the migration service
  already writes (`zeroship.app_schema_applies`).
- The fourth, `bind`, must not be split from the apply's advisory-lock bracket. The apply reaps
  epoch `E-1` roles and mints `E+1` roles under a per-database session lock; a `CREATE ROLE` issued
  from another process between those points mints against an epoch about to advance, or creates a
  role the reaper's enumeration missed. One writer under one lock removes the race by construction
  instead of by a cross-service protocol.

**WHAT WAS ALREADY WRITTEN DOWN AND SHOULD HAVE ENDED THIS SOONER.** `deploy/compose/docker-compose.yml`
states in control's own environment block that it carries "NO provisioning DSN here. The privileged
CREATE SCHEMA / CREATE ROLE work belongs to migrate-server, which is the only code that reads one;
this service used to carry it and nothing consumed it." Option B is therefore not a candidate being
weighed - it is the reversal of a landed cleanup, and it would put a cluster-superuser credential
back in the service with 54 registered routes (27 `/api`, 23 `/internal`, 2 `/me`, `/healthz`,
`/readyz` - measured 2026-08-30) including the Stripe webhook and the device-login flow. The
migration service serves six.

**THE NARROWED FIRST STEP SHIPS NO NEW ENTITY, AND ONE REVIEW'S PROPOSAL TO MINT IDS EARLY IS
REFUSED.** The step that lands is: a `create` verb on the migration service under the
`/v1/databases/{id}` shape with `{id}` still derived from the app id; apply REFUSES when the schema
is absent instead of creating it; and no auto-create anywhere. The route shape, the service, the edge
rule and the refusal semantics are all end-state, so nothing is built twice - only the id's
derivation is interim, and decision 12 already schedules that as a mechanical re-key.

Minting `dbs_` ids in that first step was proposed and is REFUSED, because it is not free: section 7
of the decoupling proposal requires the encryption salt and AAD change to land "in the same change
that makes database ids exist, not after", since changing the salt changes every derived key and
changing the AAD changes every tag. Minting the id early drags wire version `0x02` in with it.

**THE REFUSAL IS 409, NEVER 404.** `crates/zeroship-cli/src/main.rs:917`
`should_resolve_or_create_after_deploy_failure` keys on 404 and auto-creates. A 404 on the database
path feeds the auto-create-and-retry loop and reintroduces automatic creation one layer up - the
exact thing this decision removes. It follows the shipped precedent of `schema_precondition_response`
(`crates/zeroship-control/src/api.rs:167`): a `remedy` field the CLI prints raw. The refusal must also
precede the ledger open, or a refused apply pollutes the head the deploy gate reads.

**STILL OPEN, AND NAMED SO IT IS NOT DISCOVERED LATER.** This paragraph originally said the corpus
"does not assign the executor for bind DDL, and points both ways". Two reviews said so independently
and a third refuted it; re-reading the passages myself, the refutation is right about the substance
and wrong about one detail, and the corrected version is sharper than either:

- The corpus DOES assign, and it assigns two DIFFERENT events to two different executors. Section
  2.2b mints a grant's CURRENT `zs_bind_<gid>_e<E>` at BIND time in "one control-plane transaction".
  Section 4's T4 mints every `zs_bind_<gid>_e<E+1>` at EPOCH ROTATION, inside the migration service's
  apply transaction, together with the epoch write, the publication widen and the marker. Different
  role instances, different lifecycle events - not a contradiction about who mints one role.
- Cost 6 IS loose, and that is the part the refutation got wrong by reading a distinction into it.
  It says "Every bind, unbind **and epoch rotation** is shared-catalog DDL serialized through the
  control plane" - a conjunction that sweeps rotation into control, contradicting section 4's T4 in
  the same document.

So the real finding is not internal contradiction, it is this: **section 2.2b assigns bind-time
`CREATE ROLE` to a control-plane transaction, and this decision forbids control from executing DDL.**
Decision 17 overrides that half. Bind-time minting moves to the migration service, which also removes
the race that splitting it would create against the apply's T1/T4 bracket. Whoever implements bind
corrects 2.2b, and corrects cost 6's rotation clause, in the same change.

Graceful delete is the second open seam: the teardown's subscription gate needs a cluster-wide count
only control can aggregate today, so `delete` either consults a control-maintained aggregate (a read,
consistent with this decision) or accepts force-shaped slot termination. Decide it explicitly rather
than letting it pull the delete API back onto control.

## 2026-08-29

### Three more operator decisions, two of which fix defects no review round found

Taken in conversation. Recorded here because the design pages and the task list carry only the
outcome, and tasks are working state rather than the durable record.

**7. Pairwise subjects are scoped to the PROJECT, not the app.** "we support pairwise, so the same
user might have different ids in different apps, let's scope the pairwise to project, all apps in the
same project see consistent user id."

Today the sector is per app: `crates/zeroship-auth/src/oidc/issuer.rs:849` derives the subject from
`(user_id, sector_identifier)`, and `sector_identifier` is a column on `zeroship.app_oauth_clients`
falling back to the client id (`crates/zeroship-auth/src/oidc/backchannel_logout.rs:129`). Correct
OIDC pairwise behaviour, and a privacy feature.

**It is also a correctness blocker on database sharing that five review rounds and two independent
re-key designs all missed.** With a per-app sector, two apps sharing a database resolve the same human
to different subjects:

    storefront writes  orders.user_id = <storefront's sub for Alice>
    admin queries      orders WHERE user_id = <admin's sub for Alice>   -> no rows

Same person, same database, two keys, and nothing errors. Invisible in single-app testing, because it
only appears when a SECOND app reads rows the first one wrote. Every reviewer reasoned about ACCESS to
shared rows; none asked whether the two apps agree on WHO A ROW IS ABOUT.

Consequence: this settles the workspace question. One re-key draft argued against a project row
because a workspace "has no server-side lifecycle". A project-scoped pairwise sector IS server-side
lifecycle - it decides identity resolution on every request and must stay stable for the life of the
data. So the project becomes a row regardless of the ownership question, and #47's shape 2 is the
answer rather than a candidate.

Implementation trap: `COALESCE(aoc.sector_identifier, oc.client_id)` silently reinstates per-app
scoping whenever the sector is unset. Under this decision an unset sector must be an ERROR, not a
default.

**8. A creator's bill survives deletion of the app that incurred it.** Raised as an exploit: "I
created an app, produced $10k bill, then deleted the app, the creator does not need to pay."

Verified, and it works today. Billing reads from `zeroship.usage_aggregates`
(`crates/zeroship-control/src/api.rs:1499`, `registry.rs:931`,
`crates/zeroship-control/src/proration.rs:166-171`), and
`db/migrations-ts/20260702000600_constraints_indexes_fks.ts:194` carries
`usage_aggregates_app_id_fkey -> apps(id) onDelete: "cascade"`, with the same shape on
`app_spend_state` (`:130`), `app_usage` (`:131`) and `app_usage_history` (`:132`). Deleting an app
deletes the rows its invoice is computed from - by referential action, with no code involved and
nothing logged. Invoice items already pushed to Stripe survive; anything since the last reconcile
cycle does not.

The wire format already disagreed with the schema, which is the tell:
`crates/zeroship-core/src/usage_event.rs:14-20` is `UsageSubject { app: Option<Uuid>, creator: Uuid }`
- creator non-optional, app optional. The wire says the creator owes; the FK said the app owns.

**9. An app with metered usage cannot be deleted, only archived.** "if user created an app, usage
metered, then can not delete, just archive."

This supersedes the fix proposed for decision 8. `ON DELETE SET NULL` would preserve the charge but
degrade attribution to "this creator, app since deleted"; archiving keeps the `apps` row, so the FK
never fires and the usage rows keep a VALID app id. Attribution stays exact.

It also matches the decoupling rather than fighting it: archive separates the APP lifecycle from the
DATABASE lifecycle, which is exactly the split the two-verb teardown introduces. An archived app stops
serving and gives up its grant; its database persists and accrues storage until the creator deletes
the DATABASE. "Stop running this app" and "destroy the data" are the same operation today only because
one app owns one schema.

The cascade fix is kept anyway as defence in depth. "No current API path reaches it" is the argument
that was wrong twice on 2026-08-29 alone - on the audit-table grants and on the unmask read path.

OPEN, and it belongs with the teardown split: under today's one-app-one-database model, archiving
while keeping data means the creator keeps paying storage with no way to stop short of a deletion this
decision has just forbidden. That resolves cleanly once the database is a separately deletable object,
which is a reason to sequence archive after the split rather than inventing an interim answer.

### Round 5 on the reconciled design, and the thing I got wrong in its brief

Three reviewers against the rewritten pages. Five findings survived my own check and became tasks;
two of them corrected me.

**I briefed a shape that was never adopted.** My round-5 brief described a three-transaction apply
bracket ("TX-A revokes epoch E and marks the head applying, TX-B is the DDL, TX-C mints E+1") as
settled. It is in no document - `grep -rn "TX-A" docs/` returns nothing. I carried it forward from one
of the design drafts and stated it as decided. One reviewer caught this in its own self-refutation and
correctly discounted the finding that rested on it. Any round-5 finding keyed to TX-A specifically is
conditional on a shape nobody adopted.

The lesson is not "do not paraphrase". It is that a brief is an ASSERTION ABOUT THE TREE, and this one
asserted a mechanism into existence. Everything else in that brief was measured or cited; the one
unsourced sentence is the one that cost a reviewer's strongest finding.

**"Only the producer is missing" was too narrow, and it was in AGENTS.md.** I wrote that the schema
epoch's consumer ships and only its producer is absent. Verified against
`crates/zeroship-plugin-db/src/transaction/reducer/mod.rs:966-972`:

```rust
if !opened {
    return self.force(CleanupCause::BeginFailed, now).1;
}
```

A failed BEGIN returns before `classify` runs, and `classify` runs on a synthetic observation composed
before the session opens. So the epoch comparison at `identity.rs:313-315` cannot observe a real
`SET LOCAL ROLE` failure - which is precisely how a role-name-borne epoch fence fires. The epoch needs
a producer AND an adapter from the session-setup outcome into `Verdict::ReResolve`. Corrected in
`7d786518c`.

**That unified two findings filed separately.** `BeginFailed` is where classified setup errors go to
die - it is the same seam that discards a classified `GRANT_REVOKED` into a generic `begin_failed`.
One defect, two symptoms, one fix.

**The apply cannot be one transaction, and the document said it could.** `engine.rs:2656` states the
engine's contract - "everything ahead of it commits in its own transaction" - and the host loops it
once per IR file. Two reviewers reached this independently by different routes. The epoch rotation IS
atomic as one small transaction; the DDL is not and must not be inside it. Section 4 now says so, and
names the three things that remain open: where the rotation sits relative to the DDL, what unfences an
app if the process dies mid-apply (`apply.rs:708` passes `recovery_scope: None`), and whether the
rotation may reuse a publication reconciler that owns its own `BEGIN`.

**Measured this round, both closing gaps a reviewer named rather than assumed:**

- *Publication naming decides, and the list is fixed at stream start.* One slot, one dataset, two
  decodes differing only in which publications are named: 4 records vs 8. PostgreSQL's own warning -
  "The publication does not exist at this point in the WAL" - shows existence is evaluated against WAL
  position. So a per-database publication under one slot per datastore strands every later-created
  database, and the remedy is a stream restart, which is the exact cost `cdc-service.md:39` rejected
  the per-app shape for. Two live documents specify this incompatibly and never cite each other.
- *A warm pooled backend sees a role minted after it connected.* Same backend pid before and after
  (`1535`), control role minted pre-connect and case role minted post-connect both settable. So epoch
  rotation needs no connection churn, and the unmeasured connect-time membership-cache cost is not
  triggered by it. Had this gone the other way, an app would still fail after a correct rebuild.

### Reconciling the two re-key drafts: the epoch is enforced by PostgreSQL, not compared in Rust

Two independent drafts of the identity re-key agreed on nearly everything - the key per subsystem, the
two-verb teardown, the billing principal, what must land atomically - and disagreed on ONE thing: how
the serve-time schema epoch is enforced.

| | A | B |
| --- | --- | --- |
| mechanism | append `SELECT epoch ...` to the setup batch, compare in Rust | epoch in the per-grant role name; `SET LOCAL ROLE "zs_bind_<gid>_e<E>"` fails when it rotates |
| steady-state cost | one index lookup, inside an existing round trip | none: the role name is already the batch's first statement |
| enforced by | the worker | PostgreSQL |
| resource | one row per database, datastore-scoped | a role + membership per (grant, epoch) in CLUSTER-SHARED `pg_authid` / `pg_auth_members` |

B's own author nominated it as the choice most likely wrong, and named the two probes that would
overturn it. **I ran both. Both fail to overturn it.**

**Probe 1 - does `CREATE ROLE` in the apply bracket serialize applies across other databases?** If it
did, B would put a cluster-wide serialization point in every migration. Method: session 1 holds an
uncommitted `CREATE ROLE` in database `zeroship` (precondition proved via `pg_stat_activity` showing
it `active`); session 2 runs `CREATE ROLE` in database `postgres` with `lock_timeout='3s'` so blocking
surfaces as an error rather than a hang; control is the same statement with no holder.

    CONTROL (no holder)           CREATE ROLE
    CASE (concurrent holder)      CREATE ROLE   elapsed_ms=108

No cross-database serialization.

**Probe 2 - is `SET ROLE` superlinear in `pg_auth_members`?** B taxes every query on the platform if
so. Method: grow the shared catalog, then time 2000 `SET ROLE` + 2000 `RESET ROLE` server-side in a
plpgsql loop, so client round trips are excluded and the same N is used at every scale.

    pg_auth_members=3      pg_authid=20      setrole_ms=6
    pg_auth_members=103    pg_authid=120     setrole_ms=6
    pg_auth_members=1103   pg_authid=1120    setrole_ms=6
    pg_auth_members=6103   pg_authid=6120    setrole_ms=6

Flat across a 2000x growth in membership rows - 1.5us per statement at both ends.

**Decision: B.** It is the stronger shape and, once measured, the cheaper one. Stronger because the
fence becomes a condition the worker FAILS rather than a function it CALLS - it is the first statement
of the only batch that yields a usable connection, so no code path can skip, forget or be talked out
of it. Cheaper because the epoch rides a string already being sent; A adds a statement to every setup
batch forever to avoid a catalog cost that measures as zero.

**What is still unmeasured, and must not be read as covered:** per-backend membership cache
construction at CONNECTION time. Probe 2 measures `SET ROLE` on an established backend, not the cost a
new backend pays to build its membership set - a different cost, paid at connect rather than per
statement, and amortized by pooling but not eliminated. The proposal's cost 7 names it. Measure it
before the role graph ships.

**Also carried from B unchanged:** `epochs_in_flight <= 2`, enforced by making the epoch reaper part
of the apply rather than a sweep, with an apply that cannot drop epoch E-1's roles REFUSING to advance
to E+1. That converts an unbounded shared-catalog leak into a bounded, fail-closed one.

**And the first version of probe 2 measured nothing.** It restarted role numbering at 1 for every
scale, so every call after the first aborted on a duplicate name; the catalog stopped growing at 103
while the timings kept printing - identical, plausible, and pointing at the same conclusion the real
measurement later supported. It was caught only because the script printed `pg_auth_members` beside
each timing. A scaling probe must print the thing it claims to be scaling.

### The four-reviewer round on the revised design, and the verdict it forces

Four independent reviews of `data-system.md` + `2026-08-28-app-database-decoupling.md`: two
agents (14 findings each) and two codex runs (10 and 6). Twelve survived my own
check against the tree and became tasks #45-#58. **Ten of them are marked "blocks
implementation" by their reporter, and I agree with that reading on eight.**

**The design does not yet settle.** The operator directive is to implement once it
does, so this is the gate, and it is not passed. The blockers are not polish; each
one changes a shape:

| What breaks | Where | Task |
| --- | --- | --- |
| unmask returns 42501 under column grants | `crates/zeroship-plugin-db/src/crud/unmask.rs:497` | #45 |
| the id leaks by three channels, incl. persisted `currentUser()` rows | `crates/zeroship-migrate-postgres/src/backend/session.rs:935` | #52 |
| an app editor can read a sibling app's classified rows | `deploy/policies/creator/app_editor.cedar` | #53 |
| cost accrues to one app, throttling hits it, the causer is unthrottled | `crates/zeroship-metering/src/meter.rs:1` | #57 |
| binding a 2nd app refuses its first deploy | `crates/zeroship-control/src/registry.rs:469` | #49 |
| an apply with no deploy strands running isolates | same | #51 |
| SQLite CDC + transaction lanes key on alias==app_id | `crates/zeroship-plugin-db/src/backend/sqlite/cdc.rs:121` | #54 |
| teardown is app-keyed end to end | `crates/zeroship-plugin-db/src/drop_namespace.rs:69` | #55 |

Every path above is a full repo path on purpose: `tests/doc_citation_gate.sh` only
extracts citations it can resolve, so an abbreviated `meter.rs:1` would have been
skipped in silence rather than checked. The first draft of this table was written
that way and the gate's count did not move.

**The single root under most of them:** the design re-keyed the SCHEMA from app to
database and did not re-key the things that hang off it - the ledger, the deploy
gate, the meter, teardown, the SQLite alias, the publication. Each of those still
answers `f(app_id)`. #49, #51, #54, #55, #56 and #57 are six faces of that one
omission, and fixing them individually will produce six inconsistent keys.

**What the reviewers were each uniquely good for**, worth remembering when
composing the next round: the two agents converged on document-level contradiction
(both independently found the same four off-by-one citations and the same role-fence
conflict); codex found what only reading CODE finds - the `currentUser()`
persistence path, and, when its brief was bounded to 12 files and told to write
early, six findings from subsystems the agents never opened. **The bounded brief was
the change that made the second codex run productive**; the first read until it was
killed. It also, contrary to my own reading of it, finished - see
`reference_pgrep_codex_returns_zero_while_it_runs`.

### Six operator decisions that reshaped the decoupling, and what they superseded

Taken in conversation, applied to `2026-08-28-app-database-decoupling.md` and
`docs/architecture/data-system.md`. Recorded here because the design pages now
carry only the outcome.

1. **The creator owns the database; no app does.** Superseded:
   `Namespace.owner_app_id`, not null, with the migrate policy "principal may
   migrate N iff principal is an owner of `N.owner_app_id`". Schema authority no
   longer passes through an app identity. The document already contradicted
   itself here - the very next bullet said the migrator role was "named by no
   app" - which is the tell that the column was vestigial.

2. **Migrations are not coupled to an app; the model is a monorepo.** Several
   apps in one workspace share one migration source and one set of generated
   types. Superseded the claim that an owner's migration "can break a co-tenant's
   deploy" as a COST: it is ordinary shared-dependency mechanics, and the creator
   owns the risk. The deploy gate survives for a different reason than was
   given - an app built against v1 must not be SERVED against v2.

3. **One `zeroship.jsonc` for the whole workspace.** So a Database hangs off the
   project row. Consequence the proposal had not stated: `app` is a single
   string today (`schema/project-v1.json:30-34`) and must go plural, and
   environments make `app` non-inheritable specifically to prevent silent
   cross-targeting (`:66-67`) - a property the plural shape has to preserve.

4. **An app sees exactly ONE database.** The "one app, many databases" half of
   the original ask is dropped; "many apps, one database" is delivered.
   Superseded the entire binding level: `env.db.<binding>.<collection>`, the
   `binding_name` column with `UNIQUE (app_id, binding_name)`, the
   conjunction-over-bindings deploy gate, and the manifest becoming a map. Two
   consequences: `env.db.users` survives unchanged, and database resolution stays
   `f(app_id)` rather than becoming `f(app_id, binding_name)` with the binding
   coming from creator code - so a mismatched-pair bug class never comes into
   existence rather than being bounded by an argument.

5. **Databases are provisioned, never auto-created**, on the D1 model. NOT a new
   service: `crates/zeroship-migrate-server/src/provisioning.rs` already issues
   `CREATE SCHEMA`, `CREATE ROLE` and `ALTER SCHEMA ... OWNER`, from a process
   that does not execute creator code. What is wrong is the trigger -
   provisioning is a side effect of applying a migration, keyed
   `format!("app_{schema}")` at `apply.rs:1178`. The work is inversion and
   placement, not construction.

6. **`Namespace` is renamed `Database`, and its id is internal.** Creators
   address a database by its workspace-local name. Superseded a route that took
   the id in its URL, which a creator could not have called without holding it.
   The word `namespace` stays where it names PostgreSQL's own object
   (`pg_namespace`, `drop_namespace.rs`) - a full sweep would have made those
   say the wrong thing.

**The class of mistake, and it is this file's own subject.** Applying these, I
annotated each superseded claim in place - "an earlier revision said", `WITHDRAWN`,
struck-through prose - and accumulated twenty-one such markers in a day. That is
the same failure this document was created to end, at one twentieth the scale and
in a third of the time. An inline correction is legible to whoever writes it and
is sediment to everyone after. The markers are gone from the design pages; what
they said is above.

---

## 2026-08-28

### The masking flip's write path, and three numbers that were wrong

**Believed.** The flip (`ssn` holds the mask, `ssn_raw` holds the real value)
was recorded as DECIDED in SC-6 and owed four implementation items. A blocking
note added on 2026-08-28 found its write path unguarded in both directions, and
rested that finding on three measurements.

**True.** The write path is unguarded - that conclusion survives - but all three
measurements under it were wrong, and each was wrong in a different way.

**Evidence, and the corrected forms.**

1. **"34 `RETURNING *` sites in `crates/zeroship-schema/src/query.rs`."** That is
   the raw `grep -c`. **The corrected derivation, with its boundaries, is stated
   once - in the design, section 6** - and deliberately not repeated here, since
   one fact fully stated in two documents is the failure class collected at the
   bottom of this file.

   What belongs here is the shape of the error. **"34" was cited as fact in
   three documents for a day, and the first correction to it - "12" - was also
   wrong**, because it silently dropped eight `///` doc comments that are
   production lines. A third plausible-looking split, at the `#[cfg(test)]` on
   `query.rs:5504`, yields a clean 17/17 that means nothing, because that
   attribute gates a single function rather than a module. Three numbers, three
   different boundaries, none of them published. *(Class: a figure with no
   stated denominator cannot be checked, only repeated - and each re-derivation
   picks whichever boundary is nearest to hand. Twenty functions reach the
   twelve emitting sites, because six are thin delegating wrappers -
   `build_insert` `:3503` -> `build_insert_with_dialect` `:3518`, and five more
   of the same shape - so even "how many call sites" has two defensible
   answers.)*
2. **"`strip_encryption_markers` retains it (`encryption_pass.rs:502-505`)."**
   The function is `#[cfg(any(test, feature = "test-helpers"))]`
   (`crates/zeroship-plugin-db/src/crud/encryption_pass.rs:501`), so it is not
   on the production path at all, and it strips `__zsbin__` markers from a
   **write** document before binding rather than from a returned row. It is
   neither outbound nor live.

   **The conclusion survives through a different and worse route.** Nothing on
   the production read path removes an unknown key from a returned row. The only
   key removal is `mask_pass::wrap_row_on_read`, which removes exactly
   `format!("{col}_masked")` (`crud/mask_pass.rs:469`, `:480-482`), so a raw
   column survives to `mapResultDoc` (`sdks/db/src/utils.ts:28-33`). And a
   second silent arm the note missed: `decrypt_row_on_read` gates decryption on
   the same hardcoded sibling name (`crud/encryption_pass.rs:295-301`), so
   post-flip an encrypted+masked field skips the decrypt stage entirely and
   reaches JS as base64 ciphertext, while a mask-only field reaches JS as
   plaintext. Two different leaks from one missing string. *(Class: the right
   conclusion reached through a wrong mechanism. A citation that supports the
   verdict is not thereby verified.)*
3. **"The SQLite introspector drops all mask metadata with no `else`
   (`backend/sqlite/mod.rs:2220-2237`)."** True as written and irrelevant on the
   production path: `parse_mask_sentinels`
   (`crates/zeroship-plugin-db/src/backend/sqlite/mod.rs:2201`) and its only
   caller, the `SchemaIntrospect for SqliteBackend` impl (`:804`, call at
   `:917`), are both `#[cfg(any(test, feature = "test-helpers"))]`. The dev tier
   does not run it, and `crates/zeroship-plugin-db/src/descriptor.rs:30-32`
   states so in the tree's own words: "On SQLite it was never live at all".

   **The same defect IS production on PostgreSQL, and the note did not mention
   it.** `read_live_schema` filters on the suffix at
   `crates/zeroship-schema/src/diff.rs:671` -
   `if comment.starts_with("__zsmask:") && column.ends_with("_masked")` - and
   strips it at `:716`. A `__zsmask:` sentinel on a column not ending `_masked`
   falls through both `if`s and is discarded with no warning, while the
   malformed-sentinel arm ten lines below warns loudly (`:737-744`). That arm
   runs, and it feeds the migration engine's diff. *(Class: a defect found in
   the test-only copy of a mechanism, while the production copy of the same
   mechanism went unexamined.)*

**And the specification's author recommends the flip be cancelled.**
`docs/reviews/2026-08-28-flip-write-path.md` section 8.6 states it directly: "I
would not implement the flip." The accounting is that the flip's unique benefit,
after weakening, is **one narrowed descriptor-staleness window** - a stale
descriptor already fails closed on three of four stages, skipping decrypt
(`crud/encryption_pass.rs:279-281`), skipping the mask wrap
(`crud/mask_pass.rs:438-441`), and having the field stripped by the specified
row-surface filter - against three silent write-correctness inversions, one
data-loss hazard, one AEAD invariant held by convention, and a migration of
every masked column in every collection.

**The design records this as an open question, not as a settled direction.**
Three findings from that specification are worth carrying because they are
composition facts no single-file review produces:

- **The upsert conflict probe inverts in both directions, silently.**
  `rewrite_upsert_doc_id_to_existing_row_id` builds a filter from the conflict
  field names (`crud/write_pipeline.rs:474-479`), encrypts the operand for
  deterministic fields (`:485-496`), and hands it to
  `build_conflict_probe_with_dialect`, which emits `WHERE "ssn" = $1` from the
  **logical** name (`query.rs:2910`). Post-flip `"ssn"` is the mask column and
  `$1` is ciphertext: no row matches, the function returns early at `:511-513`,
  and the upsert **inserts a duplicate instead of updating**. In the other
  direction, `build_upsert_with_dialect` builds `ON CONFLICT (...)` from the
  same logical names (`query.rs:5914-5918`), so two different values sharing
  `***-**-1234` collide and the upsert **overwrites an unrelated row**.
- **The flip makes two passes write the same key, in an order nothing
  enforces.** Today the encryption and mask passes write disjoint keys - the
  encryption pass replaces `row["ssn"]`, the mask pass *inserts*
  `row["ssn_masked"]` (`crud/mask_pass.rs:147-156`) - behind independent gates
  (`crud/write_pipeline.rs:226`, `:237`). Post-flip both must target
  `row["ssn"]`, and the relocator differs by field shape within one collection
  while `WriteStages`' gates are per-collection booleans (`:199-201`). The mask
  pass's documented failure arm (`mask_pass.rs:121-125`) would then overwrite
  ciphertext with a mask of itself - unrecoverable data loss on a write that
  returns success. The safe shape is one relocation stage that runs after both.
- **`rawProjectable: false` reads as protection and is not.** It is stamped
  `Some(false)` on every masked field (`gen_types.rs:305`) and its doc says it
  declares whether a creator-facing projection may return the raw column
  (`:199-202`). Nothing reads it - and the reason is worse than
  "unimplemented": raw columns do not enter the result through a projection
  builder at all, they enter through `RETURNING *`, which has no projection
  builder. Implementing the flag faithfully would change nothing, and the
  resulting code would read like a defence. *(Class: a guarantee honoured
  somewhere useless is harder to notice than one honoured nowhere.)*

### Citation drift, measured

**Believed.** The v4 document's file:line citations described the tree.

**True.** Three load-bearing ones had already moved under the implementation
commits: `Pool::connect(&url, 8)` cited at `lib.rs:862` is at `lib.rs:998`; the
per-deprovision `Pool::connect(url, 2)` cited at `:904` is at `lib.rs:698`; the
"~200 isolates (apps) per OS thread" note cited at `exec.rs:1273` is at
`exec.rs:1321`. `sanitize_app_actor`, cited at `crud/unmask.rs:282-303`, is at
`:277`. The `pitr_targets` insert, cited at `backend/postgres.rs:1702-1718`, is
at `:1727`.

*(Class: a document that cites the tree ages against the tree. The restructured
design marks which citations were re-derived and which were carried forward, so
the next reader knows which to distrust.)*

### The lazy-DDL enumeration, which was owed and never written

**Believed.** Invariant 7 and the v2 corrections table both said "three" lazy
DDL sites, and between the design and the defect register exactly **one** was
named - `write_audit_unmask_row`. The design flagged this as OWED and said "the
count is not invented here".

**True, measured 2026-08-28.** There are **six** `CREATE TABLE IF NOT EXISTS`
statements in `crates/zeroship-plugin-db/src/`, creating **three** tables, each
with a PostgreSQL arm and a SQLite arm, and none of the six is `cfg`-gated:
`"<app>"."__zeroship_migrations"` (`audit.rs:236`,
`backend/sqlite/mod.rs:1292`), `"<app>"."__zeroship_audit_mask_drift"`
(`crud/mask_drift.rs:793`, `:827`), and `"<app>"."__zeroship_audit_unmask"`
(`crud/unmask.rs:850`, `:901`). The unmask table is created from
`ensure_audit_unmask_table` (`crud/unmask.rs:838`), called at `:732` inside
`write_audit_unmask_row`, which is reached from the denied path (`:405`), the
granted one (`:428`), and two further callers (`:1217`, `:1446`).

**So "three" was right about tables and the document could not say which**,
because nobody had run the enumeration. *(Class: a count carried in prose while
the enumeration behind it was never performed. The acceptance arm that depends
on it - "no data-plane path executes DDL, enforced by the deletions" - could not
have been written.)*

---

## 2026-08-27, second batch

### Decision 8: the data plane performs no live introspection, and validation is deferred

**Believed** (decision 7, hours earlier). Live reads survive in exactly one
place: DDL validation at startup, in the shape of Hibernate's
`hbm2ddl.auto=validate`. It walks the catalog, compares it against the
descriptor, and refuses to serve on mismatch.

**True.** Startup DDL validation is **deferred as a future feature**: not
specified, not designed, and deliberately without a placeholder shape. The rule
is absolute - the runtime descriptor is the sole authority for schema and the
data plane never reads the catalog.

**Evidence that the decision is implementable, rather than hoped-for.** The
descriptor-specification survey
(`docs/reviews/2026-08-27-descriptor-specification.md`) enumerated every
data-plane consumer of a schema fact and classified each as "the descriptor can
carry it" or "this genuinely requires the live database". **The second bucket
came back empty of schema facts.** Only two live reads survive at all, and both
are `pg_extension` capability probes (`backend/postgres.rs:472`, `:632`) - they
ask what the *server* can do, not what the *schema* is.

**The strongest evidence is that we already shipped it.** `runtime_schema_for`
had no SQLite introspector: on that backend it called `sqlite_fallback_schema`
(`crud/introspect_schema.rs:262-264`), which returned the declared cache
outright, and the comment above it (`:257-261`) called this "the documented
gap... until the SQLite-arm introspector is wired in". The entire mask and
encryption feature set had been running on the declared schema alone, on that
backend, in shipped code. The introspector was never written and nothing needed
it.

**One correction the survey forced, and it cuts the other way.** The introspected
source is not merely replaceable, it is the **less faithful** of the two.
`build_runtime_schema` (`crud/introspect_schema.rs:281-323`) kept three facts
out of eight, and its type mapper (`:329-353`) could not emit `vector` or
`geoPoint` at all - two tokens live consumers require. Deleting it removed the
weaker authority.

**What it retracts.** Decision 7's one surviving live read; invariant 6's
"descriptor/live **mismatch**" clause, which has no live side to mismatch
against; step 6 of the sequence, which lost most of its subject; and the
premises under `crud/introspect_schema.rs` and `live_metadata.rs`, both of which
lost their last caller. `live_metadata.rs`'s consumers were
`context.rs:46,441,749,1211`, `lib.rs:340,361,493,508`,
`service.rs:62,295,636`, `crud/introspect_schema.rs:51,483,782` and
`v8_classes/db.rs:477`, and every one of them was either the introspection path
being deleted or existed to hold the cache for it.

**An annotation written hours before this decision said decision 7 "moves WHERE
this is enforced, not whether" - to startup validation.** That was true of
decision 7 and is false now.

**Cost.** Stated in the design under 3.1: the deploy pipeline's ordering
guarantee becomes load-bearing, mid-life drift is undetected, and subscribers
lose their schema-change signal.

### Decision 7: `__zeroship_admin` is deleted entirely, and there is no schema epoch

**Believed** (decision 6, written hours earlier the same day). The schema keeps
exactly one resident, `app_schema_state`, and it survives because it satisfies
the repository invariant precisely: written by a separate service (the migration
service mints the epoch), read-only from the worker, and unforgeable by the
tenant - the exact shape `AGENTS.md` says a system schema exists for.

**True.** The schema is deleted in full. Not reduced to one table - deleted.

**The chain that gets there.** The runtime descriptor is the authority; it
carries the physical layout including sibling columns; therefore **the epoch has
no job**. It existed as a cheap proxy for "does the database still match what I
was built for", and validation answers that question directly against the
catalog. A proxy for a comparison you are already making is not needed.

**Why decision 6 was wrong is the part that transfers.** Every argument in it is
about whether the row could live in a platform schema **safely**, and none of
them asks whether it needs to exist. **It was safe. It was redundant.** That is
a failure mode no amount of scrutiny of its privilege posture would have
surfaced. *(Class: a mechanism examined for whether it was correctly built
rather than for whether its question was still being asked. Decision 5 has the
same shape.)*

**What it retracts, each of which was written as settled:**

- **The goal** "make live physical and security metadata authoritative and
  cacheable by a database-resident, tenant-unwritable, never-reused schema
  epoch".
- **The non-goal** "the runtime descriptor does not become the physical database
  authority". **This is the largest single reversal in the document set**: the
  design's central denial became its central claim. Every "neither source is
  silently trusted for the other's job" passage was written to keep two
  authorities in tension; decision 7 collapses them to one.
- **Invariant 3**, "code deploy identity and database schema epoch are
  different". The distinction it drew was real and its subject is gone: the two
  identities collapse into one **by construction**, which is not the same as the
  invariant being wrong - it is the invariant becoming unstatable.
- **Invariant 4**, "the database proves the epoch, and the tenant cannot write
  it". The unforgeability property is replaced by a different one - the
  descriptor arrives in the deploy artifact, which the tenant also cannot write
  at runtime. **The two are not equivalent**, and the difference is that
  validation happens once, so a migration applied mid-life is not detected.
- **Invariant 5**, "one operation uses one metadata snapshot", which becomes
  trivially true. It was previously the hardest invariant in the document - it
  forced the epoch pin, the lease, and the per-operation resolution - and it is
  now free. It is kept stated because the cheapest way to reintroduce the bug is
  to add a second source of metadata and not notice.
- **The WAL epoch carrier**, which the index presented as SOLVED with a
  measurement. See the next entry.
- **The live-metadata cache's entire premise**, and with it the whole cost
  analysis: the `O(total tenants)` catalog read, the cold-start `N` whole-schema
  reads, and the entry-size measurements. See "The metadata cost analysis" below
  for what survives of it.
- **The identity substrate step as specified**, and the epoch halves of the
  writer and CDC steps.
- **L6**, which is CLOSED rather than scope-reduced: there is no admin schema to
  provision.

**The schema never existed in production, which is what made the deletion free.**
`ensure_admin_schema` and every installer are
`#[cfg(any(test, feature = "test-helpers"))]`
(`crates/zeroship-plugin-db/src/auth/bootstrap.rs:95-96`), no migration or SQL
file created it, and `crates/zeroship-plugin-db/src/encryption/keys.rs:495`
instructs operators to run a bootstrap migration **that does not exist**. The
epoch design would therefore have landed the *first* production provisioner for
that schema; decision 7 deleted the subject instead. Per the no-back-compat rule
this is a delete, not a deprecation: there is nothing deployed to be compatible
with.

**What was deleted, sized.** `ensure_admin_schema` created **6 tables** and **13
routines** - 12 functions and one procedure, of which **12 were `SECURITY
DEFINER`** (`const_eq` was `IMMUTABLE PARALLEL SAFE`,
`auth/bootstrap.rs:645-649`). The tables were `hmac_keys`, `session_nonces`,
`session_ctx`, `column_keys`, `pitr_targets` and `mask_policies`
(`auth/bootstrap.rs:181-195`). Measured 2026-08-27: `bootstrap.rs` 1,827 lines,
`session.rs` 620, `keys.rs` 234, against a subtree of 2,978 lines across five
files. Implemented the same day: `auth/session.rs` and `auth/keys.rs` deleted
whole, `auth/bootstrap.rs` 1827 -> 493, 19 files changed, net -3,173 lines.

**And one grant nothing in the set had mentioned before.** The installer issued
`GRANT USAGE ON SCHEMA "__zeroship_admin" TO "<app-role-template>"`
(`auth/bootstrap.rs:171-178`), and its comment said exactly why: so the template
"(and any per-app role inheriting from it) can CALL the SECURITY DEFINER
functions" (`:168-170`). **That grant is the reachability precondition for every
`EXECUTE ... TO PUBLIC` in the schema.** A gate arm that checks the grants while
leaving the `USAGE` in place is checking the lock and not the door.

**The rename question is moot.** Decision 6's candidates -
`__zeroship_schema_state`, `__zeroship_epoch`, `__zeroship_platform` - are
recorded here and need no decision. Decision 6's own argument for a rename
stands as a general observation: "admin" named a drawer of platform powers, and
**a name that survives its contents is how the next reader concludes there is a
drawer to put things in.**

### The WAL epoch carrier: chosen, measured, and moot within hours

**Believed.** CDC events must carry a schema epoch so a subscriber can tell a
migration happened and resync. The design first asserted that "the producer is
the mutation's own operation - which holds the lease and already knows the
epoch", so the epoch could be stamped at produce time.

**That was false on the path that matters most, and the document asserted it
without checking.** In production the mutation-side producer is *suppressed*:
`is_app_suppressed(app_id)` gates it with the comment that when the WAL consumer
runs for an app "it owns the publish path for events this isolate writes", so
"in production with the consumer active, EVERY mutation previously paid the
build cost only to discard the result"
(`crates/zeroship-plugin-db/src/exec.rs:455-466` for the reasoning; the call at
`:501`). The real producer is `wal_consumer::emit_for_tuple`
(`crates/zeroship-plugin-db/src/wal_consumer.rs:589`) - which holds no lease, has
no operation context, and in which the string `epoch` does not appear once in
the entire file. *(Class: a mechanism well-founded on the local path and
unimplementable on the production one, because the local path is the one a test
exercises.)*

**An earlier revision of the index then argued the carrier and L12 "have one
answer", namely the relay.** That was wrong in a characteristic direction: it
was argued from the code - `emit_for_tuple` cannot read an epoch, therefore only
a process that can hold a lease can stamp one - **without asking whether the
epoch could arrive by some route other than being read.** It can.

**The carrier that was chosen and measured.** Put the platform's state table in
the app's publication under a `PUBLICATION` row filter scoped to that app's row.
The epoch update a migration performs is then delivered to the consumer as an
ordinary change at the LSN where it committed, and every row change after it is
post-migration **by construction** - WAL ordering is the proof, so the consumer
needs no lease, no session and no round trip, which is exactly what made this
hard.

Verified end to end on pg16.14 (`tmp/measure_epoch_in_band.sh`), four checks:

| check | result |
| --- | --- |
| publication accepts a row-filtered table in another schema | `app_alpha.users` + the admin table both published |
| the epoch `UPDATE` is delivered in-band | `epoch_TWO` present in the decoded stream |
| another app's epoch row leaks in | **absent** - the row filter isolated it |
| ordering | `before` -> `epoch_TWO` -> `after`, each in its own transaction |

Two preconditions, stated because the mechanism degrades silently without them:
the filter column must be covered by the table's replica identity or the
`UPDATE` is not delivered at all (`app_id` was the primary key, so the default
replica identity covered it), and the tenant must be unable to write the row.

**Then decision 7 removed the epoch, hours later.** The measurement is correct
and is kept; what is retracted is the conclusion that the platform needs the
mechanism. **It retains one use**: it establishes on a real server that a
row-filtered publication over a table in another schema is delivered in-band and
isolates per app. If a future mechanism needs to inject a platform-owned signal
into a tenant's WAL stream, that property is already verified.

**Note which three candidates were floated before it** - "a replication message
field, a per-transaction marker, or a column the consumer reads off the tuple".
All three tried to attach the epoch to *the row being changed*. The answer was
to let the epoch be its own row, already ordered against those changes by the
WAL.

*(Class: **this is the second time this document set produced a carefully
verified answer to a question that then stopped being asked.** The first was
SC-6's ceiling-read contract - a table, a writer, a CAS, an authority pool, a
saturation arm - all correct, all deleted by decision 4. The pattern in both:
"how do I make this database read correct and cheap" was answered well, and
never audited against "does this read need to happen".)*

### Decision 5: privilege follows the PROCESS, not the function

**Believed, in this document's own text from earlier the same day.**
`session_nonces` was recorded as KEEP, with a correct atomicity argument: it is
a replay cache, its single-use check must be atomic with the transaction that
consumes the nonce, and a control-plane round trip cannot provide that atomicity
- a remote "have you seen this nonce" call is a TOCTOU window by construction.
The `hmac_keys` / `session_ctx` pair was recorded as deliberately UNDECIDED,
with both sides argued, under the question "is per-request identity enforced
inside SQL, or in the worker?".

**True.** All three are deleted, together with `sign_session`,
`verify_signature`, `const_eq`, `init_session`, `reset_session` and
`rotate_session_keys`. The whole apparatus is a signed session the worker
presents **on its own behalf**, which the repository invariant names as an
appearance of a boundary rather than one.

**The KEEP argument is not refuted; it is dissolved.** It reasoned about how to
build a replay defence for a session the worker should not be presenting in the
first place. *(Class: an argument can be locally valid and still be answering a
question that has been dissolved rather than settled. Both the superseded
framing and why it was locally sound are kept here, because "this was argued
carefully and was answering the wrong question" is the part that transfers.)*

**The tree already contained the proof.**
`crates/zeroship-plugin-db/src/audit.rs:20-42` (DELETED in `ac38fac0e` by this
design's own implementation; read it there) records this exact ceremony as a
proposal - "a tamper-evident `SECURITY DEFINER` write path mediated by an
HMAC-signed `__zeroship_session_ctx` PID-keyed table living in a platform-wide
`__zeroship_admin` schema" - and **refuses it**: "app code does not have raw SQL
access ... The worker pool is the only writer ... Provenance is therefore
enforced at the Rust call boundary, not at the SQL boundary." That refusal was
written for the audit path and its argument was never audit-specific. **It
applies unchanged to every other consumer of the same session, and the session
was built anyway.**

**And the counter-example, which is the stronger half.** DB-3: app JS reached a
privileged unmask call and could pass `actor: { kind: "auto" }` to read its own
PII/PHI/PCI at will, patched by `sanitize_app_actor` stripping reserved system
kinds (`crates/zeroship-plugin-db/src/crud/unmask.rs:282-303`). That is not a
bug the shape happened to have; it is what the shape produces. A privileged call
the worker can make is a privileged call creator code can reach, and the only
defence available is a hand-maintained list of arguments to strip.

**A fact that made the answer available before the decision was taken: SQLite
has no `session_ctx` at all.** The SQLite backend says so in its own words -
"SQLite has no `session_ctx` table - there is no per-PID session-context concept
here", and downstream audit paths "bind context through the session actor's
per-call state instead"
(`crates/zeroship-plugin-db/src/backend/sqlite/mod.rs:1652-1655`). **Two tiers
disagreeing about where identity is enforced is either a contract-parity break
or evidence that one of them is sufficient; it was read as neither, and the
question stayed open.**

**What goes with it.** The `SessionMinter` half of an earlier open sentence -
"`SessionMinter` and the backup/snapshot contracts are either assigned a
destination in SC-3's ledger or deleted with the feature they serve" - resolves
as a deletion. The SQLite side goes too
(`crates/zeroship-plugin-db/src/backend/sqlite/session_minter.rs`), and the
`session_*` error codes it raises - `session_nonce_replay`,
`session_nonce_too_short`, `session_signature_expired`
(`backend/mod.rs:736-738`, classified at `auth/session.rs:204-219`) - go with
the mechanism that raises them.

**Consequence beyond the schema: CDC slot and publication ownership moves to the
CDC relay service.** The wrappers this deletes are concrete: `ensure_publication`,
`ensure_slot`, the `ensure_publication_and_slot` procedure, and `watchdog`
(`auth/bootstrap.rs:1089`, `:1124`, `:1190`, `:1244`), all `SECURITY DEFINER`,
whose own header states the intent the invariant rejects - "wrapping them in
`SECURITY DEFINER` moves the privilege check from the caller to the function
owner" (`:1064-1076`).

*(One precision, because the wrapper's own comment overstates its reach and a
deletion brief written from it would look larger than it is: the comment says
"the test suite + the V8 callback layer call this" (`:1180-1181`). **Only the
test suite does.** The production path calls the raw
`replication::ensure_publication_and_slot` in Rust, and
`install_slot_wrapper_functions` is itself
`#[cfg(any(test, feature = "test-helpers"))]` (`:1076`). So this is a shape that
was built and never wired, which is why deleting it costs nothing today and why
the decision that matters is the one about where it would have been wired.)*

**A `SECURITY DEFINER` function that survives this rule, and why.** A transition
writer called by the **migration service** - which does not execute creator code
- and `EXECUTE`-revoked from `PUBLIC` and from every app role is the invariant's
second half working as intended, not an exception to it. **A reading of "delete
the `SECURITY DEFINER` functions" that took the count rather than the rule would
delete it too.** The distinguishing question for any routine is "which process
can call it", and it is answerable from the grant set alone. Every routine
decision 5 deletes has the opposite property: granted to `PUBLIC` and reached,
or intended to be reached, from the worker.

**Cost.** Deleting the session anchor removes the only in-database record of
which actor a worker was acting as. Any future requirement for SQL-side
provenance has no mechanism and would need the separate service the invariant
points at, not a restoration of these functions.

---

## 2026-08-27, first batch

### Decision 4: the operator ceiling is worker configuration

**This is the third position the document set has held on one question, and the
second reversal.**

- **v2** said the effective policy is "resolved before the isolate is built".
- **The v2 corrections table, row 10**, called that wrong: resolved *once*, so
  lowering the operator ceiling never reaches a pinned isolate. v2 had "traded a
  **forgeable** policy for a **non-revocable** one". The remedy was "freeze only
  the declared half; resolve the ceiling at authorization time".
- **Decision 4** returns to resolve-once.

**That is not row 10 being reinstated, and reading it that way loses the
argument.** Row 10's objection was that resolve-once leaves **no revocation
lever at all**. Decision 4 supplies one - rolling the workers, which is
operator-controlled, is already how every other piece of worker configuration
changes, and is **not a lever v2 had**, because v2's policy lived in a durable
per-app table that a worker roll would have re-read identically.

**Row 10's *other* half - that v2's declared policy was forgeable from any
bundled dependency (register L1, L2) - is answered by decision 3 rather than by
anything about the ceiling.** *(Class: the two halves of row 10 turned out to
have two different answers, and treating them as one is what kept the question
open for three revisions. Forgeability and revocability are independent, not two
ends of one axis; the superseded text treated them as an axis, so fixing either
had to cost the other.)*

**What decision 4 deletes.** The ceiling table and its write path, the
per-operation ceiling read, and the entire version-discovery and linearization
contract SC-6 existed to solve. **That problem was real** - "a cache keyed by a
version the reader can only learn by reading cannot discover a new version" -
and it is not solved here; it is **not raised**, because nothing reads a
version.

**What it retracts by name:**

- **SC-6's acceptance criterion**, which demanded that lowering the ceiling deny
  the next unmask "with no rebuild and no deploy". **That requirement conflated
  two different deploys.** The ceiling is *operator* state; it was never in the
  tenant artifact, so applying a new ceiling never needed a **tenant** redeploy,
  which is what "no deploy" was defending against. What it needs is the **worker
  roll the operator already controls**. The criterion read as a strong property
  and was in fact an argument against a cost nobody was proposing to pay.
- **SC-6's whole read contract.** The apparatus it built to make an
  in-transaction authority read safe - a dedicated authority pool disjoint from
  the eight-connection data pool at
  `crates/zeroship-plugin-db/src/lib.rs:862` - has no remaining client.
- **Fork B's second clause.** The ceiling was the sole value this design ever
  proposed to read *inside* an open creator transaction. The rule is kept
  because the next authority value someone wants mid-transaction faces the same
  `SET LOCAL ROLE` posture (`transaction/mod.rs:202-217`, applied at `:540`);
  what is retracted is the claim that this design **has** such a read. SC-2's
  `AuthorityRead` row listed `ceiling` as a component of the authority row it
  returns; that component goes.
- **"Restore must also re-assert the mask ceiling."** That paragraph read: "The
  ceiling now lives in `__zeroship_admin` beside the epoch, so it is one more
  piece of privilege state the restore checklist owns - and unlike the schema
  cache, a stale ceiling is not cured by a fresh epoch. A restore that
  reinstates a permissive ceiling silently re-grants unmask." **The reasoning
  was right for the shape it assumed and the shape is gone**, and this is a
  genuine benefit rather than a deleted line: privilege state that lives outside
  the database it governs cannot be rewound by a restore of that database. The
  mask **classifications** are a different matter - they live in column comments
  in the app schema (`mask_codec.rs`), so a dump does carry them.

**The superseded runtime-resolved shape, kept because decision 4 is stated as a
rejection of it.** It held that the epoch is pinned for a transaction's lifetime
because consistency demands it, while the ceiling is *not* pinned because it is
authorization state, and pinning it would let a long-running transaction hold a
permissive ceiling across a revocation. It also carried a correction of its own:
**the read is NOT on the transaction's own connection**, and an earlier version
said it was - inside an explicit transaction the connection runs under the
tenant's role for the transaction's whole life (`transaction/mod.rs:202-217`,
called at `:540`), SC-6's privilege posture denies that role any access, the
read would return `permission denied`, and because "failure is denial" **that
would have shown up as a passing test while every non-`auto` unmask inside every
transaction was bricked.**

It also recorded why "the same rules as the epoch" was not transferable by
assertion: the epoch is affordable because it rides an existing round trip,
while `zeroship-plugin-db` has no HTTP client at all and
`check_unmask_authorization` is a synchronous `fn` (`crud/unmask.rs:305`). And
that the authorization point would have had to move to after `prepare`, since
today it runs before any SQL (`crud/unmask.rs:1235-1236`).

**Costs, carried into the design:** revocation latency becomes worker-roll time;
deploy-pinned workflow isolates keep the old ceiling until evicted
(`PinnedWorkflowKey { app_id, deploy_hash }`, `crates/zeroship-worker/src/cache.rs:28-32`),
bounded by `max_pinned_isolates_per_app`; the creator half is frozen the same
way, so force-eviction is the single lever; and the dev tier needs a named
ceiling source that nothing specifies.

### Decision 3: the mask policy is code-managed and immutable at runtime

**Believed.** Nothing - this decision *resolves* a contradiction rather than
reversing a position.

**True.** The creator's mask policy is declared in the creator's codebase,
folded at build time into the deploy artifact, delivered through the
artifact/init channel, and immutable for the isolate's life.

**Two documents already demanded this, and the code contradicted both.** That is
the part worth recording, because it is a case of the design set being right and
having no effect:

- the design's own goals: "Remove every creator-reachable privileged capability,
  **including the ability to supply a security policy as an argument**";
- the backend SPI section: the SPI carries "**no policy-store capability**:
  policy ownership is control-plane state resolved before the isolate exists, so
  an SPI capability for it would reinstate the owner this design removes".

**Both were written as settled. Both were contradicted by live code the whole
time** - `dispatch_set_mask_policy`
(`crates/zeroship-plugin-db/src/crud/mask_policy.rs:224`), reached from the
`setMaskPolicy` V8 method (`v8_classes/db_platform.rs:145-155`), plus a store
that persists the result durably - **and neither statement caused the
contradiction to be found.** It was found by asking where the policy comes from,
not by reading either sentence. *(Class: a refusal stated in a design document
is not a boundary. Two documents can both be right and both be inert.)*

*(The refusal is sometimes attributed to SC-3. It is not there: SC-3 contains
the string "policy" five times, all about per-source-collection resolution on
joined rows. It is the design's, in the backend SPI section, and the v2
corrections table's row 9 is where it was first stated.)*

**It also decides the replacement wire the cutover owed.** The step said
deleting `defineMaskPolicy` "removes the only way a creator can declare a
policy, and neither manifest shape carries one", and SC-6 elaborated that the
string `mask` appears zero times in `crates/zeroship-bundle/src/manifest.rs` and
zero times in `sdks/vite-plugin/src/zship.ts`. The **carrier** is now decided -
the artifact channel that already carries the descriptor - and the five concrete
artifacts SC-6 enumerates are owed against a decided target rather than an open
one. *("Neither manifest shape carries one" was also narrower than the truth:
there is no latent route in either file.)*

**A second defect it closes by construction, which nobody had counted.**
`mask_policies: HashMap<String, MaskPolicy>` is keyed by **`app_id` alone**
(`crates/zeroship-plugin-db/src/context.rs:368`, read at `:837-841`, written at
`:847-857`) - **exactly L10's shape, in a map L10's fix did not touch.** L10
moved runtime schema metadata to a `DbBinding { app_id, deploy_token }` key
precisely because the worker keeps several isolates of the same app at different
deploys alive on one thread. The mask policy did not move, so two pinned
isolates of one app at two deploys shared one policy entry and the last boot to
run won for both. *(Class: a fix scoped to the site where a defect class was
noticed, rather than to the class.)*

**Cost.** Changing a mask policy now requires a build and a deploy. There is no
runtime edit, no dashboard toggle, no hot path to a looser rule during an
incident.

### Decision 2: `__zeroship_admin` is four concerns under one name

**Believed.** The production provisioner was described only as "the first
production provisioner for that schema", which reads as though its six tables
were one unit of work. The identity substrate step was estimated as though they
were.

**True.** Recreating all six for production would carry four unrelated concerns
into one schema on the strength of a shared name prefix.

| table | verdict | why |
| --- | --- | --- |
| `column_keys` | DELETE | Decision 1: one key per app, derived. Nothing to store |
| `pitr_targets` | DELETE, move to the control plane | Operator state that the data plane neither reads nor acts on |
| `mask_policies` (`auth/bootstrap.rs:502-506`) | DELETE | Decision 3: the policy ships in the artifact |
| `hmac_keys` + `session_ctx` + `session_nonces` | DELETE | Decision 5: a privilege the worker holds is not a boundary |
| `app_schema_state` | the only survivor - and decision 7 deleted it too | |

**The heading's count is the diagnosis, not the outcome**, and it is kept
because "four concerns under one name" is how the schema was found to be wrong.
A reader who sees only the end state cannot tell whether the others were
considered.

**`pitr_targets` is the only table in this schema granted write to `PUBLIC`** -
`GRANT INSERT, UPDATE, SELECT ON ... pitr_targets TO PUBLIC`
(`auth/bootstrap.rs:466-473`) - while every sibling ends at
`REVOKE ALL ... FROM PUBLIC` (`:282`, `:330`, `:371`, `:412`, `:513`). The grant
is deliberate: the comment says apps "can write through" (`:431-433`). **What
they write through to is a row the platform does not act on.** The same comment
block records that "the platform doesn't replay WAL from a client connection
(that requires server-level `recovery.conf` setup); this table is the queue
dashboards / the maintenance cron read" (`:426-429`), and the installer repeats
it: "only the API surface lives here" (`:187-190`). So the tenant can write an
operator's recovery target and the mechanism that would consume it is out of
band.

**This is a constraint on the provisioner, not a live defect**, and the
distinction is the same one the register draws for `get_column_key`:
`ensure_pitr_targets_table` is `#[cfg(any(test, feature = "test-helpers"))]`
(`auth/bootstrap.rs:435`), so the `PUBLIC` grant could not exist in `main`. It
matters because **a provisioner written by porting the installer's statements
would port this one.**

**It also removes an SPI member the deletion list did not account for.**
`Backup::pitr_replay` (`crates/zeroship-plugin-db/src/backend/mod.rs:1370`) goes
with the table, along with its PostgreSQL implementation, which does nothing but
`INSERT INTO __zeroship_admin.pitr_targets` (`backend/postgres.rs:1702-1718`),
and its SQLite stub (`backend/sqlite/mod.rs:2362`). Nothing calls it:
`DbPlatform` exposes only `registerModel` and `setMaskPolicy`
(`v8_classes/db_platform.rs:115`, `:145`), both of which this design deletes.

### Decision 1: no per-column encryption keys

**Believed.** `__zeroship_admin.column_keys(key_id TEXT PRIMARY KEY, root_key
BYTEA NOT NULL, ...)` (`auth/bootstrap.rs:401-405`) and the `SECURITY DEFINER`
`get_column_key` getter (`:613-637`) were treated as a constraint on the
provisioner - a thing to recreate correctly.

**True.** One key per app, derived `HKDF(platform_master_key, app_id,
key_version)`. There is no key table and no getter, so the constraint is moot.

**Evidence.** The full argument - that `canonical_aad` already binds domain
separation more tightly than per-column keys ever did, that `SECURITY DEFINER`
is a query-time boundary against a bytes-at-rest threat, and that `key_id`
defaults to the literal `"default"` in both producers
(`crates/zeroship-schema/src/query.rs:2295`, `diff.rs:1636`) so per-column
keying was already nominal - is in the design under "Key custody", because it is
current rather than historical.

**What decision 1 owes and this document set does not have.** The rotation half
is not implementable: the key version has no per-row carrier. Also in the design
under "Key custody".

**One thing the decision does not do, worth being explicit about.** It shrinks
the key cache's key from `(app_id, key_id)` to `(app_id, key_version)` and, in
the shipped default where `key_id` is `"default"` everywhere, **that is not a
reduction in entry count at all, only in what an entry means.** The
unbounded-per-thread property is untouched.

### Decision 9: there is no `aadColumn`

**Believed.** `docs/reviews/2026-08-27-descriptor-specification.md:648-655`
specified `storage.aadColumn` as descriptor state separate from
`storage.rawColumn`, devoting a subsection (`:665-700`) to why it must be
separate: `canonical_aad` binds the physical column name into the AEAD tag
(`encryption/aad.rs:75-98`), so a collection holding **pre-flip rows** would need
its AAD pinned to `"ssn"` while the data physically moved.

**True.** Operator decision, in their words: **"no backward compat, no data."**
The specification's own point 4 conceded that pre-launch "every collection is
authored after the flip and `aadColumn == storage.rawColumn` everywhere". **That
is a field whose only job is to serve rows that do not exist.**

**Note that the shipped struct had already resolved it the other way.**
`FieldStorage` (`crates/zeroship-migrate-core/src/render/gen_types.rs:172-211`)
has no such field, and its doc comment says so explicitly at `:157-159`: "There
is deliberately **no separate `aadColumn`**". One of the two documents was
current and the specification was not.

**What survives, unchanged and still load-bearing: the flip is a re-encrypt, not
a rename.** Carried into the design under "The AAD binds the physical column".

### Decision 10: connections always come from a pool

**Believed.** `acquire_dedicated_client` opening a brand new TCP connection per
transaction was treated as an implementation detail of the transaction path.

**True.** Pooled checkout is the rule, and it reaches further than the
transaction path - three sites create connections and only one is a pool
checkout. The evidence, the exception (logical replication), and the three
questions the decision leaves open are in the design under 3.12, because they
are current.

**What the framing corrected.** Landing `OwnedPooledClient` was described as a
step that was "mechanical and behavior-neutral". It is a **capacity-model
change**: the current concurrent-transaction ceiling is *unbounded*, and moving
to a pooled checkout inverts the failure mode from "one app exhausts
`max_connections` and takes the cluster down for every tenant everywhere" to
"one app occupies the pool and stalls its co-residents on one worker". The
second is much better. It is not nothing, and **that is not a reason to keep
per-transaction connections - it is a reason not to land the change while
calling it neutral.**

### Decision D9: deterministic encryption mode, deferred - and the reason given for it was false

**Believed, and put to the operator in these words.** "`EncryptionMode::Deterministic`
has real runtime ... What has **no** implementation is the capability that
justifies deterministic encryption at all: equality search against an encrypted
value. **A creator can select the mode and cannot use what it is for.**"

**True.** The last sentence is false: **a creator cannot select the mode at
all.** The paragraph was written from the runtime, where the mode is real
(`aad.rs:75-78`, `backend/postgres.rs:1173`, `backend/sqlite/mod.rs:2053`,
`crud/mask_drift.rs:575`, `crud/unmask.rs:234`). It is unreachable from the
authoring surface, which is the only surface a creator has:
`ColType::Encrypted { of }` (`crates/zeroship-migrate-ir/src/ir.rs:670`) carries
the inner type and nothing else, and lowering hardcodes `"mode": "randomised"`
at `crates/zeroship-migrate-core/src/render/lower.rs:9513`.

**This changes the deferral's cost.** "Keep it, decide later" was weighed as
keeping a selectable-but-incomplete feature. What is being kept is reachable
only from Rust. *(Class: a claim about a feature's reachability argued from its
implementation rather than from its entry point. The runtime was real and the
door was not.)*

The argument for deletion was the L11 precedent - a feature with no producer is
closed by deleting it - and the operator chose to defer. The cost that defers
with it: **a mode that reads as supported and is not is the same shape as L11**
(FTS advertised in three layers with no producer) **and L14** (a comment
claiming a guarantee its constant could not provide).

---

## 2026-08-27, corrections inside the document

### The count of thread-local schema readers: 19, then 17, then four that mattered

**Believed.** "There are also 19 direct thread-local `schema_for` reads plus a
transaction-wide enumeration, including six inside the concrete backends' search
paths."

**True.** Seventeen, and four in the backends, not six - and more usefully,
`runtime_schema_for`, the accessor the read and write pipelines use, had exactly
**four** production call sites: `crud/read_pipeline.rs:71`,
`crud/write_pipeline.rs:116`, `:368` and `:380`. Every other hit in a raw grep
was a doc comment, a `*_for_tests` helper (`crud/mod.rs:2486-2494`,
`v8_classes/collection.rs:53`, `v8_classes/db.rs:449`), or the definition itself
(`crud/introspect_schema.rs:104`).

**Why it mattered.** The replacement plan was **keyed to this number**: a step
that says "replace all 19" and finds 17 cannot tell a completed migration from a
missed call site. Two things followed that the wrong count hid:

- **The data plane ran on two schema sources and they were not equals.** The SQL
  *builders* read the declared cache; only the read/write *pipelines* read the
  introspected one. The path being deleted had four consumers, not nineteen.
- **The introspected source was the poorer of the two**, per decision 8's
  entry.

Registration's writers were `register_model/mod.rs:66-104` and its readers
spanned `context.rs:540-603`, `:671-691` and `v8_classes/transaction.rs:66-90`.

### The metadata cost analysis, and the 7x that was really ~18x

**Superseded in premise by decisions 7 and 8** - the data plane no longer learns
live metadata, so there is nothing to make affordable and nothing to cache. The
measurements are kept because two are correct and one was wrong in an
instructive way.

**Finding 1: the catalog read cost `O(TOTAL TENANTS ON THE CLUSTER)`, not
`O(this app)`.** `read_live_schema` filtered on `WHERE n.nspname = $1`, and
**`pg_class` has no index led by `relnamespace`** - verified on the project's PG
16 container, its three indexes are `(relname, relnamespace)`,
`(reltablespace, relfilenode)` and `(oid)`. A leading-column mismatch means the
planner cannot seek to one tenant's tables; it scans and filters. The joined
catalogs (`pg_index`, `pg_constraint`, `pg_attrdef`, `pg_description`) have the
same shape. Measured independently on a throwaway PG 16.14 with synthetic app
schemas: going from 2,000 to 4,000 tenants **doubled** rows scanned and buffers
touched - `pg_index` 32,102 -> 64,102, `pg_constraint` 16,112 -> 32,112 - to
return the **same 112 columns**, at roughly 27.5 ms of catalog execution per
cold-start read at 4,000 tenants. **That cost cannot be fixed by caching**; it
makes each *miss* arbitrarily expensive as the platform grows. Three options
were recorded and none chosen: filter by OID rather than name; **maintain the
descriptor ourselves** - which is what decisions 7 and 8 did; or shard tenants
across clusters.

**Finding 2: cold start introspected the whole app schema once per collection.**
`runtime_schema_for` missed the deploy-keyed cache, called
`read_live_schema(pool, app_id)`, narrowed the result to the one collection, and
cached **only that slice**
(`crates/zeroship-plugin-db/src/crud/introspect_schema.rs:96-110`, DELETED in
`632c1d1fa` when the descriptor became the sole schema authority; read it there).
`read_live_schema` selected every column of every table in the app's schema -
`WHERE n.nspname = $1`, no table predicate - over `pg_attribute` joined to
`pg_class` and `pg_namespace`, `LEFT JOIN`ed to `pg_attrdef` and
`pg_description`, with a correlated subquery over `pg_depend`/`pg_proc` per
column (`crates/zeroship-schema/src/diff.rs:606-640`). An app with N collections
paid **N whole-schema catalog reads** where one would populate all N. *(This is
the cost that dominates at millions of apps, because the long tail is
rarely-hit apps, so a large fraction of requests are cold starts - and it is
invisible in any benchmark that warms one app and then measures steady state.)*
The half-fix that landed opened L18's cross-tenant eviction window.

**Finding 3, and this is the one that was wrong.** A structural measurement
reported a 7.0x multiplier from serialized bytes to resident bytes, flat across
shapes, reasoning that `sizeof(serde_json::Value)` is 72 bytes and
`sizeof(String)` is 24, so every column costs two `Value` slots and two `String`
headers before a byte of payload.

| shape | serialized | "in memory" | ratio |
| --- | ---: | ---: | ---: |
| narrow (8 cols) | 273 B | 1,912 B | 7.0x |
| typical (16 cols) | 551 B | 3,830 B | 7.0x |
| wide (40 cols) | 1,391 B | 9,590 B | 6.9x |

**The real multiplier is ~17-19x.** Three independent reviews measured it: two
used a counting global allocator and got 17-18x and 17-19x; **the third
reproduced ~6.5-6.9x by repeating this table's own structural method, which is
agreement about the method rather than corroboration of the number.** The gap is
the per-column `IndexMap` allocations the structural count never included - the
workspace enables `serde_json/preserve_order` (root `Cargo.toml`), so `Map`
**is** an `IndexMap` and does carry per-map backing storage.

The correction moved every derived figure by ~2.5x: 10,000 entries is ~100 MB,
not ~38 MB; 100,000 apps per thread is ~5 GB, not ~1.9 GB.

*(Class, and it is the most transferable thing in this document: **the
structural count measured what was easy to count rather than what was allocated,
and then reported it as "measured".** A number with a unit and a test behind it
still carries whatever the method left out. The third review's agreement was
method agreement, which is the failure mode that makes a wrong number look
corroborated.)*

**And the recommendation derived from it was wrong in the same direction.** The
document concluded that entries are small individually and the count is what
runs away, so bound by **entry count** rather than bytes. Review measured a
**25x spread** in bytes per entry across realistic shapes, because per-column
cost depends on facets rather than column count: ~230 B/column plain, ~400
B/column once `encrypted` and `mask` are present. The same "10,000 entries"
bound holds ~38 MB at this document's fixture shape, ~67 MB at 16 columns with
facets, and **~476 MB** at 120 columns with facets. **An entry-count bound
admits a 25x range in the very quantity it exists to bound, so it does not
bound.** Resolution: **bound BYTES, or bound entries AND cap per-entry column
count.**

*(Class: **the fixture is the cause of the error.** Every column in it was a
plain `text` with a short name and no facet - simultaneously the cheapest and
the least representative shape. A bound derived from a fixture inherits the
fixture's assumptions silently.)*

**A second recommendation, wrong in the same direction.** The document proposed
deriving the metadata bound from the isolate bound - "the isolate bound times a
typical collection count" - and, better, evicting an app's metadata when its
isolate is evicted. Both are wrong:

- **1:1 coupling defeats the cold-start fix.** `max_isolates` defaults to 200
  per thread (`worker/src/config.rs:129`). Under LRU churn, and under CHWBL
  spill oscillation, isolate evict-then-reload is the common case at target
  scale, so tying metadata lifetime 1:1 to isolate lifetime turns every reload
  into a fresh whole-schema catalog read. **The design would spend a fix and
  then re-buy the problem through the eviction policy.**
- **The correct relation is the inverse.** A metadata entry is ~10 KB; a V8
  isolate is orders of magnitude larger. Metadata entries should **outnumber**
  isolate entries under an independent, larger, byte-capped bound. Isolate
  eviction is a good **pruning hint**, not the right bound.
- **"The isolate bound times a typical collection count" is exactly the sin the
  same section forbids two screens earlier**, where it demands that "nothing
  here should carry a byte number that was not measured". *(Class: the rule was
  stated correctly and then not applied to the next paragraph the author
  wrote.)*

Mechanically the hint IS deliverable for the LRU arm - `evict_lru` runs on the
owning thread, the same thread as the DB context - but no eviction path calls
into plugin-db today; the only reference to plugin-db anywhere in
`worker/src/cache.rs` is the `DbPlugin::new` construction at `:219`.

**The census of unbounded caches was itself incomplete, and that is the lesson
inside the lesson.** Counting four maps on one struct and calling it the census
was the enumeration failure this document keeps warning about: the list was
assembled by grepping one struct, so anything cached outside it was invisible to
the count however careful the count was over what it did cover. The maps found
were `introspected_schemas` (0 eviction sites), `deploy_tokens` (0), `schemas`
(0) and `mask_policies` (1, a targeted per-app remove, not a bound). Review then
found `registered_models` and - the one that matters most - the encryption
`KeyStore`.

**One delegation trap, recorded because a true claim nearly got retracted.**
Grepping `\.resolve(` in `encryption_pass.rs` returns **nothing**, because the
call goes through the backend trait's `resolve_key` wrapper, which forwards to
`key_store.resolve` in `backend/postgres.rs:1294`. One indirection was enough to
make a true claim look false. *(Class: source enumeration fails three ways -
shape, delegation, and `format!` interpolation. This is the second.)*

### `SCHEMA_CHANGING`, its "documented consumer", and a citation pointing at unrelated code

**Believed.** "`SCHEMA_CHANGING` is distinct from `SCHEMA_NOT_APPLIED` and its
hint names the **recovery** command. The distinction is load-bearing: the
sibling code is on the public allowlist precisely so creators can act on it
(`dispatch.rs:236-246`), so conflating them tells a developer to run migrate
against a database left mid-transition. `zeroship migrate --recover-schema-state
<app>` takes the exclusive lease, re-introspects, and compares against the
migration service's own record, not `"<app>"."__zeroship_migrations"`
(`audit.rs:227`), which lives in the app schema and is rewound by restore."

**True, measured 2026-08-27 at `196622c9b`.** Two of the three citations are
false:

- `SCHEMA_CHANGING` occurs **0 times** under `crates/` and `sdks/`.
- `recover-schema-state` / `recover_schema_state` occur **0 times** under
  `crates/`. Every hit in the tree is inside this proposal set or a snapshot of
  it.
- **`crates/zeroship-gateway/src/router/dispatch.rs:231-249` is workflow-outcome
  normalization** - it reads `runId`, `dispatchNonce` and `outcomes`. There is
  no error-code allowlist at those lines. The file's only public-error surface
  is `oidc_callback_public_error` (`dispatch.rs:2777`), which is OIDC-specific.

**One clause in it is true and still matters.**
`"<app>"."__zeroship_migrations"` really does live in the app's own schema
(`audit.rs:227`, `ensure_audit_table_exists`, writing
`CREATE TABLE IF NOT EXISTS "{app_id}"."__zeroship_migrations"` at
`audit.rs:234`), so it really is rewound by a restore and really cannot be the
authority a recovery path compares against. Whatever replaces this inherits that
constraint.

*(Class: **a paragraph in which one verified fact carried two invented ones, and
the verified one made the passage read as though all three had been checked.**
This is why the design's error-contract section now states outright that of its
codes, only `INVALID_ARGUMENT` exists in the codebase, and that no row may be
argued from as though it described shipped behaviour.)*

### The transition writer's signature: three mutually inconsistent versions

**Believed.** Three forms coexisted in one revision - a three-argument form in
one section, a four-argument form in another, and a sentence elsewhere saying
the function mints the epoch while both signatures passed it in.

**True, once fixed.** `publish_schema_state(p_app_id, p_state, p_expected)
RETURNS text`. The caller does **not** supply the epoch: the function mints it,
so entropy is the function's responsibility and cannot be weakened by a caller.
The caller proves it still holds the transition by passing `p_expected`, and the
function performs one atomic
`UPDATE ... WHERE epoch IS NOT DISTINCT FROM p_expected RETURNING epoch`,
raising on a zero-row result.

**Two corrections that had to be made to it, both of which looked finished.**

- **`p_expected` is load-bearing, and an earlier version omitted it** - which
  looked finished precisely because the column and the functions were already
  there. Without it, `deprovision_app(app_id)` cannot distinguish a *delayed*
  retirement of incarnation A from the live incarnation B that replaced it, so a
  retry issued before a recreate tombstones the **new** app. **Not
  hypothetical**: the worker's pending-deprovision set stores bare UUIDs and
  deprovisions by app id alone (`crates/zeroship-worker/src/sync.rs:135-166`),
  and SC-5 explicitly requires a cleanup carrying A not to act on B. The check
  has to be inside the function and atomic with the write: no amount of care at
  the call site can repair a tombstone already written to B.
- **First provision needs its own arm, and an `UPDATE`-only CAS cannot serve
  it.** An `UPDATE ... WHERE ...` against an app with no row matches zero rows
  and raises, so the very first epoch could never be installed, whatever
  `p_expected` was passed. `IS NOT DISTINCT FROM` handles a NULL *epoch* in an
  existing row; it does not conjure the row. The fix must **not** be a bare
  `INSERT ... ON CONFLICT DO UPDATE`, which would let a stale `p_expected`
  overwrite a live epoch. The insert arm is admissible only when the caller
  asserts first provision (`p_expected IS NULL`) and only as
  `INSERT ... ON CONFLICT DO NOTHING`, with a zero-row result treated as a lost
  race.

**And the epoch/incarnation lifecycles must not share a mint.** Two lifecycles
that rotate on different events - a schema change versus a deprovision - sharing
one mint means a migration would silently issue a new app identity and terminally
deny every live handle. The incarnation got its own pair, `deprovision_app` and
`provision_app_incarnation`, both platform-role only, neither letting a caller
choose the bytes it mints. `deprovision_app` sets `deprovisioned_at` and leaves
`incarnation` in place, because the old value is what a stale binding's terminal
denial is compared against.

**What one rotating row does and does not give.** The security properties hold -
a stale binding carrying A reads the current row, sees B, and denies terminally;
a delayed cleanup CAS naming A against a row holding B matches zero rows - but
**"permanent tombstone" overclaims it**: once B is provisioned, the record that
A existed and was deprovisioned is gone, so the row cannot answer "was this id
ever retired, and when". If that history is wanted it needs an append-only
companion. *(Class: a word in a design - "permanent" - implying a durability the
shape does not deliver.)*

**The transition also required a signature change in the migration boundary that
nothing else in the design had noticed.** `apply_ir_documents` takes a **DSN**
and opens its own session inside
(`crates/zeroship-migrate-server/src/apply.rs:235-236`, connect at `:437`), so a
caller has nothing to take a session-scoped lease on. "The lease is taken in the
caller" and "all on the same session" cannot both be true of that shape.
Anything less than passing an already-connected session leaves the CAS fencing
the publish while the DDL runs unfenced - **reintroducing exactly the fail-open
the protocol exists to prevent.** The crash window it guards is real: neither
`zeroship-migrated/src/apply.rs` nor the platform runner
`migrate-adapter/src/platform.rs` can enlist in the vendored engine's per-step
transaction, and the runner's own comment records that "a crash between the
engine apply and `insert_completion_ledger_row` leaves the gap at the END"
(`:951-953`, its only `BEGIN` being the one-time ledger creation at `:611`).

**Lock ordering, because there were three locks over one app.** The engine
already takes its own project advisory lock (`apply.rs:1053-1059`); the
exclusive schema lease is acquired **before** the engine is invoked and never
while the project lock is held.

### The lease acquisition, and a `search_path` that contradicted itself

**Superseded** - there is no per-operation lease. Three results are kept.

**The blocking measurement, which is why try-lock was chosen.** MEASURED on PG
16.14: a blocking shared request behind a queued exclusive waiter stalls to
`lock_timeout` (3105 ms) against a 118 ms control; the try variant returns `f`
behind a queued exclusive and `t` otherwise; and the three-way cycle (op holds
shared, waits on a row lock held by a transaction queued for shared behind the
migration's exclusive) does **not** deadlock - PostgreSQL rearranges the wait
queue and all three commit, so there is no deadlock error to map. Blocking
mattered because a parked operation holds one of 8 pooled connections
(`lib.rs:862`) shared by ~200 apps per worker thread (`exec.rs:1273`).

**A failed try-lock returns a normal result**, so `pg_try_advisory_xact_lock_shared`
returning `f` would let later statements in the same batch run **without the
lease** - the acquisition had to be wrapped to abort the batch with `55P03`.

**The lease key.** v2 proposed "first 4 bytes of `sha256(app_id)` with a fixed
second key". **4 bytes is 32 bits** - the identical birthday bound v2 criticised
`hashtext` for, with the same constant second key. Corrected to a single 64-bit
value, `sha256(namespace || app_id)` truncated to 64 bits.

**The `search_path` pin, and two corrections to it that were both the author's
own.** The pin could not be `''`: the query builder emits unqualified pgvector
types and operators (`query.rs:4931-4932`, `:4965`), and MEASURED, with
`search_path=''` a vector distance fails with `type "vector" does not exist`
against a control returning `0.008540`. Then:

- **"never `public`" contradicted the rest of the sentence.** The confinement's
  extension schema **is** `public`:
  `crates/zeroship-migrate-postgres/src/confinement.rs` sets
  `extension_schemas: vec!["public".to_string()]`, because "pgvector / PostGIS
  install into `public` on the platform/dev image". Both halves could not hold.
  The property actually wanted is stated directly: never the app schema, never
  `pg_temp`.
- **It is not "the same set the migrator role pins."** That pin puts the
  **project schema first** (`provisioning.rs:178-183`), which is precisely the
  entry the data plane must not have. The extension-schema list comes from the
  confinement, not from the migrator's `search_path`
  (`provisioning.rs:178-190`).

*(Class: a sentence that names a source - "the same set X pins" - is a citation,
and citations of behaviour need checking exactly like citations of lines.)*

### Two gate arms that could never fail

**Believed, twice, in two different sections.**

1. A gate arm asserting `EXECUTE` privilege absence on `__zeroship_admin`
   *functions*.
2. A gate arm asserting no Rust file runs `CREATE SCHEMA "__zeroship_admin"`.

**True.**

1. **The property relied on is TABLE privilege absence**, so the function-scoped
   arm would pass on a tree containing a single
   `GRANT SELECT ON ALL TABLES IN SCHEMA __zeroship_admin TO <template>`. And
   `has_table_privilege` alone is **not sufficient**, MEASURED on PG 16.14 with
   a paired control:

   | Grant state | `has_table_privilege(..., 'UPDATE')` | `has_any_column_privilege(..., 'UPDATE')` |
   | --- | --- | --- |
   | none | `f` | `f` |
   | `GRANT UPDATE (epoch)` | **`f`** | **`t`** |

   A tenant holding `UPDATE` on one column defeats the invariant while a
   table-level arm reports green. **That is not a theoretical grant shape**:
   column-level grants are house style one file away -
   `db/migrations-ts/20260818000200_worker_database_authority.ts:77,81,85`
   issues three of them. The arm must use `has_any_column_privilege` as well,
   enumerate the **full** privilege list, and enumerate roles from `pg_roles`
   rather than naming the template, because the property must hold for roles
   that do not exist yet.

   **And it must assert the positive control**: that the worker's login role
   *can* read the table. An absence-only assertion passes just as happily on a
   table nobody can read at all, including one that was never created or was
   dropped - **which would take the entire data plane down while the gate stayed
   green.**
2. **The literal matches zero lines in `crates/` and `libs/` today, and the
   schema is created anyway**, because the real site interpolates the
   identifier:
   `format!(r#"CREATE SCHEMA "{ADMIN_SCHEMA}" AUTHORIZATION "{PLATFORM_ROLE}""#)`
   (`crates/zeroship-plugin-db/src/auth/bootstrap.rs:145-150`). So the arm as
   written passes on today's tree, passes after the deletion it is meant to
   enforce, and passes if a second provisioner is added tomorrow using the same
   idiom - **it cannot observe its own subject.** It must match the interpolated
   form and be **proved against a fixture containing the `format!` spelling**
   before it is trusted. *(Class: the third way source enumeration fails, after
   shape and delegation, and the one that leaves a green gate behind.)*

**A third arm was written as a condition no script can evaluate**: "no
`__zeroship_admin` `EXECUTE` is granted to `PUBLIC` or the app-role template,
**unless the body binds its app-scoped argument to `current_user`**". No gate
can read a PL/pgSQL body and decide whether it binds correctly. **A conditional
a script cannot evaluate is a comment wearing a gate's clothes.** The escape
hatch was made mechanical instead - an explicit allowlist beside the gate, with
the arm asserting the granted set **equals** it - and then decision 6 made the
allowlist empty, which is a stronger arm still: "the schema has no routines" has
no judgement in it and no place for an entry to be added quietly.

*(The starting position was genuinely deny-by-absence: the template held `USAGE`
on the schema with the in-source note that it "does NOT get any direct CRUD on
admin tables", `auth/bootstrap.rs:168-178`, and app roles `INHERIT IN ROLE` that
template, `apply.rs:1686` - which the arm had to see through. Future roles
needed no special clause: every production `ALTER DEFAULT PRIVILEGES` in the
tree is `IN SCHEMA <app|project|zeroship>`, none unqualified.)*

### The deletion was sized from the wrong count

**Believed.** The brief that carried decision 5 said "9 `SECURITY DEFINER`
functions".

**True.** 13 routines, 12 of them `SECURITY DEFINER` (`const_eq` is `IMMUTABLE
PARALLEL SAFE`, `auth/bootstrap.rs:645-649`). The names are `get_mask_policy`,
`set_mask_policy`, `get_column_key`, `const_eq`, `sign_session`,
`verify_signature`, `init_session`, `reset_session`, `rotate_session_keys`,
`ensure_publication`, `ensure_slot`, `ensure_publication_and_slot` and
`watchdog`.

*(Class: **a deletion sized from the wrong count is a deletion that leaves four
routines behind.** The direction was unchanged and the number was larger, which
is the combination most likely to be accepted without checking.)*

### Restore's cache story, and what replaced it

**Believed.** "Restore performs no cache invalidation. It cannot: worker caches
live in other processes and the only channel is the hint path, whose delivery
this design says correctness never depends on. It does not need to: a fresh
128-bit random epoch cannot equal a cached key, so every post-restore operation
misses and re-introspects. Open subscriptions need no separate teardown either -
events produced after the restore are stamped with the fresh epoch."

**True.** There is no epoch, so none of that has a subject, and **what replaces
it is weaker**: a restore that changes the schema is invisible to a running
worker until it restarts. The operational rule is **roll the workers after a
restore**, and it is a procedure rather than a mechanism. The subscription half
loses more - nothing in the event stream says the schema changed, and nothing in
this set replaces the signal.

**Two live facts about the provisioner that the epoch design surfaced and that
survive it.** The reason the epoch row needed a platform-owned schema rather
than a table in the app's own schema was that a table in the app schema is
tenant-writable by default: the live provisioner
(`crates/zeroship-migrate-server/src/apply.rs:1694-1707`) contains **zero `REVOKE`
statements**, and its `ALTER DEFAULT PRIVILEGES` auto-grants full DML on every
future migrator-created table, while the reserved-prefix revoke lives in
`ensure_per_app_role`, which has **zero production callers**. That is a
measurement about the provisioner, not about the epoch, and it still holds.

**And restore's home was named but its machinery never existed.**
`restore` at `crates/zeroship-plugin-db/src/backend/postgres.rs:803` is the site the design
moves out of the data-plane crate into the migration service - **which today has
no restore machinery at all**, so the tooling (`pg_restore` invocation, snapshot
handle, blob access) is named work rather than a move.

**One property the epoch design paid for and had correctly identified.**
`app_schema_state` was **itself restorable state**: a partial recovery can
reinstate an older row and with it an epoch a worker cache still holds - the
same ABA the design rejects the migration-digest epoch for. The primary rule was
**mint before visible**, which covers app-scoped restore but **not**
operator-driven PITR, where recovery happens outside the system entirely. That
is why the cache key was bound to `(system_identifier, timeline_id)` rather than
`system_identifier` alone: `system_identifier` identifies the **cluster**, so a
same-cluster PITR *preserves* it and would alias straight back onto a live cache
entry. An earlier draft got that wrong. Both values are one cheap read, MEASURED
together on PG 16.14 via `pg_control_system()` and `pg_control_checkpoint()`.

### The step order contradicted its own stated dependency

**Believed.** The implementation sequence printed the cutover before the
identity substrate while calling the substrate a prerequisite of it.

**True.** The cutover constructs the binding, and the binding must already carry
`app_incarnation` and be checkable against the authority domain. Under the
printed order none of that existed yet, and the only ways to satisfy the step
would have been a placeholder incarnation, a bare app id, or a lazy "adopt
whatever exists at first use" - **each of which reinstates the stale-handle and
same-id recreation hole Fork C was adopted to close.**

*(Class: **a step order that contradicts its own stated dependency is a defect in
the plan, not a presentational quibble** - an implementer follows the numbers.
Building the fence after the thing it fences is a window.)*

### "Fix every live defect independently" could not be followed

**Believed.** Step 1 said to fix every live defect in the register
independently, each with its own regression test, and named four as independent:
L4, L5, L7, L8.

**True.** L1, L2, L3 and L6 are coupled to later steps by their own intended end
states - L1/L2 end in deleting the policy writers with an artifact replacement,
L3 in deleting `__zsSchemaReady`, and L6 *is* the absent production provisioner.
Fixing them independently would mean inventing throwaway intermediate APIs,
which the no-shim rule forbids. And of the four named, L5 and L8 had since
landed and L7 was reclassified as a missing gate arm rather than a defect.
**Independent today: L4, and only L4** - re-derived from the register's status
markers rather than carried forward.

### Registration removes four effects, not three

**Believed.** Deleting registration removes three distinct effects.

**True.** Four, and the module's own header enumerates them
(`register_model/mod.rs:1-35`). The earlier count collapsed the first two, which
are separate mechanisms with separate writers: `cache_schema` (the declared JSON
the readers consume) and `mark_model_registered` (a per-thread flag written
independently of it). The other two are what that flag gated - `runtime_schema_for`
returned `None` for an UNREGISTERED collection
(`crud/introspect_schema.rs:64-78`), so **absence of registration meant
unprotected** - and the SQLite `ATTACH` (`register_model/mod.rs:122-129`,
`:173-206`), which the module itself calls "in the wrong place, and that is a
known item".

The reachability of the third is in the design under step 5c, because it is
current: `db.collection(name)` mints a collection for any non-empty string
(`v8_classes/db.rs:119-139`) and `schema = None` yields `SELECT *`
(`crates/zeroship-schema/src/query.rs:3000-3012`).

### The command-tag obligation, and where it was being dropped

**Believed.** Driving raw `BEGIN`/`COMMIT`/`ROLLBACK` from an owned session was
treated as equivalent to using the driver's borrowing wrapper.

**True.** The wrapper discharges an obligation the raw path does not: PostgreSQL
may answer `COMMIT` with a `ROLLBACK` tag, which `transaction.rs:54-59` detects
and **plugin-db's raw executor discarded** (`backend/postgres.rs:201-211`, as of
2026-08-26). A session that drives its own terminal SQL must inspect command
tags. This shipped as `exec_terminal_on_tx` (`transaction/mod.rs:139`) and is
recorded as L8 FIXED.

*(Class: "use the raw primitive instead of the wrapper" is never a neutral
substitution - the wrapper is where the accumulated obligations live, and they
are invisible at the call site.)*

### `zeroship-schema`: "the crate stays" and "the crate goes" were both asserted

**Believed.** Both, within one day, in the same document set.

**True.** Neither is supportable, and what is measurable was stated instead:
`read_live_schema` and `estimate_row_count`
(`crates/zeroship-schema/src/lib.rs:22-23`) lost their only consumer; the other
five modules still have plugin-db callers nobody has audited. **The crate is
15,034 lines across 7 modules** and this document set does not claim it is
retired.

**And `AGENTS.md`'s crate index is stale about it**, which is why it is worth
saying twice: the landing page says the crate is "reused by the migration engine
(write/diff) + plugin-db's data plane (read/introspect)", and the first half is
false - measured 2026-08-27, no `crates/zeroship-migrate*` crate depends on it
or uses it. **A wrong line in the landing page gets repeated by everyone who
reads the landing page, and it was repeated into a decision brief.**

*(One measurement trap that nearly produced a second wrong claim:
`grep -rl zeroship_schema` also matches
`crates/zeroship-control/tests/registry_schema_test.rs`. That hit is a test
function **name**, `registry_core_tables_live_in_zeroship_schema`, referring to
the PostgreSQL schema called `zeroship`, not to this crate. Spelling, not
behaviour.)*

### Landing the descriptor cutover: two traps

**L24 turned out to be three fail-open arms, not two** - `build_find`'s
`Ok("*")`, the PostgreSQL search path, and the SQLite vec0 search path. **The
third surfaced only because the fix was a TYPE change** (`Option<&Value>` ->
`&Value`) and the compiler found it. A guard would have been added to the two
someone had already noticed. *(Class: making the bad state unrepresentable finds
the instances a guard would have missed.)*

**A descriptor cutover has a build artifact in its blast radius that no
`git status` shows.** The fix initially failed with `expected v1` even with a
correct v2 descriptor, because `sdks/bootstrap/dist/` was **stale** - the
committed `src/` was on v2 and the gitignored build output was not. A rebuild
fixed it, and `@zeroship/db`'s dist came back byte-identical, so bootstrap was
the only staleness.

**And it opened a coverage gap invisible from a green suite**, recorded in the
design under section 5: `distributed_live` was the tree's only live boot of a V8
isolate with `env.db` and no descriptor, and fixing it to ship a descriptor
means nothing now observes `collection_not_declared` end to end.

### Fork A, Fork B and Fork C were cited by name in four contracts and defined in none

**Believed.** SC-1, SC-2, SC-5 and SC-6 referred to "Fork A", "Fork B" and
"Fork C" as established terms.

**True.** They were review shorthand that leaked into the contracts. They are
now defined in the design under 3.3.

*(Class: a term that circulates in review becomes load-bearing in a document
without anyone noticing it was never defined.)*

### Two contracts were presented as pending after they had been settled

**Believed.** A paragraph listed "a non-SQLite dev URL" and "the
deprovision/recreation lifecycle" as open decisions.

**True.** Both were made - typed rejection (SC-4) and a durable
`AppIncarnationId` qualified by the authority domain (SC-5, Fork C) - so **an
implementer reading only the parent document would have re-opened questions the
sub-contracts had settled.**

---

## v2 to v3: the corrections table

Round 2 reviewed the v2 revision and found ten errors introduced *by the
revision*, plus eight sentences that could not be turned into an unambiguous
failing test. Round 2's decisive finding was that **v2 asserted contracts it did
not contain** - which is why the six sub-contracts exist as prerequisites with
owners and acceptance shapes, rather than as prose in the parent.

Nothing in this table is a reviewer's opinion; each was verified against the
code or measured.

| # | v2 said | Why it was wrong | v3 |
| --- | --- | --- | --- |
| 1 | Add `SET LOCAL search_path = ''`, "free in the same statement" | MEASURED: it breaks vector search. `query.rs:4965` emits `$1::vector` and `<=>`/`<->`/`<#>` unqualified; `provisioning.rs:178-190` pins a role-level `search_path` so they resolve. Control: `0.008540`. With `search_path=''`: `ERROR: type "vector" does not exist` | Pin to `pg_catalog` plus the confinement's extension schemas; never `''`, never the app schema |
| 2 | Read the epoch through a `SECURITY DEFINER` wrapper whose `app_id` "is validated inside the function" | The read runs *before* `SET LOCAL ROLE`, so `current_user` is the shared pool role for every tenant. **There is nothing to bind the argument to**; the sentence claims an authorization property it cannot deliver | No read wrapper. Plain schema-qualified `SELECT` as the pool login role, with the table unreachable by any app role |
| 3 | Follow "the shape `get_mask_policy` / `set_mask_policy` already use" | **Those are the defect, not the pattern**: `SECURITY DEFINER` + `GRANT EXECUTE TO PUBLIC` with no caller check (`bootstrap.rs:550-551`, `:589-590`), and `get_column_key(p_key_id)` is keyed by `key_id` alone (`:628-635`) | Do not copy them; fix them |
| 4 | "Delete the `set_mask_policy` `EXECUTE` grant from the app-role template" | The grant is `TO PUBLIC`, not to any template - the actual grantees are at `bootstrap.rs:587-595` and `:548-556`. **Executing this instruction is a no-op** and leaves every role able to write any app's policy | Drop the function; `REVOKE ... FROM PUBLIC` on its sibling |
| 5 | One batch: `BEGIN`, try-lease, epoch read, `SET LOCAL`s | A failed `pg_try_advisory_xact_lock_shared` returns `f`, a **normal result**, so the later statements still run and the epoch is read **without the lease** | Wrap the acquisition so failure aborts the batch with `55P03` |
| 6 | The dynamic callback "reads the referrer's id directly" from `host_defined_options` | Verified against the `Cargo.lock`-resolved v8 147.1.0: `data.rs:458-463` has no `impl_try_from! { Data for PrimitiveArray }` and no `is_primitive_array()`; only identity comparison exists (`:462`) | Per-runtime table of stamped handles, compared **by identity** |
| 7 | Lease key = "first 4 bytes of `sha256(app_id)`" with a fixed second key | 4 bytes is 32 bits. **The identical birthday bound v2 criticised `hashtext` for**, with the same constant second key | Single-bigint form, 64 bits of `sha256(namespace \|\| app_id)` |
| 8 | "Every transaction slot **and claim**" keyed by `(runtime_instance_id, tx_id)` | Unique transaction ids never contend. The existing app-keyed claim **deliberately** serialises two same-app top-level begins from before `BEGIN` through settle (`transaction/mod.rs:312-335`) | Separate registry identity from an explicitly chosen admission key |
| 9 | The SPI carries a policy-store capability | Contradicts v2's own section on policy ownership, which moves it to the control plane before the isolate exists | Deleted |
| 10 | The effective policy is "resolved before the isolate is built" | Resolved *once*. Lowering the operator ceiling then never reaches a pinned isolate. **v2 traded a forgeable policy for a non-revocable one** | Freeze only the declared half; resolve the ceiling at authorization time *(later reversed again by decision 4 - see that entry)* |
| 11 | Five state names constitute the transaction state machine | **They are labels, not a protocol.** Missing at minimum a `RollbackOnly`/`Poisoned` health state | Named as a required sub-contract (SC-1) |
| 12 | A creator transaction's lifetime is "bounded by a deadline enforced by the settle path" | **Circular**: no settle path runs for a body that never settles | Deadline enforced by an independent timer, in SC-1 |
| 13 | Invariant 7: no data-plane path executes DDL | Contradicted by live code: `write_audit_unmask_row` runs on the **denied** path (`unmask.rs:410`) and the granted one (`:433`), and begins with `ensure_audit_unmask_table` -> `CREATE TABLE IF NOT EXISTS` | Delete the lazy DDL sites *(the enumeration was owed until 2026-08-28 - see that entry)* |
| 14 | Restore invalidates every live-cache entry "across all epochs" | Restore runs in a different process from the worker caches, and v2's own rule says correctness never depends on hint delivery | A 128-bit random epoch makes invalidation unnecessary *(moot under decision 7)* |
| 15 | Delete the bare-specifier table | It has **three** arms; the third is `zeroship`, the creator-facing `env` facade, needed because the static BFS does not compile dynamically-only imports | Delete two arms, keep `zeroship` |

**v3 also adopted a dependency-correct sequence** in place of v2's seven merges,
which were inverted in seven concrete ways - notably deleting the registration
writer before replacing its readers, promising CDC epoch behaviour before the
epoch existed, and calling a step containing a driver extension, an actor
redesign, a compiler IR and a crate retirement "mechanical and behavior-neutral".

**And v4's own mechanism was different from v3's, which is worth recording
because it is the reason this document exists.** v3 corrected v2 in a table at
the front. v4 corrected itself **inline**, in boxed or bolded passages beside the
claim they retracted - so a reader who stopped at the table concluded the
document had been corrected once, when it had been corrected in both places. By
2026-08-28 there were forty-three retraction markers across the two mechanisms.
The split into a specification and this log is the third mechanism, and the first
one where a reader can finish either document without holding the other in their
head.

---

## Sub-contract acceptance shapes, and why each could not be discovered under TDD

Recorded here because the *reason* each is a prerequisite is historical - it is
what round 2 found - while the contracts themselves are live documents.

| # | Sub-contract | Why it cannot be discovered under TDD | Acceptance shape |
| --- | --- | --- | --- |
| SC-1 | Explicit transaction protocol | A black-box suite passes on the wrong concurrency semantics. Whether two same-app top-level transactions serialise is user-visible and currently deliberate | A state table plus a test per illegal transition; explicit statements for "second same-app begin" and "settlement arriving while an operation owns the client" |
| SC-2 | SQLite actor protocol | Two decisions change documented user-visible behaviour: whether autocommit work stalls behind an app's open creator transaction (today it does, `tx_route.rs:119-124`), and whether cancellation interrupts an in-flight statement | Concurrency arm: app A's autocommit **reads** proceed while A holds an open explicit transaction - **reads**, not ops, since SQLite has one writer per database on any number of connections, so an "ops" arm cannot pass. Interrupt arm: cancellation takes effect *during* a long statement |
| SC-3 | `DbPlan` IR and source ledger | `query.rs` runtime builders alone are ~3,100 lines (`:2907-6011`, against DDL/schema rendering at `:1019-2905`). An IR discovered incrementally will be shaped by whichever call site is ported first | A ledger whose source column is exhaustive and whose unported count reaches zero, checked by a gate |
| SC-4 | Dev and HMR mechanism | "A fresh dev isolate" names an outcome, not a mechanism. The server builds one runtime under one accept loop (`serve.rs:1719-1795`) | One observable restart/swap contract with a test that a removed collection is absent afterwards |
| SC-5 | Service ownership | The plugin set is memoised **per thread**, not process-wide: `build_runtime` calls `plugin_set()` (`cache.rs:426`), which caches `create_plugins()` into a `thread_local!` (`:194-204`), so an n-thread worker holds n plugin sets and a process-wide cache has no owner. Deletion still reparses the URL and opens a second pool (`lib.rs:901,904`) | Current and pinned runtimes on one thread share exactly one backend slot and one cache, **and** a deprovision arriving while an isolate holds a handle has defined behaviour |
| SC-6 | Ceiling contract | The meet is not a map intersection, and getting that wrong **inverts** revocation for `auto` - the actor with the most access. That failure is invisible to every same-key fixture | A ceiling that revokes `auto` denies `auto` when the draft does not mention `auto` at all; **and an unmask the ceiling still permits succeeds in the same test** - a deny-only arm passes on an implementation where the meet is broken and everything is denied |

**SC-5's cell carried its own stale claim**, worth keeping as an instance: it
read "calls `create_plugins()` inside each `build_runtime`", which was true when
written and was fixed in `22c4d75f1`. It was retracted in SC-5 while the parent's
copy went stale. **The per-runtime mint is gone; the per-thread scope is what
SC-5 is actually about.** *(Class: one fact, two documents, one of them
updated.)*

**SC-6's cell was rewritten by decision 4, and the rewrite deletes the question
rather than the answer.** It previously read "Where the app-current mask ceiling
lives, who writes it, how a data operation observes a newly committed value, and
that read's linearization point", with an acceptance shape requiring a lowered
ceiling to deny "with no rebuild and no deploy". There is no ceiling row, so
there is no newly committed value to observe and no linearization point to
establish. **The justification column's original text - that a cache keyed by a
version the reader can only learn by reading cannot discover a new version - was
true**, and is retained in SC-6 as the reason the runtime-resolved shape was
abandoned rather than fixed.

---

## Recurring classes, collected

The entries above name a class each time it appears. Collected, because the
repetition is the finding:

1. **A number published without its boundary cannot be checked, only repeated.**
   The `RETURNING *` count (34, then 12, then 20/14 with the boundary stated);
   the `schema_for` readers (19, then 17, then the four that mattered); the
   `SECURITY DEFINER` routines (9, then 13); the registration effects (3, then
   4); the lazy DDL sites (3, unenumerated, then 6 sites over 3 tables).
2. **A mechanism examined for whether it was correctly built rather than for
   whether its question was still being asked.** Decision 6's surviving row (safe
   and redundant); decision 5's `session_nonces` (correct atomicity argument for
   a session that should not exist); the WAL epoch carrier and SC-6's ceiling
   read (both carefully verified answers to questions that stopped being asked).
3. **A guard bound to the wrong thing.** The `EXECUTE`-scoped privilege arm
   against a table-privilege property; `has_table_privilege` against a
   column-level grant; the literal `CREATE SCHEMA` grep against an interpolated
   site; a source gate scoped to a path that does not exist.
4. **A refusal stated in a design document is not a boundary.** Two documents
   demanded the mask policy not be an isolate input while live code supplied
   exactly that, and neither statement caused the contradiction to be found.
5. **One verified fact carrying unverified ones.** The `SCHEMA_CHANGING`
   paragraph, where one true clause about `__zeroship_migrations` made two
   invented citations read as checked.
6. **Enumeration failing three ways - shape, delegation, and `format!`
   interpolation.** The cache census assembled by grepping one struct; the
   `resolve_key` wrapper hiding a true claim; the interpolated `CREATE SCHEMA`.
7. **A measurement that measured what was easy to count.** The 7x structural
   multiplier reported as measured, corroborated by a third review that repeated
   the same method.
8. **A fixture's assumptions inherited silently.** The entry-count bound derived
   from an all-plain-`text` fixture, admitting a 25x spread in the quantity it
   existed to bound.
9. **A rule stated correctly and not applied to the next paragraph.** "Nothing
   here should carry a byte number that was not measured", followed immediately
   by "the isolate bound times a typical collection count".
10. **One criterion with two halves, reported green by exercising the easy
    one.** The delivery arm whose mask-plaintext half was implementable while its
    epoch half was not.
