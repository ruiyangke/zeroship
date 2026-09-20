# Platform service database split

**Status. PROPOSED. NOTHING IN THIS DOCUMENT IS IMPLEMENTED.** No table has
moved, no database has been created, no grant has been revoked. What exists is
one PostgreSQL database holding every platform table, six login roles over it,
and one service - the worker - already fenced out of the platform schema
entirely.

**The goal.** Each service owns its own database. A service reaches another
service's data through that service's API, never through a shared connection.
The worker cannot reach the control plane's data at all.

---

## What exists today

`deploy/ops/postgres-init.sql` states it plainly: "The whole platform shares ONE
database". Every service DSN in `deploy/compose/docker-compose.yml` and
`deploy/helm/zeroship/values.yaml` resolves to the same host and the same
database, differing only in login role.

Separation today is by ROLE and GRANT, not by database. That separation is real
and in one case strict: `db/migrations-ts/20260702000900_grants.ts` revokes ALL
privileges and schema `USAGE` from `zeroship_worker`, and
`crates/zeroship-worker/src/db_posture.rs` refuses to boot if the login can
still reach the `zeroship` schema. **The worker is the model this proposal
generalises.**

## The ownership rule

**The service that WRITES a table owns it.** A reader can be migrated to an API
call; an owner cannot. Where a table has more than one writer, ownership goes to
the service whose invariant the table protects, and every other writer becomes a
caller.

Ownership is not recorded anywhere in the tree today.
`policies/platform-table-owners.json` assigns every platform table to a single
`zeroship_platform` owner, which is a placeholder rather than a map.

---

## The assignment

### `auth` - identity, credentials, sessions

    users                       the root of the identity domain
    sessions                    session objects
    grants                      consent grants
    device_grants               RFC 8628 device flow
    email_verifications
    federated_identities
    idp_sessions
    magic_links
    magic_completions
    oauth_authorization_codes
    oauth_clients               CONTESTED - see below
    oauth_grants                CONTESTED - see below
    oidc_session_clients
    signing_keys                read by gateway today
    totp_credentials
    totp_backup_codes
    identity_links              CONTESTED - written only by control's pool
    principal_grants            CONTESTED - same
    rate_limits                 written by the authn library on two pools
    cron_state                  no reader found; auth-granted

### `gateway` - edge sessions and anchors

    gateway_sessions            CONTESTED - auth deletes from it
    app_session_anchors         CONTESTED - auth revokes through it
    app_user_identities         CONTESTED - three writers
    token_revocations           CONTESTED - three writers
    dpop_jti                    no reader found; edge-shaped

### `control` - organizations, projects, apps, billing, deploys

    organizations               organization_members     organization_roles
    organization_invites        organization_accounts    organization_account_history
    organization_billing        organization_billing_status
    organization_billing_status_history                  organization_fee_policy
    projects                    project_members          project_data_keys
    apps                        app_vars                 app_secrets
    app_env_expose              app_scope_defs           app_egress_rules
    app_audit                   app_spend_limit          app_spend_state
    spend_state_history         app_usage                app_usage_history
    app_oauth_clients           app_deploy_commands      app_lifecycle_intents
    plans                       plan_change_events       pricing_config
    metric_weights              usage_aggregates         billing_metrics
    billing_customer_refs       billing_provider_refs    billing_line_provider_refs
    billing_disputes            billing_notifications    pending_disputes
    billing_reconciliation_findings                      connect_checkout_failures
    invoices                    invoice_lines            invoice_payments
    credit_ledger               refunds                  refund_provider_refs
    payouts                     payout_failures          stripe_events_seen
    provider_dead_letter        execution_zones          authz_decisions
    audit_events                CONTESTED - three writers
    worker_instances            CONTESTED - three consumers
    worker_join_tokens          worker_join_token_claims
    worker_join_signers         worker_join_signer_zones

### `migrate-server` - schema application and cluster convergence

    datastores                  self-registered from pg_control_system()
    app_schema_applies          the apply ledger
    databases                   CONTESTED - control declares, migrate converges
    database_bindings           CONTESTED - same

