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
`crates/zeroship-authz/src/authority.rs` joins `users` to
`organization_members` and `organization_roles` in one statement, and
`zeroship-authz` is linked into auth, gateway, control and migrate-server.

*Unverified:* whether this runs per decision or is cached. That measurement
decides whether the split needs a `users` projection in control's database or
merely an API call.

**3. Auth takes a row lock on control's `organizations` while deleting `users`.**
`refuse_if_it_strands_an_organization` holds `SELECT ... FOR UPDATE` on a
control table inside the transaction that deletes an auth row, and the argument
for its correctness rests on PostgreSQL's own `FOR KEY SHARE` referential
locking. Neither survives a split.

**4. Three data-modifying CTEs write across service boundaries in one
statement.** `password_reset::complete`,
`revoke_user_app_credentials_in_transaction`, and
`platform_cli::materialize_default_grants`. A CTE cannot become a saga without
changing its semantics.

---

## Sequencing

**1. Worker.** Already free: no platform SQL, and a boot gate that refuses
platform-schema reach. The change is a separate database, a role with no grant
on it, and extending `db_posture.rs` to check database identity rather than
schema reachability.

**2. CDC relay.** One narrow read - `worker_instances.public_key` - and an
in-memory replay store already. It becomes a control API call.

**3. Gateway.** No production read-joins; every statement is single-table. Its
cost is the session and anchor tables, whose ownership this document assigns to
it.

**4. Workflow.** Already a separate schema with its own migrator role and a boot
check that enumerates its tables. Its cross-service reads are column-scoped
projections of control tables.

**5. migrate-server.** Needs a role of its own first - it currently logs in with
control's credential, and there is no `zeroship_migrate` role in the corpus.

**6. auth ∥ control.** Last, and the only one that requires solving erasure, the
authorization join and the cross-service CTEs. Nothing above it is blocked by
it.

---

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

**What does not transfer:** that split had a natural fault line, because creator
data holds no foreign key into the platform schema. This one has no equivalent.

---

## Open

1. **Is the authorization ladder join per-decision or cached?** Decides whether
   control needs a `users` projection. Unmeasured.
2. **One audit store or one per service?** `audit_events` has three writers and
   no reader.
3. **Does `service_assertion_replay` stay shared?** The argument for sharing is
   weaker than its header claims.
4. **Which database holds an auth-domain row written only by control?**
   `identity_links` is the case; both answers cost something.
5. **`cron_state` and `dpop_jti` have no reader or writer in any crate.** Drop
   them before assigning them.