### `workflow` - durable execution

    workflow_manager.*          all eighteen: workers, queue_scopes,
                                deployment_holds, jobs, assignments,
                                placement_receipts, management,
                                management_scopes, schedule_deployments,
                                schedule_activations, schedule_disables,
                                schedule_scopes, schedules,
                                schedule_occurrences, recovery_scopes,
                                recovery_duties, capacity_demands,
                                capacity_targets
    workflow_policy_ledger      workflow_rollout_config
    app_deploys                 CONTESTED - workflow's DDL, control writes
    app_deploy_holds            same

### `mailer` - suppression list

    email_suppressions

### Shared by design, owned by nobody

    service_authn.service_assertion_replay

The header of `db/migrations-ts/20260816000100_service_assertion_replay.ts`
argues from "exactly one shared, strongly consistent store". That argument is
weaker than it reads: the replay key is `<iss>|<jti>`, `aud` is verifier input,
and an assertion is verified by exactly one service. The real requirement is
that all REPLICAS of one service share a store, which a per-service database
satisfies. The CDC relay already opts out with an in-memory store.

---

## The contested tables, and how each resolves

**`oauth_clients`, `oauth_grants`.** Both auth and control INSERT them. The
registry is auth's; control creates clients on a creator's behalf. **Auth
owns**; control's creation becomes an API call.

**`identity_links`, `principal_grants`.** Written only by
`crates/zeroship-authn/src/platform_cli.rs`, executed on control's and
migrate-server's pools - the auth SERVICE never touches them. They FK-cascade
from `users`. **Auth owns**; the materialisation becomes an auth API call. This
is the sharpest case for the ownership rule, because asking which crate contains
the SQL gives the wrong answer and asking whose pool it runs on gives the right
one.

**`app_user_identities`, `token_revocations`.** Three writers each, and they are
the revocation substrate.
`crates/zeroship-control/src/oauth_grants_handlers.rs` and
`crates/zeroship-auth/src/store/relay.rs` are deliberately coupled read-side to
write-side in place of a lock. **Auth owns both**; gateway and control publish
revocations through auth.

**`gateway_sessions`, `app_session_anchors`.** Gateway writes them in the hot
path; auth deletes through them during erasure and reset. **Gateway owns**; auth
calls gateway to revoke.

**`audit_events`.** Three writers, no production reader, and a trigger
(`zeroship.audit_events_block_tamper`) whose disarm switch is set only by auth's
retention sweep. Append-only with no reader is the shape of a sink, not a table.
**Control owns** the store; auth and gateway append through it. The alternative -
one audit store per service - is defensible and cheaper, and should be decided
before the split rather than after.

**`worker_instances`.** Three consumers, two purposes: workflow-server and the
CDC relay resolve a worker's public key from it, and workflow-manager reads
`{id, status, execution_zone_id, expires_at}` for scheduling without the key.
**Control owns** it, because control mints instances through
`join_worker_instance`. The key read becomes a control API; the scheduling read
becomes a projection. A split that reasons only about the key column will move
the table and break the manager, which needs the row to exist rather than the
key.

**`databases`, `database_bindings`.** Control declares, migrate-server
converges. This is already the declare-and-converge split, with
`generation`/`observed_generation` as the seam. **Control owns the rows;
migrate-server owns the convergence.** They stay in control's database and
migrate-server writes observations through control's API - which is the one
contested pair whose answer the tree already contains.

**`app_deploys`, `app_deploy_holds`.** DDL lives in
`crates/zeroship-workflow-manager/schema/deployments/schema.ts`; control writes
them inside its deploy transaction. **Control owns**; the manager reads through
a projection.

---

## What the split breaks

Four constraints, each measured rather than assumed. The re-runnable checks are
named; no counts are recorded here because they change.

**1. Account erasure is implemented as a foreign-key cascade.**
`crates/zeroship-auth/src/cron/account_reaper.rs` contains exactly one delete -
`DELETE FROM zeroship.users` - and the cascade is the entire cleanup. Across
databases that statement succeeds, reports success, and leaves rows in every
other service behind. A cascade that reaches nothing is indistinguishable from
one that had nothing to reach.

*Replacement:* erasure becomes a declared intent with per-service acknowledgement,
the shape `database_bindings` already uses for convergence.

**2. The authorization ladder join spans auth and control.**
`crates/zeroship-authz/src/authority.rs` joins `users` to control's membership,
role and project tables in one statement, and `zeroship-authz` is linked into
auth, gateway, control and migrate-server.

*Measured: per decision, uncached.* `authority::resolve` has exactly one
production caller, `enforce` in `crates/zeroship-authz/src/eval.rs`, reached
from `crates/zeroship-control/src/authz_guard.rs`. The only cache in the crate
is `RevocationCache` in `crates/zeroship-authz/src/wrapper_revocation.rs`,
which caches revocations and not authority.

**So the auth/control cut needs a `users` PROJECTION in control's database, not
an API call.** A network round trip per authorization decision, in four
services, is not viable on that path. The query bounds what that projection may
be, in ways that are read off it rather than chosen.

*It carries `id`, `email_verified_at` and `locked_until`, and nothing else.*
`resolve` reaches `users` through a single constant in
`crates/zeroship-authz/src/authority.rs`:

    const USER_ATTRS: &str = "u.email_verified_at IS NOT NULL AS email_verified, \
         (u.locked_until IS NOT NULL AND u.locked_until > NOW()) AS account_locked";

Every other table those statements name - `organization_members`,
`organization_roles`, `projects`, `project_members`, `apps`, `databases` - is
one this document assigns to control, so they do not cross the cut at all. The
ladder join looks like a wide cross-service query and is a narrow one.

*It must carry `locked_until` raw, never the derived boolean.* `account_locked`
is computed against `NOW()`. A materialised copy of it is correct at the instant
it is written and silently wrong afterwards, and it goes stale with no write to
replicate - there is no invalidation signal to miss, because nothing happened.
Control has to hold the timestamp and evaluate the comparison itself.

The rule generalises past this column, and is worth stating as a rule because
the next projection will face it too: **a projection may carry facts; it must
not carry a conclusion computed against the current time.** A fact goes stale
only when something writes, which is a problem replication is built to solve. A
clock-derived conclusion goes stale when nothing happens at all, which no
replication topology and no invalidation signal can address.

*Lag on it is a security window, not a latency budget.* The crate states the
property it buys by not caching. `crates/zeroship-authz/src/authority.rs`: "its
result is never stored: a membership row removed by a committed transaction is
invisible to the very next request in every process, with no invalidation signal
to build, publish or miss". And `crates/zeroship-authz/src/lib.rs` points at it:
"Nothing is cached: see [`authority`] for why the cache that used to sit here
was deleted rather than fixed." The cache was not an oversight that a projection
now gets to repeat - it was removed on purpose to buy this. A lagging projection
reinstates exactly what was deleted to get that guarantee, and on `locked_until`
the consequence is concrete: an account locked in auth keeps authorising in
control for the length of the lag.

That makes this cut a distributed-systems problem rather than a refactor, and it
is why the sequencing puts it last - with the caveat recorded there.

**3. Auth takes a row lock on control's `organizations` while deleting `users`.**
`refuse_if_it_strands_an_organization` holds `SELECT ... FOR UPDATE` on a
control table inside the transaction that deletes an auth row, and discards the
result - the query exists only to take the locks.

**Two races, closed two different ways, and only one is visible in the SQL.** A
brand-new seat is closed for free: an `INSERT` into `organization_members` takes
`FOR KEY SHARE` on the referenced `zeroship.users` row for its foreign key, and
the reaper already holds `FOR UPDATE` on exactly that row. A PROMOTION is not -
`transfer_ownership` raises a sitting member with a plain `UPDATE ... SET role`,
which touches no key column and therefore takes no lock on `users`. That is why
the lock covers every seat and `role = 'owner'` is asked only in the re-check,
which runs in a later snapshot.

A split loses both the free referential interlock and the deterministic lock
order (`ORDER BY o.id` with `LockRows` above `Sort`) that stops two reapers
erasing two co-owners from deadlocking. A read-plus-retry can be built, but it
is optimistic concurrency replacing pessimistic serialization the database was
providing at no cost.

**4. Three data-modifying CTEs write across service boundaries in one
statement.** `password_reset::complete`,
`revoke_user_app_credentials_in_transaction`, and
`platform_cli::materialize_default_grants`.

**The writes are not what forbids decomposition - two of the three are already
idempotent.** `token_revocations` upserts `ON CONFLICT ... DO UPDATE SET
revoked_after = GREATEST(existing, EXCLUDED)`, which is monotonic and safe to
replay; the session revocation is guarded `WHERE revoked_at IS NULL`.

What forbids it is the ENTRY GATE. The whole statement hangs off a `candidate`
that selects a magic link only `WHERE consumed_at IS NULL AND expires_at >
NOW()`, and consumes it in the same statement: a single-shot token. Decomposed,
a crash after the token is spent and before the gateway's
`app_session_anchors` write leaves the password changed, the token gone, and the
app sessions live - with nothing left to re-drive from. The idempotent writes do
not help, because they have become unreachable.

*Replacement:* the gate has to outlive the statement - a reservation the
authorising service can re-issue against, rather than a row consumed in the same
breath as the work it authorises.

---

## Sequencing

**1. Worker.** Already free: no platform SQL, and a boot gate that refuses
platform-schema reach. The change is a separate database, a role with no grant
on it, and extending `crates/zeroship-worker/src/db_posture.rs` to check
database identity rather than schema reachability.

**2. CDC relay.** One narrow read - `worker_instances.public_key` - and an
in-memory replay store already. It becomes a control API call.

**3. Gateway.** No production read-joins; every statement is single-table. That
property is what makes this step mechanical, but it is not what makes it small.
Beyond the session and anchor tables this document gives it, the gateway reads
`users`, `oauth_clients` and `oauth_grants` from auth, and `apps`, `projects`,
`organizations`, `plans` and `app_oauth_clients` from control, plus the shared
`audit_events`. Every one of those becomes an API call. The step is ordered
third because single-table reads convert one at a time without decomposing a
join, not because there are few of them.

**4. Workflow.** Already a separate schema with its own migrator role: the
corpus carries `zeroship_workflow` and `zeroship_workflow_migrator`, and
`db/migrations-ts/20260919000000_workflow_journal.ts` and
`db/migrations-ts/20260911000000_workflow_coordination.ts` build the schema. Its
cross-service reads are column-scoped projections of control tables.

**5. migrate-server.** Needs a role of its own first - it currently logs in with
control's credential, and there is no `zeroship_migrate` role in the corpus.

**6. auth and control together.** Last, and the only one that requires
solving erasure, the authorization join and the cross-service CTEs. Nothing
above it is blocked by it.

*Ordering alone may not be enough for it.* "Last" is the right position, but
position is not the mechanism. If the `users` projection cannot be made
synchronous with the lock write, an asynchronous copy is a live authorization
gap for the length of the lag no matter when the cut is taken - deferring it
changes the date, not the property. Closing it needs a choice this document does
not yet make: a lock write that reaches control before it commits, or control
reading `locked_until` from auth on the authorization path, which is the round
trip claim 2 ruled out on cost. This is the one place where the split is not yet
shown to be buildable as specified.

---

## Re-deriving the claims above

Every sentence in this document that describes what a service reads is a claim
about code, and none of it is checkable by anything that checks citations: a
path resolving says nothing about whether the sentence beside it is still true.
So rather than ask a reader to trust the prose, here is how to re-derive it.

What a service reaches in the platform schema, as SQL:

    grep -rhoE '(FROM|INTO|UPDATE|JOIN)[[:space:]]+zeroship\.[a-z_]+' \
        --include='*.rs' crates/<crate>/src/ | grep -oE 'zeroship\.[a-z_]+' | sort -u

Run against `zeroship-worker` this returns nothing, which is the worker claim in
step 1. Run against `zeroship-gateway` it returns the set step 3 names. Whether
a role exists is the same question asked of the corpus:

    grep -rho 'zeroship_[a-z_]*' db/ | sort -u

Two ways this sweep lies, both of which it did while this section was written:

**`zeroship\.` is not a schema reference.** The bare pattern also matches
`spiffe://zeroship.ai/svc/worker`, which is how the worker's identity claims are
spelled. A sweep for the bare prefix reports the worker touching the platform
schema when it does not. Anchoring on the SQL keyword is what makes the answer
mean what it says.

**`src/` is not production.** `#[cfg(test)]` modules live in `src/` and the
sweep cannot see the gate. `crates/zeroship-data-cdc-server/src/source.rs`
inserts `zeroship.databases` rows with `status = 'active'` inside such a module,
against stand-in tables it creates itself. Read the gate before concluding a
service writes a table; the difference decides whether step 2 is one read or
three tables.

The same caution applies to a comment that states an invariant.
`crates/zeroship-control/src/databases.rs` carries a header asserting that
nothing in the file reaches `status = 'active'`. It is true - the writers bind
`STATUS_PROVISIONING`, `STATUS_DELETING` and `BINDING_STATUS_PENDING`, and
`STATUS_ACTIVE` appears only inside a read predicate - but it would read
identically if it were false. Check the writers.

## What transfers from the app-database decoupling

`docs/proposals/2026-08-28-app-database-decoupling.md` established mechanisms
this split should reuse rather than reinvent:

- **Declare and converge** instead of a cross-database transaction:
  `generation` / `observed_generation`, one side declaring, the other observing.
- **Composite keys carrying a co-location discriminator**, so "same project" is
  true by construction rather than by trigger.
- **One shared predicate constant** rather than each service re-spelling the
  same question - `crates/zeroship-core/src/live_binding.rs` exists because
  three services must answer one question identically.
- **Registration by self-identity**: a datastore keys on
  `pg_control_system().system_identifier` rather than an operator-chosen name.
- **A per-service posture check at boot**, which is what keeps a split honest
  and already exists for one service.
- **Subset tests, never equality.** Equality coupled every app on a database to
  every other; the same trap waits for any cross-service version check.

**What does not transfer:** that split had a clean fault line, because creator
data holds no foreign key into the platform schema. This one has a narrower one
that has to be found rather than assumed.

Counting foreign keys that REFERENCE `users` and `apps` overstates it badly -
that population includes keys whose child table never leaves the referenced
table's own service, and they cannot cross a boundary they never reach. The
question is how many point at `users` FROM A TABLE ANOTHER SERVICE OWNS, and on
the `users` side that is at most five: `app_session_anchors`,
`app_user_identities`, `oauth_grants`, `organization_members`, `identity_links`.
Most of the cascades never leave auth.

Five named tables is a design problem with a shape. The FK count was a wall,
and it was the wrong measure.

---

## Open

1. **ANSWERED: the authorization ladder join is per-decision and uncached, and
   it crosses on `users` alone.** It requires a projection in control carrying
   `id`, `email_verified_at` and `locked_until`, not an API call - see claim 2.
   **Still open, and the only unresolved blocker in this document: can that
   projection be made synchronous with the lock write?** If it cannot, the
   authorization gap is a property of the cut rather than of its timing, and
   step 6 of the sequencing states the two mechanisms that could close it.
2. **One audit store or one per service?** `audit_events` has three writers and
   no reader.
3. **Does `service_assertion_replay` stay shared?** The argument for sharing is
   weaker than its header claims.
4. **Which database holds an auth-domain row written only by control?**
   `identity_links` is the case; both answers cost something.
5. **`cron_state` and `dpop_jti` have no reader or writer in any crate.** Drop
   them before assigning them.
