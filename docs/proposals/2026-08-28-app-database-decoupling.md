# Decoupling app identity from database identity

**Status.** PARTLY BUILT, on `feat/app-database-decoupling`. What exists is the identity, the
entities, the control-plane surface that declares them and the cluster reconciler that makes a
cluster match. The data path has not moved: the worker still composes a per-app role name, so
no app reaches a second database and no fence is enforced at runtime yet.

Built:

- the typed ids `dst_`, `dbs_` and `bnd_` (`crates/zeroship-id/`), with prefix disjointness bound
  by a test rather than asserted in a comment
- the physical-name derivations: the composers and their truncation refusal in
  `crates/zeroship-core/src/database_role.rs`, and the typed seam over them in
  `crates/zeroship-core/src/database_derivation.rs`. A name PostgreSQL would have truncated is
  refused rather than shortened, because truncation drops the epoch digits of
  `zs_bind_<binding>_e<epoch>` first and lands two epochs of one binding on one role
- `Resource::Database` and its policy bands (`crates/zeroship-authz/`,
  `deploy/policies/zeroship.cedarschema`)
- the three tables and the project's zone
  (`db/migrations-ts/20260919000100_project_execution_zone.ts`,
  `db/migrations-ts/20260919000200_database_entities.ts`,
  `db/migrations-ts/20260919000300_database_placement_keys.ts`)
- the tenant fence measurement target
  (`crates/zeroship-data-orm/tests/postgres_tenant_fence.rs`) - see Open 5
- the control-plane management surface (`crates/zeroship-control/src/databases.rs`): the
  placement query below, create and delete a database, bind and unbind an app, and the two
  listings. Every mutation carries the project-seat rank predicate inside its effect statement
  and authorizes at `Resource::Database` or at the project, so this is the first surface that
  constructs that Cedar resource. Control writes no `datastores` row: placement READS that
  table, and a cluster registers itself through the service holding its credential.
- the cluster reconciler (`crates/zeroship-migrate-server/src/datastore/`), one loop per
  datastore inside the migration service: a cluster self-registers on its own
  `pg_control_system()` identity, the bootstrap corpus creates the platform logins and the
  admin schema, each declared database becomes a schema and its three roles, each declared
  binding becomes the two grant edges, and a binding role no declaration names is reaped. A
  cluster below PostgreSQL 16 is refused by version. `crates/zeroship-migrate-server/tests/
  datastore_reconciler_pg.rs` drives it against a live control plane and a live tenant cluster,
  including the fence end to end through a real worker login.

Not built, and this is the whole data path: `zeroship_core::app_derivation::schema_name`
(`crates/zeroship-core/src/app_derivation.rs`) still returns the app id itself; `DbBinding`
(`crates/zeroship-data-orm/src/binding.rs`) still carries `{ app_id, deploy_token, schema }`;
the worker still composes the per-app role name, and no apply advances an epoch.

**Control still never writes `active` itself.** The management surface declares and stops: a
database it creates stops at `provisioning` and a binding at `pending` until a reconciler holding
that cluster's credential has made the cluster match. That ceiling is asserted from the stored
rows in `crates/zeroship-control/tests/database_surface_test.rs`, not left to inspection.

Four things this design depends on HAVE landed and are relied on below: the organization and
project ladder with its composite ownership keys
(`db/migrations-ts/20260906000000_organization_entity_model.ts`,
`db/migrations-ts/20260906000200_apps_project_ownership_key.ts`), execution zones with the
app's zone frozen by trigger
(`db/migrations-ts/20260914000400_execution_zones_and_join_signers.ts`,
`db/migrations-ts/20260914000600_placement_eligibility.ts`), the project-scoped encryption
root key (`db/migrations-ts/20260911000100_project_data_keys.ts`), and the edge split routing
`/v1/*` to the migration service (`deploy/ops/Caddyfile`).

The header of the organization migration names this work and its parent:

> DELIBERATELY NOT HERE: ... the project-level `sector_identifier`, and project-owned data
> resources with their capability bindings. NO COMMITTED DOCUMENT DESIGNS ANY OF THE THREE.

This is that document for the third.

---

## What it is

An app id is a tenant. It is not a schema name, not a role name, not an encryption salt and
not a publication key. Today it is all five by string identity, which is what makes a database
that outlives its app, or one that two apps share, or an app that reaches two databases,
unrepresentable.

### The ladder

```
org_  Organization   ownership root, billing subject                  EXISTS
prj_  Project        shared-infrastructure boundary, audience unit,
                     holder of the encryption root key                EXISTS
app_  App            deploy target, runs creator code, zone frozen    EXISTS
ezn_  ExecutionZone  operator-declared set of worker deployment
                     units that share creator-side connectivity       EXISTS

dst_  Datastore      ONE PostgreSQL cluster and the one PG database
                     the platform uses inside it. Operator-owned.
                     Creators never name one and never see one.       NEW
dbs_  Database       one schema inside a Datastore. Project-owned.
                     The unit that is migrated, bound and dropped.    NEW
bnd_  Binding        (app, database, capability). The N:M edge.       NEW
```

```
        organizations
              |
              | 1:N
        projects -------------------+
          |      \                  |
          | 1:N   \ 1:1             | 1:N
          |        project_data_keys|
        apps                      databases
          |                           |  N:1
          | 1:N                       v
          +----> database_bindings <--+   datastores --N:1--> execution_zones
                  UNIQUE (app_id, database_id)
```

**Many apps may bind one database, and one app may bind many databases.** Both directions are
open. What confines them is not cardinality but one structural predicate: a binding may only join
an app and a database **in the same project**. Because a project is pinned to one execution zone,
that predicate carries co-location with it rather than needing a second one.

`dbs`, `dst` and `bnd` are all free against the prefix set in
`crates/zeroship-id/src/typed_id.rs`. The data-access edge is called a **binding**, not a
grant: `GRANT_PREFIX` there is `grt` and means one row per (person, audience) in
`zeroship.grants`, which `db/migrations-ts/20260907000100_session_object.ts` creates. Reusing
either spelling would put two unrelated entities in one namespace.

**A database is addressed by its id, always.** There is no `(project, name) -> database_id`
resolution anywhere on the wire. `databases.name` is display text for dashboards and the CLI's
local dereference; nothing sends it to a server as an identifier. An id is not a capability:
authorization is evaluated on the database itself.

**A DSN never reaches the control plane at all.** A cluster's credential stays in the config of
the service that holds it, and control records only the cluster's own identity, its zone and its
status. There is no secret reference on any row here, because there is nothing for one to point at.

---

## The tables

Three new tables. The control plane gains no record of what migrations ran; see below.

### `zeroship.datastores`

```
id                 text   PK   ^dst_[0-9a-z]{25}$   COLLATE "C"
system_identifier  bigint NOT NULL  the CLUSTER'S own identity, not a chosen name
execution_zone_id  text   NOT NULL  -> execution_zones(id) RESTRICT
status             text   NOT NULL  pending | active | draining | retired | failed
last_error         text
created_at, updated_at

UNIQUE (system_identifier)                              <- the natural key
UNIQUE (id, execution_zone_id)                          <- FK target
CHECK  status in (...)
```

`system_identifier` is `SELECT system_identifier FROM pg_control_system()`, which is stable for
the life of a cluster and identical from every database in it. Keying on it rather than on an
operator-chosen name is what makes registration idempotent: two services configured against the
same cluster converge on one row instead of creating two, and a mistyped DSN either fails to
connect or reaches a different cluster, where it becomes a visibly new row rather than a silent
duplicate.

There is no `name` and no `admin_secret_ref`. The credential lives in the config of the service
that holds it and never reaches control; a display name can be added when something needs to
render one.

`status` carries the convergence signal as well as the operator's intent: the cluster
reconciler flips `pending -> active` when the bootstrap corpus has applied. Placement admits
`active` only, so a cluster that is unreachable or half-bootstrapped is never chosen. The
`'%://%'` check catches the one mistake a human will actually make, which is pasting a live
DSN where a secret name belongs; `file:/...` and `urn:zeroship:file:/...` pass and
`postgres://user:pass@host` does not.

No capacity columns. A hand-maintained number on an entity row goes stale silently, and
placement does not need one.

### `zeroship.databases`

```
id                 text  PK   ^dbs_[0-9a-z]{25}$   COLLATE "C"
project_id         text  NOT NULL  -> projects(id) RESTRICT
execution_zone_id  text  NOT NULL  denormalized, kept honest below
datastore_id       text  NOT NULL
name               text  NOT NULL  display only
schema_epoch       int   NOT NULL  a role-name input, NOT a schema record
status             text  NOT NULL  provisioning | active | draining | deleting
created_at, updated_at

UNIQUE (project_id, name)
UNIQUE (id, project_id)                                 <- FK target
FOREIGN KEY (project_id,   execution_zone_id)
         -> projects(id, execution_zone_id) RESTRICT
FOREIGN KEY (datastore_id, execution_zone_id)
         -> datastores(id, execution_zone_id) RESTRICT
```

Those two foreign keys are why the denormalized zone is safe, and between them they say the whole
placement rule: the first makes the database's zone its **project's** zone, the second makes it
its **cluster's** zone. So a database is placed on a cluster in its project's zone, structurally,
with no trigger and nothing to forget to run.

`schema_epoch` answers exactly one question - which `zs_bind_<bnd>_e<E>` should control compose
- and it is a counter, not a description of a shape. Its authority is the epoch row in
`__zeroship_admin` on the cluster, written inside the transaction that mints the epoch's roles,
because that is the only place it moves atomically with the DDL. Control keeps a copy solely to
compose a binding without a cross-zone read on the deploy path. A stale copy composes a role
name that does not exist, `SET LOCAL ROLE` fails, and the caller re-resolves: fail-closed and
self-correcting. The column comment must say all of this, or someone will read it as a record
of the schema and rebuild the deploy gate on top of it.

The physical schema name `db_<id>` is derived, never stored, and it lives in
`crates/zeroship-core/src/database_derivation.rs`, a sibling of
`crates/zeroship-core/src/app_derivation.rs` rather than an addition to it. Both exist for the
same reason - so the data plane and the migration service cannot answer the question differently
- but they are kept apart because the app-keyed derivations do not all retire: the lifecycle lock
seed stays app-keyed, and so does the workflow journal until
`docs/proposals/2026-09-19-workflow-journal-relocation.md` moves it. One module holding both would
invite a future reader to assume every derivation in it moved.

The text layer beneath them is shared: `crates/zeroship-core/src/database_role.rs` composes every
role name and refuses one it would have truncated, so the 63-byte ceiling is enforced at a single
site rather than once per caller.

There is no `engine` column. The hosted tier is PostgreSQL only per the AGENTS.md invariant,
and SQLite is the dev tier, which has no control plane at all.

### `zeroship.database_bindings`

```
id                   text PK  ^bnd_[0-9a-z]{25}$  COLLATE "C"
app_id               text NOT NULL
database_id          text NOT NULL
project_id           text NOT NULL
capability           text NOT NULL  readwrite | readonly
status               text NOT NULL  pending | active | revoking | revoked
generation           int  NOT NULL  desired state version
observed_generation  int  NOT NULL  what the reconciler converged
last_error           text
created_at, updated_at

UNIQUE (app_id, database_id)
CHECK  observed_generation <= generation

FOREIGN KEY (app_id,      project_id) -> apps(id, project_id)
FOREIGN KEY (database_id, project_id) -> databases(id, project_id)
```

**The two foreign keys are the design.** They agree on `project_id`, so the sharing predicate
holds on every write to either side rather than at issuance only. This is the mechanism
`apps_project_ownership_fkey` already uses for app ownership.

**The binding carries no zone, because the project does.** Since a project sits in exactly one
execution zone, "same project" already implies "same zone" and a zone pair here would be a
second copy of a fact the project holds. Co-location is not an extra predicate the binding
enforces; it falls out of ownership.

The row carries its own id because the PostgreSQL role name is derived from it,
`zs_bind_<bnd>_e<E>`; a composite natural key would put two ids in one identifier. It carries
no label: the creator's local name for a database rides in the manifest, so the control plane
never treats a creator-chosen name as an identifier and two apps may call one database
different things. It carries no epoch: which role names exist for a binding is answerable from
the cluster catalog, which is authoritative, and a stored copy could only be wrong.

`generation` / `observed_generation` are here and not on `datastores`. A binding's convergence
is per-app and its failure is routine - a cluster briefly unreachable during a grant - so "the
row says active but the roles are not there yet" is a state this will actually reach. A
datastore's convergence is a one-time bootstrap and `status` carries it.

### The control plane records no migration at all

`zeroship.app_schema_applies` is **deleted and not replaced**, and
`crates/zeroship-migrate-server/src/schema_apply_store.rs` goes with it. It exists today as the
platform's own record of what schema an app corresponds to, because the engine journal lives in
the creator's schema where the migrator role can drop it, and it feeds the deploy gate's
descriptor comparison. Both halves fall under this design.

**The descriptor comparison is an equality test, and equality is a coupling mechanism.** An
app's build hashes the schema it was generated against. With one app per database that hash
answered a real question. With several, any migration - a purely additive one included -
invalidates the build of every app bound to that database, so all of them stop being deployable
while all of them keep running correctly. Forcing every app on a database to move together is
precisely the property this design exists to remove. It is not a transient window either: a
database owned by a project and apps built on their own cadence do not move together, and a
database that outlives its apps never moves together with any of them.

**The remaining columns have no production reader.** The control-side model
(`crates/zeroship-control/src/publication/models.rs`) declares only `id`, `app_id`, `status`,
`descriptor_sha256`, `submitted_at` and `applied_at`, under a comment saying it declares "only
the columns these operations read or write". `migration_id`, `request_body`,
`effective_profile`, `ceiling_id`, `ceiling_version`, `applied_versions`, `submitted_by` and
`last_error` are written and read by nothing - the same one-writer-zero-readers test
`docs/proposals/2026-08-28-migration-record-consolidation.md` used to delete
`zeroship.migrated_migration_audit`.

So the engine journal in the creator's own schema becomes the only record of what ran, which is
what that consolidation set out to achieve and stopped one table short of. Per-apply audit, if
it is wanted, is an audit event in the audit system rather than a bespoke ledger.

### Changes to existing tables

```
projects  + execution_zone_id, NOT NULL, frozen by trigger
          + UNIQUE ("projects_zone_identity_key")  (id, execution_zone_id)

apps      + UNIQUE ("apps_project_identity_key")   (id, project_id)
          + FOREIGN KEY (project_id, execution_zone_id)
                     -> projects(id, execution_zone_id)

zeroship.app_schema_applies   DROPPED, no successor
```

**The zone moves to the project.** `apps.execution_zone_id`
(`db/migrations-ts/20260914000600_placement_eligibility.ts`) landed before anything above the app
needed a zone; with a project-owned database it is the project that has to carry it, or "same
project" stops implying "can share" and every sharing surface has to explain a second rule.
`projects` therefore gains the column and the freeze trigger `apps` already has, and the app keeps
its copy under a composite foreign key so the two cannot disagree.

Keeping the app's copy rather than deriving it is deliberate: `instance_serves_app`
(`crates/zeroship-control/src/worker_join.rs`) joins on `app.execution_zone_id`, and
`db/migrations-ts/20260914000600_placement_eligibility.ts` grants the workflow manager
`SELECT (execution_zone_id, deleted_at)` on `apps`. Both keep working untouched, and the composite
foreign key is what stops the copy drifting.

`apps_project_identity_key` is new too. `apps` today carries only `apps_name_key`, and its
composite ownership key points outward at `projects(id, organization_id)` with nothing pointing
in. PostgreSQL requires a real unique constraint on referenced columns, so without it the binding
foreign key will not create.

`CreateAppBody.execution_zone` (`crates/zeroship-control/src/api.rs`) and the zone resolution in
`Registry::create_app` move to project creation with it.

`crates/zeroship-authz/src/resource.rs` gains `Resource::Database { id: DatabaseId }`, typed
rather than `String` for the reason that file already gives for `AppId`: the decode is the
parse, so a wrapper policy naming a database in any other spelling is a decode failure rather
than a row that matches nothing. `Resource::validate_ids` is an exhaustive match, so the new
variant forces an arm.

### Companion registrations, none of them optional

Every new table needs an entry in `policies/platform-table-owners.json`; the applier refuses
fail-closed on any op targeting a table with no ownership entry, so a table created without one
halts the whole corpus. Every typed-id text column needs `COLLATE "C"` inline in its creating
migration, as `project_data_keys` and `execution_zones` already do; the collation map in
`db/migrations-ts/20260831000001_sortable_entity_id_collations.ts` is a one-time backfill for
tables that predate it, not a registry new tables join.

---

## Where a fact lives

This platform is self-hostable, so every fact has to be filed by who owns it. Three mechanisms
already exist in the tree and each is correct for one kind:

| kind | mechanism | exemplar |
| --- | --- | --- |
| **Product**, invariant across deployments | source, parameterized where it varies | `deploy/ops/Caddyfile` written `<label>.{$ZEROSHIP_DOMAIN}`, per `crates/zeroship-control/src/reserved_names.rs` |
| **Product default**, a starting value an operator may extend | boot-time idempotent seed into an operator-editable table | `plan_catalog::seed_plans` (`crates/zeroship-control/src/plan_catalog.rs`), called once from control's main |
| **Deployment**, true of this installation only | an operator-provided file outside the tree, named by a config key | `control.join_signers_file` (`crates/zeroship-control/src/config.rs`) |

A migration can carry the first two. It cannot carry the third, because **a migration is not
overridable**: the corpus is ordered, journalled and verified, so a self-hoster declaring their
own clusters would have to fork it and merge against it forever, while upstream carried a
stranger's hardware inventory as product.

Which zones and which clusters exist is deployment data. The schema of the three new tables,
and a seed of the one default zone, are product.

---

## Deployment

A Datastore is one cluster, and the platform uses one PG database inside it. Multiple PG
databases per cluster are rejected; the reasoning is under Why it is this way.

**A cluster registers itself.** There is no topology document and no new config key. The service
holding a cluster's privileged credential presents it to control, which records what it can verify
rather than what it was told - the shape worker join already has:

```
  migrate-server boots with provision_database_url
      connects, reads pg_control_system().system_identifier
      registers with control: identity + its own zone, status = pending
      runs the datastore bootstrap corpus
      status = active
```

Adding a cluster is therefore provisioning it and configuring a service to reach it. Nothing is
declared twice, and the deployment data stays where deployment data already lives: in a service's
config, not in a document control has to parse and not in the migration corpus.

**Registration is idempotent by construction**, because the key is the cluster's identity rather
than a name. Two services configured against one cluster converge; a credential that reaches
nothing registers nothing; a credential that reaches the wrong cluster registers a row an operator
can see. None of that needs import semantics, a refusal policy or a runbook.

**Control never learns a DSN.** It records identity, zone and status. The credential stays in the
config of the service that holds it, which is also what keeps the blast radius of the control
database's compromise away from every tenant cluster.

**So every service resolves addresses itself, the same way.** A binding names `dst_X`; control
cannot say where that is, because it does not know. Each service therefore connects to every
cluster its own config lists, reads `system_identifier`, and builds the map from identity to
connection. Nothing has to be told which address is which cluster, because the cluster answers
for itself - the same property that makes registration idempotent makes address resolution need
no coordination.

**Which puts two config keys into the plural and deliberately leaves one alone:**

```
  migrate-server   provision_database_url  -> a LIST
                   one process serves every cluster in its zone
  worker           database_url            -> a LIST
                   one process serves every cluster in its zone
  cdc relay        database_url            -> stays SINGULAR
                   one relay process per cluster; scale by running more,
                   not by teaching one process about many
```

The lists carry DSNs and nothing else. No names, no ids, no ordering: identity is read from the
cluster, so config that tried to name one would be a second spelling able to disagree.

**Adding a cluster is therefore config in two places plus one process**, not one place. Registration
buys control a placement registry; it does not buy the worker its addresses, and it was never going
to while control holds no DSN.

**One manual step survives, and it is a deliberate trade.** A fresh cluster has only its own
superuser, so someone creates the provisioning login before migrate-server can connect and register
anything. That could be collapsed - let the bootstrap corpus connect as the cluster's superuser on
first contact and create its own least-privilege role, making the whole procedure "add a DSN,
restart" - at the cost of a superuser DSN sitting in a service config, even temporarily. This
design keeps the manual step. Reversing that is a decision, not a refactor.

**Control owns lifecycle state.** `status` is the operator's only lever and it is control-side, so
taking a cluster out of rotation during an incident is a row update rather than a deploy or a
config push across replicas.

**Zones do not follow, and that is deliberate.** A datastore can register itself because reaching
a cluster proves it exists. A zone proves nothing: it gates which join signers may mint workers,
and control today "refuses a file naming a zone this deployment does not declare". Letting a
service invent one by presenting itself would undo that.

So `execution_zones` stays declared rather than registered. The default seed is fine where it is -
a product default for the single-host case, in the `data()` phase the corpus supports for exactly
that, with `zeroship_workflow_manager::eligibility::ZoneId::default_zone`
(`crates/zeroship-workflow-manager/src/eligibility.rs`) already spelling its id once. Where zones
BEYOND the default are declared is Open 12: a three-zone self-host still cannot say so without
patching the corpus, and the cheapest answer is probably the existing join-signers file, which
already lists the zones each signer may mint for.

**Bootstrapping a cluster is a migration.** `migrate-server` gains a second corpus - the
datastore bootstrap corpus - creating the worker login, the CDC login and the extensions.
Creating roles is DDL, so this is the job that service already does, done with machinery that is
journalled, idempotent and re-runnable, rather than a hand-run shell script. The corpus is
product; which clusters it is applied to is deployment.

**The bootstrap refuses a cluster below PostgreSQL 16, by version.** Below 16 `pg_auth_members`
carries no `inherit_option` or `set_option`, so `WITH INHERIT FALSE` and `WITH SET FALSE` are not
a weaker fence, they are no fence. A pre-16 cluster would fail anyway when role creation hit the
syntax, which is fail-closed but reads as a confusing SQL error while an operator is adding
capacity. Refusing by version says why.

**The fleet is a set of majors, not one.** Clusters are declared by an operator and upgraded one
at a time, so a mixed fleet is the normal state rather than a hazard. Anything version-dependent
is therefore a per-datastore fact, not a global one - `transaction_timeout` being the live example,
absent on 16 and present from 17.

**Creator storage is not the control database**, and the worker already refuses to boot
otherwise: `validate` in `crates/zeroship-worker/src/db_posture.rs` fails on a login that can so
much as resolve the `zeroship` schema, because "the worker connects to the creator database and
reads app metadata from Control". Splitting creator schemas out of the control plane's own
PostgreSQL is a prerequisite for the Datastore entity rather than a consequence of it.

**Relocating a database between clusters is not supported.** Placement is therefore a one-way
door; see Why it is this way for what that gives up and what replaces it.

---

## Placement

An execution zone is, in the words of its own migration, "an operator-declared set of worker
deployment units that share creator-side connectivity". It is already a secrets boundary and
not a latency hint: `zone_scoped_app_read` in `crates/zeroship-control/src/internal.rs` narrows
the three host reads - the app, its environment, and its project data key - to the calling
worker instance's zone, and the code states why:

> an app's environment is its decrypted secrets and its project data key is a decryption
> capability, so reaching either from another zone is exactly what zones exist to prevent.

A datastore belongs to exactly one zone. Extending the fence to storage makes enforceable an
assertion the tree already makes twice but cannot check: that moving an app between zones "is a
data migration of its creator storage, not a metadata edit"
(`db/migrations-ts/20260914000600_placement_eligibility.ts`).

**Co-location is required, not preferred.** With one database per app, availability was one
cluster's availability. With several, it is the product, and across zones it is the product plus
a wide-area round trip on every statement of the remote one.

Pinning the zone on the **project** closes this without the binding carrying anything: a project
is in one zone, its apps and its databases inherit that zone under composite foreign keys, so an
app can only ever bind a database in its own zone. A multi-region product becomes two projects
under one organization, which is also how data residency usually wants to be drawn - a hard
boundary rather than a soft one.

**Choosing a cluster uses facts control already owns**, because control created every database
and knows exactly how many sit on each:

```sql
SELECT d.id
  FROM zeroship.datastores d
 WHERE d.execution_zone_id = $1
   AND d.status = 'active'
 ORDER BY (SELECT count(*) FROM zeroship.databases db WHERE db.datastore_id = d.id),
          d.id
 LIMIT 1;
```

One query in the control database, runnable inside the same transaction that admits the
placement - the discipline the workflow manager already follows for zone facts, reading them
after taking its locks and again before commit. An empty result is a typed refusal at create
time; a zone with no capacity must fail loudly rather than overload a cluster.

The operator's lever is `status`. A cluster that is filling gets flipped to `draining` and
placement stops choosing it. Disk and connection alerting stay in ordinary observability with a
human deciding when to flip the flag. Capacity-aware placement is deferred deliberately and
named in Open.

---

## Managing the three resources

Three resources, three owners, three management models.

```
  Datastore   OPERATOR       registers itself when a service reaches it. No creator surface.
  Database    PROJECT        created and deleted by a creator with a project seat.
  Binding     PROJECT        the edge. Both endpoints must already be in that project.
```

### Authority

| | create | change | delete | authorized by |
| --- | --- | --- | --- | --- |
| Datastore | self-registration, keyed on the cluster's `system_identifier` | `status`, control-side | when it holds no database | operator, no API |
| Database | creator | schema via migrate-server; `name` via control | when it has no binding | `Resource::Database`, project seat |
| Binding | creator | capability is a role rotation | revoke | both endpoints already in one project and zone |

### Creating a database is two facts in two places

```
  creator: zeroship db create --project prj_... --name main
        |
        v
  CONTROL     authorize: does the caller hold a qualifying seat on the project
              place:     the query above, inside this transaction
              insert:    databases row, status = provisioning
        |
        v
  CLUSTER RECONCILER     CREATE SCHEMA db_<dbs>
                         CREATE ROLE zs_db_<dbs>_{mig,rw,ro}
                         column grants
        |
        v
  status = active
```

Schema *content* is then managed entirely by migrate-server, addressed by database id and
authorized by the creator's own bearer.

### Deploy verifies bindings; it never creates them, and it never compares schemas

The tempting design is for the manifest to declare which databases an app uses and for deploy to
reconcile bindings to match. **That is wrong**, and it is worth stating rather than leaving to
inference: it would mean a revoked binding is silently restored by the next deploy, because the
config file is a stale snapshot of an intent someone has since changed.

```
  GRANT is an explicit act     zeroship db bind dbs_... --app app_... --readwrite
  DEPLOY only verifies         "app X declares database main (dbs_Y) but holds no active
                               binding to it" -> refuse, naming the command that fixes it
```

Deploy checks exactly two things, and neither needs to know what schema any database is at:
every database the manifest declares has an **active binding** for this app, and every database
the manifest declares has a **descriptor in this bundle**. The second is a self-consistency
check on the artifact, and it preserves the one refusal worth keeping from today's gate - an app
that declares a database but carries no schema for it would boot with `env.db` uninstalled over
live data.

Capability change is not an UPDATE in the usual sense. `readwrite -> readonly` is a role
rotation: mint the new binding role inheriting the other database role, drop the old. New
transactions see it immediately; an in-flight transaction that already assumed the old role runs
to its end.

### The creator surface

```
  zeroship db create --project prj_... --name main     -> prints dbs_...
  zeroship db list
  zeroship db bind   dbs_... --app app_... --readwrite
  zeroship db unbind dbs_... --app app_...
  zeroship db bindings dbs_...                          -> which apps, which capability
  zeroship db delete dbs_...                            -> refuses while bound

  zeroship migrate --database main                      -> migrate-server, by id
  zeroship deploy                                       -> verifies bindings, not schemas
```

`main` is the local label from `zeroship.jsonc`; the CLI dereferences it to a `dbs_` before any
request, so a label never travels as an identifier.

The routes those commands call are mounted by `zeroship_control::databases::configure`
(`crates/zeroship-control/src/databases.rs`):

```
  POST   /api/projects/{project_id}/databases            create, placed here
  GET    /api/projects/{project_id}/databases            list
  POST   /api/databases/{database_id}/bindings           bind, capability in the body
  GET    /api/databases/{database_id}/bindings           list, with each capability
  DELETE /api/databases/{database_id}/bindings/{app_id}  unbind
  DELETE /api/databases/{database_id}                    delete, refuses while bound
```

The first two name the PROJECT, because the database either does not exist yet or is not
singled out; the rest name the database, and they are what make `Resource::Database` a resource
Cedar is asked about.

---

## The cluster reconciler

Datastore bootstrap, database provisioning and binding grants are all the same problem - control
declares, a cluster must be made to match - and all need the same privileged connection to the
same cluster. They are **one loop per datastore**, not three:

```
  for each datastore in this zone:
      converge the bootstrap corpus                 pending -> active
      converge every database declared on it        schema, db roles, column grants
      converge every binding to those databases     the two role edges
      record status / observed_generation
```

It runs in `migrate-server`, which already holds the privileged DSN per cluster
(`migrate_server.provision_database_url`, `crates/zeroship-migrate-server/src/config.rs`) and
already never executes creator code.

**Granting is not one transaction, and never was.** Control's database and the tenant cluster
are different servers, so nothing spans the control row and the role DDL. Any design claiming
that transaction is claiming a distributed transaction it does not have.

Nothing downstream trusts a half-converged row: placement admits `status = 'active'` only, and a
deploy requires a binding whose `observed_generation` has caught up to its `generation`.

---

## Enforcement: role membership, not an in-process check

```
zs_db_<dbs>_mig     owns schema db_<dbs>
zs_db_<dbs>_rw      USAGE on db_<dbs> + column-listed DML
zs_db_<dbs>_ro      USAGE on db_<dbs> + column-listed SELECT
zs_bind_<bnd>_e<E>  NOLOGIN, no privileges of its own; inherits exactly ONE database role
zeroship_worker     LOGIN, member of zs_bind_<bnd>_e<E>, per live (binding, epoch)
```

```sql
CREATE ROLE zs_bind_<bnd>_e<E> NOLOGIN;
GRANT zs_db_<dbs>_<cap> TO zs_bind_<bnd>_e<E> WITH SET FALSE;    -- inherits, not assumable
GRANT zs_bind_<bnd>_e<E> TO zeroship_worker   WITH INHERIT FALSE; -- assumable, never ambient
```

`<E>` is the schema epoch. The data plane narrows per transaction with
`SET LOCAL ROLE "zs_bind_<bnd>_e<E>"` as the first statement of the setup batch that already
exists (`crates/zeroship-data-orm/src/backend/postgres/pg_session_sql.rs`): same statement, same
batch position, no extra round trip. It replaces the per-app role name composed by
`crates/zeroship-core/src/database_role.rs` in that builder and nowhere else.

**The role must be per binding, not per database.** `SET ROLE` authorizes against the transitive
closure of the memberships held by the role that *connected*, which here is always the shared
worker login and never the app. Revoking one app's edge would therefore have no database
consequence while any parallel edge survived. A binding role inherits exactly one database role,
so per-statement confinement stays one database while revocation still bites.

**Boot posture becomes per datastore and stops being fatal.**
`db_posture::validate_database_url` runs once on one DSN at startup today
(`crates/zeroship-worker/src/main.rs`). Under many clusters, making boot success the product of
every cluster's health means a worker in a busy zone never starts. Posture runs per datastore, at
first use and periodically after, and a datastore that fails it renders its databases unavailable
under a typed refusal while every other datastore keeps serving. It gains one catalog-checkable
arm - `pg_has_role(login, <database role>, 'SET') = false` for every database role - and records
each datastore's `server_version`, so a mixed fleet is an observable fact rather than something
discovered during an incident.

**The fence partitions per cluster, which makes it narrower rather than weaker.** Roles live in
one cluster's catalog, so each cluster has its own `zeroship_worker` and its own memberships. If
the posture fails on one, the blast radius is that cluster's databases. A single-cluster design had
no such containment, and the instinct on reading "one worker, many clusters" is the opposite of
what is true.

**There are no ambient exceptions, and the posture is already stricter than this design needs.**
`validate` in `crates/zeroship-worker/src/db_posture.rs` refuses boot on five arms: a login that
is not `zeroship_worker`; `SUPERUSER`, `CREATEROLE` or `CREATEDB`; `REPLICATION` or `BYPASSRLS`
("CDC belongs to the relay"); a login that can resolve the `zeroship` schema at all, which is the
zone arm; and **any** inheriting membership, deny-by-default with no name exempted, on the stated
ground that "a future role that genuinely needs one fails boot loudly". So the claim is stronger
than a per-app design could make: the worker holds no ambient authority of any kind, and the
replication plane is fenced by living in a different process under a different login rather than
by roles.

---

## Binding resolution

The control plane resolves the app's bindings at deploy time and hands the worker a set:

```
DbBinding { db: "dbs_...", schema: "db_...", ds: <datastore handle>,
            cap: "readwrite", epoch: <E> }
```

The vehicle is `DbBinding` (`crates/zeroship-data-orm/src/binding.rs`), which already carries
identity to the plugin without passing through V8 and is already the per-isolate value every
`Db` and `Collection` wrapper travels with. It gains the database id, the datastore handle, the
capability and the epoch, and loses nothing.

The worker does not compare `epoch` in Rust to authorize a transaction; it composes the role
name the setup batch sends. An app whose binding is absent is a hard refusal with the same shape
as `collection_not_declared` (`crates/zeroship-data-orm/src/descriptor.rs`).

**Per-thread resources become maps keyed by datastore.** `ThreadDbContext`
(`crates/zeroship-data-v8/src/context.rs`) holds one `Option<LocalConnection>` and
`install_connection` replaces it whenever the factory identity differs, so registering a second
target tears the first down. It becomes a map keyed by the datastore, with pools created lazily
on first use and evicted when idle - the shape the isolate cache already has. A worker thread
cannot hold a connection floor to every cluster in its zone.

The connection identity that keys that map is `ConnectionFactory::identity()`
(`crates/zeroship-data-orm/src/connection/factory.rs`), whose `url` is a separate
`Option<&str>`. Whether the datastore handle can be made unable to carry a password into `Debug`
or a log line is work, not an inherited property.

**The handle must not reach the worker-internal env map.**
`crates/zeroship-worker/src/cache.rs` states the rule and a live test binds it:

> An identifier naming a resource SHARED WITH ANOTHER TENANT - a datastore key, or a database id
> once databases are shared - must never enter it, because two apps under one actor that read
> equal values have confirmed co-residency.

`worker_env_is_exactly_the_app_owned_ids` asserts the exact key set rather than a subset, because
the failure it guards is an addition.

---

## Column-level GRANT is the masking authority

The migration service emits column-level grants from the owner's own IR, withholding every column
whose classification is not `none` and granting the column that holds the mask instead. It already
writes classification and mask kind as `COMMENT ON COLUMN` sentinels
(`crates/zeroship-migrate-backend/src/mask_codec.rs`) and is the one process in the tree that does
not execute creator code, so this satisfies the AGENTS.md privilege invariant with no
`SECURITY DEFINER` wrapper and no system-schema state.

A creator migration cannot widen the ACL back open: the CONFINED ceiling
(`crates/zeroship-migrate-server/policies/confined.policy.toml`, whole file, plus the shared shape
at `policies/confined-system-shape.inject.toml`) grants exactly `schema.create_table`,
`schema.rename` and `safety.destructive_ops`, and a creator draft may only tighten.

Two deletions ship with it, in `runtime_role_provisioning_sql`
(`crates/zeroship-migrate-server/src/apply.rs`): the blanket
`GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA` and the prospective
`ALTER DEFAULT PRIVILEGES` rules. Both must be deleted rather than supplemented, because a
table-level grant alongside a column list returns the plaintext and `ALTER DEFAULT PRIVILEGES` has
no column-list form. Every apply regenerates explicit per-column grants inside the same
transaction as the DDL.

The runtime descriptor is demoted from security boundary to shape declaration. That is its correct
altitude whether or not sharing ships.

**This deletion breaks the workflow journal, and the ordering is load-bearing.** The journal's
tables are installed into the creator's schema by a separate bundle, so they are not in the
creator's IR and the regenerated per-column grants do not cover them. They are reachable today
only because the blanket grant covers everything in the schema, which is what goes. The answer is
not an interim grant for them: `docs/proposals/2026-09-19-workflow-journal-relocation.md` moves the
journal into the workflow service's own storage, and it should land FIRST so this design never has
to cover a table it does not own. If the ordering slips, the journal installer emits its own
grants as a throwaway, deleted in the same change that relocates the journal.

---

## Ownership and migration

A database is owned by the **project**. The project is the shared-infrastructure boundary and,
under the auth foundation redesign, the audience a subject is scoped to; it is also where the
encryption root key already lives. `Database` therefore carries `project_id` and no app column,
and the migrator role `zs_db_<dbs>_mig` is named by no app. There is no `owner` capability an app
can hold, so there is no ownership transfer, no ping-pong between apps, and nothing for an app
deletion to cascade into.

**One route, and the control plane is not on the apply.** The CLI addresses the database by id:

```
creator -> POST /v1/databases/{database_id}/migrations/apply   (zeroship-migrate-server)
```

`crates/zeroship-migrate-server/src/api.rs` already registers `/v1/databases/{database_id}`, and
its `ControlPlaneAuthenticator` (`crates/zeroship-migrate-server/src/auth.rs`) already authorizes
against the creator's own bearer rather than the platform control key. What changes is the
identity: the handler's path type is `AppId` and `provision_app_database`
(`crates/zeroship-migrate-server/src/provisioning.rs`) derives its schema with
`app_derivation::schema_name`. The URL was renamed; the identity was not.

`Resource::Database` gains the policy "principal may migrate N iff principal holds a qualifying
seat on N's project", resolved the way app authority already resolves. That is the load-bearing
change in the re-key, not the mechanical `app_id` occurrences across the service.

**With many zones the migration service is zone-local.** A single global instance would hold a
session advisory lock across a whole multi-file apply over a wide-area link. So `migrate-server`
runs per zone, and the CLI resolves a `dbs_` to its zone's endpoint once before the apply. That is
a real weakening of "the control plane is not in the path" and is recorded as such rather than
discovered: control answers one resolution request, and no control code runs during the apply.

**The apply lock moves to the database and stays SESSION-scoped.** The engine already takes a
session lock around a whole plan and releases it explicitly; the host acquires once on its pinned
session (`crates/zeroship-migrate-server/src/session.rs`) and passes `LockMode::AlreadyHeld` for
every IR file. Only the logical key changes. The publication reconciler's own advisory lock
(`crates/zeroship-migrate-server/src/publication.rs`) is keyed on an app-derived publication name
and is replaced, not reused.

**The deploy gate stops comparing schemas.** `crates/zeroship-control/src/registry.rs` today
predicates the deploy UPDATE on one `descriptor_sha256` matching the newest applied row and
reports `RegistryError::SchemaNotApplied { descriptor_sha256, applied_sha256 }`. That comparison
is deleted, along with the catalog read behind it
(`crates/zeroship-control/src/publication/catalog.rs`). The descriptor-mismatch arm of
`SchemaNotApplied` goes with it; the artifact-inconsistency arm survives as a manifest
self-consistency check that needs no knowledge of any database.

`Manifest.runtime_descriptor` (`crates/zeroship-bundle/src/manifest.rs`) still changes shape,
because an app now carries one descriptor per database: entries of `{ label, database_id }` plus
that database's declared schema. It carries no hash to compare and no database id used as a name.
Every producer, consumer, fixture and doc moves in the same patch.

**Nothing is lost that fails closed anyway.** A build expecting a column the database does not
have gets `42703 undefined_column` at query time, which names the column. The right check is
compatibility rather than equality - refuse when the database lacks something the app requires,
say nothing when it has grown things the app does not use - and only a process holding both the
descriptor and a connection can evaluate that. Control holds a hash and is not such a process; the
worker is. That is Open 11.

### The apply

All subtraction from the catalog happens before any DDL commits; all addition happens after every
DDL has committed:

```
L    host takes a SESSION advisory lock on the database key, held to U
P    preflight: lower every IR file, refuse a denied plan
T1   one transaction: head FOR UPDATE, reap E-1 roles, claim, shrink this
     Database's publication members
D1..DN  the DDL, engine-journalled, every file passing LockMode::AlreadyHeld
T4   one widen transaction: mint E+1 roles for every live binding, widen those
     members, advance the head to E+1, emit the marker - iff the committed
     schema delta requires rotation
E    advance databases.schema_epoch on the control connection
U    release the lock
```

The apply writes nothing to the control plane but step E, and step E is a projection: if it fails
or is lost to a crash, control composes a role name that no longer exists, the next statement
fails `SET LOCAL ROLE`, and the caller re-resolves. There is no ledger to close and no
exactly-once obligation across two databases.

**A serving app is never fenced.** The apply mints `E+1` and drops `E-1`; it never touches `E`.
Every partial crash state leaves apps serving on `E`. Recovery is a plain retry: the engine
journal skips completed DDL, and the head's recorded journal state decides whether the rotation is
still owed. A retry after a crash between the last DDL and T4 finds every version applied and must
STILL rotate.

---

## CDC

- **One relay-owned slot and one pgoutput stream per Datastore.** Slots replicate decode work,
  they do not partition it. A logical slot is bound to one PG database, so this part is physics.

- **One relay PROCESS per Datastore, and that part is a choice.** A single relay could hold N
  replication connections the way the worker holds N pools. It should not, and the reason is what
  a relay owns when it dies: a slot that falls behind retains WAL upstream, and
  `max_slot_wal_keep_size` measures `-1`, unbounded, at `context = sighup`. One relay per cluster
  means one relay's outage threatens one cluster's disk; one relay per zone means every cluster in
  the zone retains WAL simultaneously.

  The general rule, which also explains why the worker and `migrate-server` go the other way: a
  **request-serving** process must span clusters or placement fragments, because a request can name
  any database in the zone. A **stream-consuming** process need not, because each stream is
  independent, and should not, because the failure it owns is per-stream. So the worker and
  migrate-server take a list of DSNs; the relay keeps one and is scaled by count.

  The cost is on the worker side and is not yet recorded elsewhere:
  `ZEROSHIP_WORKER_CDC_RELAY_URL` is one URL today, and becomes a list. The worker resolves which
  relay serves which cluster the same way it resolves DSNs - connect, ask, build the map - so no
  new mechanism, but more connections and one more list in config.
- **One relay-owned shared publication per Datastore**, its membership the union of every
  Database's safe table projections plus the heartbeat exception. Creating or migrating a Database
  edits only its member entries under the Datastore publication mutex. One Database must never run
  `ALTER PUBLICATION ... SET TABLE` over the shared object.
- **The published column set per table is the INTERSECTION over every binding on the database**,
  and the plaintext of any column with `classification != none` is never published to anyone:
  publish the field's own column, which holds the mask, and exclude `__zs_raw__<f>`, which holds
  the plaintext.
- **The relay performs fan-out**, resolving
  `(datastore, physical schema) -> database_id -> active bindings -> app_id`. Today
  `crates/zeroship-data-cdc-server/src/source.rs` derives its slot name from
  `replication_names::publication_name(app)` and is app-keyed throughout; that keying moves to the
  datastore.
- **The broker's routing key gains the database.** `crates/zeroship-data-orm/src/cdc/broker.rs`
  routes on `(app_id, collection)`, and the hub in `crates/zeroship-data-cdc-server/src/hub.rs`
  keys subscribers under an app. With one app reaching two databases that key is ambiguous: two
  databases can each declare `users`, and an event from one would land on a subscription to the
  other with nothing raising. It becomes `(app_id, database_id, collection)`. **This is the one
  re-key in the design whose omission is silent rather than loud.**
- **Fan-out authority is control's binding topology**, never `pg_auth_members` or a worker refresh
  map. Binding changes are revision barriers that purge old relay and worker queues before control
  exposes them.
- **`pg_logical_emit_message` is revoked from `PUBLIC`** on both four-argument overloads before the
  marker is trusted; no worker, app or relay role is granted it, and the migration service emits
  `(database_id, database_epoch)` in the widen transaction.

Two costs, both named. The owner loses plaintext reactivity on classified columns: a subscription
never carries the plaintext, for anybody, including the owning app. Reads still do. And binding
changes pay a relay-and-worker revision barrier, which can reconnect healthy apps that shared a
relay response.

---

## The stale-binding fence

A binding goes stale two ways and both are answered by whether
`SET LOCAL ROLE "zs_bind_<bnd>_e<E>"` succeeds. Role membership answers "does this app still hold
a live binding"; the `_e<E>` answers "is the shape this isolate was built against still the shape
the database has". Neither is answered by anything the worker compares.

An apply that changes the schema advances `E -> E+1`, mints `zs_bind_<bnd>_e<E+1>` for every live
binding and drops `zs_bind_<bnd>_e<E-1>`. An isolate built against `E-2` fails at
`SET LOCAL ROLE`. Steady-state cost is zero: the epoch is a substring of a role name the batch
already sends. Live epochs are capped at two, fail-closed: **an apply that cannot drop `E-1`
refuses before any DDL commits and never advances to `E+1`.**

Note what this does and does not cover. It catches *the schema moving forward under code that is
behind*. It does not catch *code moving forward against a schema that is behind*, because there
the binding role for epoch `E` exists and `SET LOCAL ROLE` succeeds. That direction is Open 11,
and until then it surfaces as `42703 undefined_column` at query time.

**Error taxonomy.** The classifier
(`crates/zeroship-data-orm/src/backend/postgres/pg_error.rs`) today matches SQLSTATE 22023 plus the
exact role name and collapses it into `SCHEMA_NOT_PROVISIONED`
(`crates/zeroship-data-orm/src/error.rs`). Under this design there are three outcomes, and the
third is decided BEFORE a statement is sent rather than by a SQLSTATE:

- `42501 permission denied to set role` -> `GRANT_REVOKED`. Terminal, 403-shaped, never retried,
  never falls back to the pool. The reconciler leaves a revoked binding's role standing precisely
  so this stays separate from a retired epoch.
- `22023 role does not exist` -> `SCHEMA_EPOCH_STALE`, retryable, the same condition
  `Verdict::ReResolve` already carries.
- **no binding at all** -> refused at RESOLUTION with `binding_not_resolved`, terminal. A role name
  is composed only from a database edge (`pg_session_sql::session_setup_sql` returns `Err` when
  `binding.session_role()` is `None`), and both call sites build the batch and `?` on it before
  `simple_query`, so a narrowing connection with no edge never reaches the classifier at all.
  Control serves only a binding that is `active` with `observed_generation` caught up on a database
  that is `active`, so "never provisioned" is decided where it is decidable.

**Do not put the third arm back in the classifier.** An earlier revision of this document had it
there, reasoning that the classifier "only needs to know from the injected binding whether a live
binding exists". That reasoning is wrong: holding a binding is not evidence that it is live. The
worker's store is process-wide and cleared only by `deprovision_app`, so a worker can hold an edge
Control has since stopped serving, and the classifier's only input is the binding it was handed -
it would have answered `SCHEMA_EPOCH_STALE` for that case either way. What distinguishes a
withdrawn binding is a re-resolution, which is a protocol rather than a classification.

The three codes being pairwise distinct is bound by
`pg_error::the_three_setup_outcomes_are_pairwise_distinct`, so a later collapse is a red test rather
than a silent merge.

**The `__zeroship_admin` schema is created**, in the one shape the privilege invariant permits -
state a separate service writes and the worker only reads. Exactly one table, holding the current
schema epoch per database, written by the migration service inside the transaction that mints the
new epoch's roles. Zero worker-callable functions: no `SECURITY DEFINER`, no
`EXECUTE ... TO PUBLIC`, no `GRANT USAGE ON SCHEMA` to any app or binding role. The data plane
still reads nothing from it - the role name carries the epoch precisely so no query has to.

---

## Encryption

Today the key derives from a per-app salt and
`canonical_aad(app_id, collection, column, row_pk)` binds a hardcoded `WIRE_VERSION_V1`
(`crates/zeroship-data-orm/src/encryption/aad.rs`), while the root key is per project
(`db/migrations-ts/20260911000100_project_data_keys.ts`,
`crates/zeroship-data-orm/src/encryption/keys.rs`). App keying fails in opposite directions on both
new axes: co-binding-holders derive different keys and get an AEAD failure on data they are
entitled to read, and one app across two databases derives one key with no database in the AAD, so
a ciphertext lifted from one database verifies in the other. The target:

```
derive_key(project_root, database_id)
canonical_aad(WIRE_VERSION_V2, database_id, collection, column, row_pk)
```

Encryption stops fencing co-binding-holders and becomes purely at-rest, which is what it should
have been. If a column must be readable by one app only, that is a column-level GRANT, and on the
subscription path a column the publication does not carry.

Changing the salt changes every derived key and changing the AAD changes every tag, so **this
lands in the same change that makes database ids exist**, not after.
`crates/zeroship-core/src/app_derivation.rs` already records why the deterministic half is the
dangerous one: a new salt makes equality search over an encrypted column return fewer rows and no
error. Pre-launch the answer is drop and re-encrypt.

Salting by the database rather than the datastore also keeps ciphertext valid if a cluster is ever
replaced underneath a datastore, since the database id does not change.

**The root key stays on the project, and the zone decision is what settles it.** The argument for
moving it down to the database was blast radius: a project whose apps sat in two zones would serve
one root into both, so the key would reach further than any single zone. A project pinned to one
zone cannot do that. The root is served only into its own zone, `zone_scoped_app_read` narrows it
there, and the derived key is already per database because the salt is the database id. So
`project_data_keys` needs no change.

---

## Metering

- **Op counts stay keyed on the app.** `db_reads` / `db_writes` / `db_rows_written` are emitted
  against the server-injected app id at the op boundary (`crates/zeroship-data-orm/src/exec.rs`),
  and the app that issued the op consumed the compute.
- **The billing principal for a resource is the project**, because no app owns a database. Any
  metric measuring the *resource* attributes to the project; any metric measuring an *op*
  attributes to the app. Collapsing them is what makes one app pay for a co-tenant's bytes.
- **The database is a dimension with no producer today.** `UsageEvent.dims`
  (`crates/zeroship-core/src/usage_event.rs`) is constructed empty because `Meter::increment`
  (`crates/zeroship-metering/src/meter.rs`) and `MeterHandle::record`
  (`crates/zeroship-metering/src/lib.rs`) carry no third axis. Under N:M this stops being an audit
  nicety: an app with two databases posting one number cannot be split at all. The aggregate
  primary key stays `(app_id, period, metric)`, so the dimension is observable and auditable but
  does not reach the spend engine; spend is an app-level control.
- **Two suppression holes close.** Failure is free today, every emit sitting after the `?`, with a
  regression test asserting that a failed query did not bill; emit `db_statement_us` in both arms
  and change that test with it. Subscriptions are unmetered; in the target the relay owns the
  shared slot, subscription time meters to the app and retained WAL to the project.

---

## Creator surface

One `zeroship.jsonc` for the whole workspace. Databases are declared once at workspace level; apps
are declared beside them and name the ones they use.

```jsonc
"databases": {
  "main":      { "id": "dbs_...", "migrations": "./db/main",      "out": "./generated/zeroship/main" },
  "analytics": { "id": "dbs_...", "migrations": "./db/analytics", "out": "./generated/zeroship/analytics" }
},
"apps": {
  "storefront": { "app": "app_...", "databases": ["main"],              "primary": "main" },
  "admin":      { "app": "app_...", "databases": ["main"],              "primary": "main" },
  "reporting":  { "app": "app_...", "databases": ["main", "analytics"], "primary": "main" }
}
```

The map key is a **local label**, not a resolvable name: it refers to an entry a few lines above,
and the CLI dereferences it locally before it makes any request. The label reaches the runtime only
through the manifest, which is the app's own build artifact. Declaring a database here does not
grant access to it; deploy verifies that a binding already exists.

### `env.db` barely moves

`Collection` (`crates/zeroship-data-v8/src/v8_classes/collection.rs`) owns a full `DbBinding` copy
rather than a reference to a shared one, and `cached_collection`
(`crates/zeroship-data-v8/src/v8_classes/db.rs`) stamps it in at mint time. So a collection routes
by what it carries, and the collection surface needs no change at all. The JS side already
anticipates a second handle: `readFrom` (`crates/zeroship-data-v8/js/runtime/read.ts`) refuses a
source whose handle differs, with the message "read source belongs to another database".

Four verbs read the `Db` wrapper's single binding and therefore need a handle of their own:
`transaction` and `collection(name)` and `declareMaskPolicy` (all in
`crates/zeroship-data-v8/src/v8_classes/db.rs`) cannot infer a database, while `from` and `live`
already derive theirs from the collections they are given.

```ts
await env.db.users.find({ where: { active: true } });            // unchanged
await env.db.transaction(async (tx) => { ... });                 // unchanged
await env.databases.analytics.events.insert({ ... });            // only if declared
await env.databases.analytics.transaction(async (tx) => { ... });
```

`env.db` is the app's primary database and `env.db === env.databases[primary]` by object identity,
so there is one concept and one code path. A single-database app sees no difference. Collection
name collisions across databases cannot arise, because the collections live on different handles
rather than in one flat namespace - which also removes any need for a build-time refusal that would
have been resolved by renaming a collection inside a database the app might share.
`RESERVED_ENV_DB_NAMES` (`crates/zeroship-data-v8/js/runtime/install-schema.ts`) needs no new
entries, because nothing new is planted on `env.db`.

---

## Transactions

`env.db.transaction()` covers exactly one database. There is no two-phase commit and none is
proposed.

**A dispatch against database B inside a transaction on database A is refused.** This is the
sharpest consequence of opening the second axis, because today it would be silently admitted:
`CapturedRoute::capture` (`crates/zeroship-data-orm/src/tx_route.rs`) decides `in_tx` by comparing
app identity alone, and its comment states why the schema is excluded:

> SEC-1 compares TENANT against TENANT. The schema rides along; it is never the admission key,
> because two apps sharing one database would share a schema and must still not share a transaction
> frame.

That reasoning is correct for the many-apps direction and incomplete for the many-databases one.
The admission key becomes the (app, database) pair. Without the change, B's SQL runs on a
connection narrowed by `SET LOCAL ROLE` to A's binding role. The refusal must exist in Rust, not
only in JS; `readFrom`'s handle comparison is the pattern to copy.

`validate_binding_target` (same file) already compares both app identity and schema against the
binding and errors on either mismatch, so the mismatched-pair fence is already written.

`TxLanes` (`crates/zeroship-data-orm/src/tx_lanes.rs`) keys lanes by `app_id`, so an app can hold
one transaction at a time. Under N:M it holds at most one per database and the key becomes
`(app_id, database_id)`. **The lane key uses the database id and never the creator's label**, or
two co-resident apps both calling a database `main` would compare equal.

---

## Teardown

**Delete an app.** Revoke both edges of every binding the app holds, then drop
`zs_bind_<bnd>_e<E>` for every live epoch. That is the complete teardown of an app's access and it
is instant. No data is destroyed, because the app owns no database, and a deleted app never
destroys data another app can still read.

**App deletion refuses a bound app rather than revoking for it.** `delete_app`
(`crates/zeroship-control/src/organizations.rs`) ends an app by setting `apps.project_id = NULL`,
which is an UPDATE to the exact key `database_bindings_app_project_fkey` references under
`onUpdate: "restrict"`. Left alone that surfaced as a constraint violation - a 500, measured rather
than inferred - so the UPDATE now carries a `NOT EXISTS` over the app's bindings and
`OrganizationError::AppHasDatabaseBindings` answers 409 naming the databases and the unbind route.

That is the smaller of the two available answers. The other is for deletion to revoke the app's
bindings as part of its own teardown, which is what makes the paragraph above's "instant" true
without a creator first unbinding by hand. Which one is right is app-lifecycle work rather than
this design's, and the current behaviour is at least a refusal a creator can act on instead of a
constraint error.

**Unbind DELETEs the binding row rather than marking it `revoking`.** The role name derives from
the binding id, so a withdrawn edge whose row survived would keep `zs_bind_<bnd>_e<E>` reserved,
and a re-bind would either resurrect a dropped role or collide with
`database_bindings_natural_key`. The reconciler's "converge every binding" pass drops roles no
declaration names, which is the same sweep that must reap a role left by a create whose row rolled
back. `revoking` and `revoked` stay in the CHECK for the reconciler's own use.

**Delete a database MARKS the row `deleting`; it does not remove it.** Only when its binding set
is empty, and the refusal names the bound apps. The row survives because the schema and its data
survive: control is not the process that can drop them, and the reconciler that can must be TOLD
to, never left to infer it. Absence is the one thing a reconciler cannot read safely - a failed or
partial read of the declarations is indistinguishable from "nothing is declared here", and under
that reading a network error destroys a tenant's data. So the two sweeps are deliberately
asymmetric: a ROLE is reaped by absence, because the next pass re-grants it, and a SCHEMA is
dropped only against an explicit `deleting` row, because nothing re-creates the data. The
`databases_status_check` already admits the value. The reconciler removes the row once the drop
has committed, which is also what frees the name for reuse.

Two consequences worth stating rather than discovering. A database being deleted still holds its
name under `databases_project_name_key`, so re-creating one by the same name waits for the drop -
correct, because the old schema is still there. And `bind_database` refuses a `deleting` database:
bind and delete serialize on the same organization lock, so without that refusal a bind admitted
just after the mark would hand an app a binding to a schema already condemned.

The step order in `crates/zeroship-data-orm/src/cdc/lifecycle.rs` and
`crates/zeroship-data-v8/src/service.rs` is right; each step re-keys:

| step today | under the decoupling |
| --- | --- |
| subscription gate, keyed on the app | keyed on the database: refuse while any subscription on any binding holder is observable |
| drain the broker | the broker's routing table gains the database, so this fans out to every binding holder |
| consumer cancel plus slot teardown | **No shared CDC object is dropped.** The relay-owned slot, stream and publication live for the Datastore; remove only this Database's publication members and fan-out routes |
| `DROP SCHEMA "<app_id>" CASCADE` | `DROP SCHEMA "db_<dbs>" CASCADE` |
| `DROP ROLE "app_<id>_role"` | `DROP ROLE zs_db_<dbs>_{mig,rw,ro}`, still after the schema |

The third is the one that fails silently if it is ported rather than re-keyed.

**Retire a datastore.** Only when it holds no database. With relocation unsupported, that means
waiting for its databases to be deleted, or replacing the cluster underneath it instead.

**No teardown here touches a workflow journal.** The journal leaves creator schemas under
`docs/proposals/2026-09-19-workflow-journal-relocation.md`, so deleting a database destroys no
app's runs and deleting an app removes its runs through the workflow service rather than by a
schema drop. That also closes a defect this design would otherwise create: with the journal still
in the creator's schema, dropping a SHARED database would take the workflow state of every bound
app with it, not just the one doing the dropping.

---

## SQLite dev tier

One file per database, `zs-db-<dbs>.sqlite`, ATTACHed under alias `db_<dbs>`. `attach_app_file`
(`crates/zeroship-data-orm/src/backend/sqlite/mod.rs`) already is a per-database handle under a
different name, with a dedup set because SQLite errors on a duplicate alias; the dedup key becomes
the database id. There is no Datastore entity on this tier and no control plane at all.

Three fidelity gaps, all owed to `docs/reference/sqlite-divergences.md`, which carries no grants
row today:

- **Bindings map to no physical object.** SQLite has no roles and no column ACLs, so everything the
  production tier enforces in the catalog the dev tier enforces in process. A dev-tier pass is
  therefore not evidence about production for any binding, capability or epoch behaviour.
- **The schema epoch has no carrier**, since it rides a role name. A dev-tier equivalent is owed
  and is not specified here. This is live disagreement, not an oversight: a sibling design reaches
  the opposite conclusion under the heading "No dev epoch, and no dev authority domain", and it
  must be settled rather than inherited.
- **One writer per file**, so a shared database behaves worse in dev than in production, which is
  the inverse of the usual direction and the one that gets filed as a bug.

---

## Why it is this way

**Provenance.** Claims marked RE-DERIVED were measured during this revision against the running
`postgres:16.15` container, which matches the `deploy/compose/docker-compose.yml` pin; the
instrument is named with each. Claims marked CARRIED were measured on earlier majors during the
original drafting and have NOT been re-measured. Claims marked GATED name the arm of
`crates/zeroship-data-orm/tests/postgres_tenant_fence.rs` that re-runs them, each on its own
throwaway container that refuses a server below the deploy pin, and each paired with the control
that keeps it from passing over a fixture which granted nothing. The catalog-cost claims stay
rationale rather than becoming arms; Open 5 says why.

**Self-hostability decides where topology lives, not convenience.** The table under Where a fact
lives sorts each fact by who owns it, and the load-bearing asymmetry is that a migration is not
overridable: the corpus is ordered, journalled and verified, so a self-hoster cannot declare their
own clusters in it without forking it permanently.

That rules out the corpus. It does not imply a new document, which an earlier revision of this
design reached for: a cluster already has a credential in a service config, and reaching it proves
it exists, so registration needs no second declaration and no import semantics. A zone has no such
proof, which is why it stays declared and datastores do not. Three decisions rest on this - how a
datastore comes to exist, where zones beyond the default are declared, and why `status` is
control-side rather than something an operator edits and pushes.

**Equality between an app's build and a database's schema is a coupling mechanism.** It is the
reason the deploy gate and its ledger are deleted rather than re-keyed. A hash comparison forces
every app bound to a database to move together on every migration, including migrations that break
nothing, which is the property this design exists to remove. The general form is worth stating
because it will be tempting again: **any check that compares an app artifact to a database for
equality re-couples them.** Only a subset test does not.

**Catalog scope decides every cardinality here. RE-DERIVED** with
`SELECT relname, relisshared FROM pg_class WHERE relname IN (...)`. Shared across the whole
cluster: `pg_authid`, `pg_auth_members`, `pg_database`, `pg_db_role_setting`, `pg_tablespace`. Per
PG database: `pg_class`, `pg_namespace`, `pg_extension`, `pg_publication`, `pg_publication_rel`,
`pg_collation`, `pg_default_acl`. So every database role and every live epoch competes in one
cluster namespace, while publications are per database.

**A logical slot is bound to one PG database. RE-DERIVED**: `pg_replication_slots` carries a
`database` column. One slot per Datastore is therefore physics rather than a design choice.
`max_replication_slots` is `context = postmaster`, so raising it is a restart; read the value with
`SHOW max_replication_slots`. With Datastore collapsed onto the cluster, one slot is used and the
budget is not a constraint.

**Multiple PG databases in one cluster are rejected.** The four things a separate PG database buys
are, measured above: extension isolation, `CONNECT` as a fence (`datacl`/`datallowconn`), encoding
and collation, and a smaller per-backend catalog. The first is unreachable because the CONFINED
ceiling gives a creator no extension vocabulary at all. The second buys nothing because there are
no per-tenant logins to fence and the worker must connect to every database it serves. The third is
already handled more finely by `COLLATE "C"` at the column level. The fourth is real and is a
density argument, and the answer to density here is another cluster, which shrinks the catalog
*and* the cluster-wide role namespace *and* the failure domain *and* adds connection capacity.
Against that: a connection targets one PG database, so N of them means N pools per worker thread;
one slot means one walsender, so N of them means N walsenders reading the same WAL; and the slot
budget caps the split anyway. Splitting multiplies decode work rather than partitioning it.

**Relocation is not supported, and the cluster moves instead.** A per-schema move needs
build-by-replay, its own full-fidelity publication (the relay's is deliberately lossy, since its
column lists withhold plaintext), a write quiesce with a straggler deadline, manual sequence
advancement, and a cutover. That is a large amount of machinery for one operation. It is not needed
for hardware refresh: physical replication, promote, and repoint the service config that reaches
the cluster. The datastore row is keyed on `system_identifier`, which a promoted replica carries
forward, so the whole cluster moves with no entity changed, no epoch rotated and no app aware. What is given up is rebalancing a hot cluster,
consolidating two half-empty ones, and offering a dedicated cluster as an *upgrade* - if that tier
ships it must be chosen when the database is created.

**`transaction_timeout` does not exist on the deployed major. RE-DERIVED**:
`SELECT count(*) FROM pg_settings WHERE name='transaction_timeout'` returns zero on 16.15; the GUC
arrived in PostgreSQL 17. `DB_STATEMENT_TIMEOUT_MS` bounds one statement and
`DB_IDLE_IN_TX_TIMEOUT_MS` one idle gap (`crates/zeroship-data-orm/src/budgets.rs`), and
`env.db.transaction()` holds a dedicated connection for a whole JS callback, which is the shape
that defeats both.

**`max_identifier_length` and truncation. RE-DERIVED** with `SHOW max_identifier_length`.
PostgreSQL truncates past it silently, and the epoch sits at the END of `zs_bind_<bnd>_e<E>`, so a
future prefix change that pushed past the limit would collapse two epochs onto one role rather than
error. The composer must refuse a name it would have truncated.

**`WITH SET FALSE` on the binding-to-database edge is load-bearing. GATED** by
`a_worker_login_reaches_a_shared_database_only_through_a_live_binding_role`. Without it the
worker assumes the database role directly and the chain is decorative. Two bindings on one
database: the worker assuming the database role gets `permission denied to set role`; narrowed to
binding A it reads its row; after revoking the database role from A's role it gets
`permission denied for schema`; co-tenant binding B is unaffected, which is the arm that separates
a fence from a database-wide outage.

**`WITH INHERIT FALSE` on the worker-to-binding edge is required, and the role attribute is not a
substitute. GATED** by `only_a_noninheriting_membership_keeps_a_binding_out_of_ambient_login_privileges`.
Three arms differing in one variable, and two of them must SUCCEED: a plain `GRANT` of the
intermediate with the login inheriting returns the row; the same grant with `ALTER ROLE w NOINHERIT`
still returns the row; `GRANT ... WITH INHERIT FALSE` refuses with `permission denied for schema`
while the same login still reaches the database by assuming the binding. PostgreSQL 16+ records
`inherit_option` per membership at grant time, and a pre-existing membership stays inheriting when
the attribute is flipped. Below 16 the option does not exist and this design has no fence. The
worker's boot posture already refuses any inheriting membership at all, so a database still
carrying the old grant shape is detected rather than silently trusted.

**The worker may never hold a direct membership in a database role.** One such grant restores
unrevocable access, and nothing in PostgreSQL will complain because both memberships are
individually legal. It is catalog-checkable, and `db_posture`'s boot check already counts every
inheriting membership row rather than checking a pair.

**No CRUD code may reach a raw connection, and this must stay a compile-time property.** The role
fence is applied by two functions
(`crates/zeroship-data-orm/src/backend/postgres/pg_autocommit.rs` and
`crates/zeroship-data-orm/src/backend/postgres/implementation.rs`, `apply_per_app_role`); anything
issuing SQL outside them is unfenced. Under a private database the blast radius of one unfenced
statement was a single app's schema; under sharing it is every database in the cluster, and
`WITH INHERIT FALSE` only converts such a statement from succeeding to failing at runtime. The
compile-time route is what stops the site existing.

**Why roles and not RLS, given that nine platform tables use RLS.** They are not alternatives;
they separate different things, and no substitution is available between them:

```
  schema separation   roles. Nothing else can do it: a policy is bound to one
                      relation (pg_policy.polrelid), and reaching a schema at all
                      is a USAGE grant, which is a role.
  column separation   column grants. RLS is row-level and cannot express columns,
                      so the masking design needs grants regardless.
  row separation      RLS, forced. Roles cannot express rows, which is why
                      db/migrations-ts/20260702000800_policies_rls.ts uses it for
                      nine shared platform tables keyed on app_id.
  decode separation   publication column lists. Neither roles nor RLS reach the
                      decode path at all.
```

The decisive asymmetry for the data plane is what ABSENCE does. A table created without a grant is
unreadable; a table created without `ENABLE ROW LEVEL SECURITY` is fully readable. Roles fail
closed on omission and RLS fails open, and this design applies creator-authored DDL that adds
tables the platform did not write.

Shared-table multi-tenancy - one `users` table with a tenant column and a policy - is not available
here for a product reason rather than a PostgreSQL one: creators author their own schemas, so two
apps' `users` tables have different columns and there is no shared relation to put a policy on.
That is also why the nine tables that do use RLS are all platform-authored.

**The irreducible limit.** PostgreSQL has no server-side notion of which app a shared-login session
is acting for; `session_user` is fixed at authentication, and `SET ROLE` is a lateral move inside
the closure, never a one-way narrowing. So every available fence decides whether a binding is
*alive*, never whether the worker picked the binding matching the dispatch. That binding is
worker-side, enforced by Rust provenance and the absence of a raw-SQL surface. Per-app logins would
not change it: the process would then hold every tenant's credential.

**Column grants add, they never subtract. GATED** by
`a_table_level_grant_returns_the_column_a_column_list_withheld`. A table-level `GRANT SELECT`
alongside a column list returns the plaintext, and `ALTER DEFAULT PRIVILEGES` refuses a column list
outright with `default privileges cannot be set for columns`. Schema evolution then fails closed by
a PostgreSQL property rather than by a reconciler: a column added after the grant carries no ACL
entry and is unreadable.

**Logical decoding consults no ACL and no RLS. GATED** by
`logical_decoding_ignores_the_column_acl_and_obeys_the_publication_column_list`. The same role
refused `SELECT` on a column receives its plaintext in the decoded stream when the publication has
no column list, and does not when the same slot's changes are decoded under a publication whose
column list omits it. A publication column list *does* filter decoded output and is a genuine
server-side fence, and PostgreSQL refuses conflicting column lists for one table across the
publications named on one decode stream, so a table has exactly one published column set per stream
and the shared stream serves the weakest reader.

**The column list is incompatible with `REPLICA IDENTITY FULL`, and neither DDL step refuses.
GATED** by `a_column_list_and_replica_identity_full_are_accepted_then_make_writes_fail`. Adding
`REPLICA IDENTITY FULL` to a table that already has a column list is accepted, and creating a
column list on a table already `FULL` is accepted; then every `UPDATE` and `DELETE` fails
`42P10` with `Column list used by the publication does not cover the replica identity` while
`INSERT` still succeeds. The symptom is not a CDC fault but a table that has silently become
append-only on the creator's write path.

**The two role failures split by SQLSTATE alone. GATED** by
`set_role_separates_a_missing_role_from_a_role_the_session_may_not_assume`, from a real
non-superuser session, which matters because a superuser's `SET ROLE` permission is checked against
`session_user` and hides the split - the arm runs the same two statements as a superuser and
watches one of them succeed. Role does not exist is `22023 invalid_parameter_value`; role exists
but the session is not a member is `42501 insufficient_privilege`. `22023` is the generic bad-GUC
code, which is why the classifier pins the exact role name alongside it.

**`SET LOCAL ROLE` must remain the FIRST statement of the setup batch. GATED** by
`the_driver_reports_the_setup_batch_s_first_failure_by_sqlstate`. PostgreSQL aborts a
simple-query batch at the first failing statement and emits exactly one `ErrorResponse`, and
`compio-postgres` hands that response's own SQLSTATE and message to the caller for both `42501` and
`22023`, leaving the transaction in `25P02`. If anything is placed before the role statement, that
statement's failure masks the role failure, the SQLSTATE taxonomy silently collapses, and because
the epoch rides the same role name, a rotated epoch surfaces as whatever the earlier statement
failed with.

**Revocation lands within one in-flight transaction, not one statement. CARRIED.** After a revoke
from a second connection, the next `SET LOCAL ROLE` on the same backend fails `42501` with no
reconnect. A session running as a role that *inherits* the privilege fails on its very next
statement; a session that has *assumed* a role holding it directly continues to the end of the
transaction. This design assumes the binding role, so the bound is one transaction, which nothing
currently bounds - see `transaction_timeout` above.

**The catalog costs the role graph was suspected of both measured at zero. RATIONALE, not a
constraint.** `CREATE ROLE` inside an apply bracket did not serialize applies in other databases of
the same cluster, and `SET ROLE` was flat across a large growth in `pg_auth_members`. That is why
the epoch rides the role name instead of an extra `SELECT epoch` in the setup batch: the name form
cannot be removed without removing the connection.

These two are deliberately excluded from Open 5's test target. They justify a choice rather than
hold a boundary: if they turned out false the design would be slower, not wrong. Gating them would
mean asserting a timing in a test, which is flaky, and recording one here, which the no-statistics
rule forbids. Re-measure them if the choice is ever questioned, not on a schedule.

**The apply cannot be one transaction.** `crates/zeroship-migrate-core/src/engine.rs` states its
contract - everything ahead of it commits in its own transaction - and the host loops the engine
once per IR file, with earlier files already committed when a later one fails. The engine's crash
recovery is journal-driven on exactly that basis.

**Nothing spans the control database and a cluster, and the design no longer needs anything to.**
`migrate_server.database_url` and `migrate_server.provision_database_url` are separate secrets
(`crates/zeroship-migrate-server/src/config.rs`), so an apply's control-side write and its DDL
cannot share a transaction. With the ledger deleted the only control-side write an apply makes is
the epoch projection, and a lost projection is self-correcting rather than a stale authority. The
same asymmetry is why granting is a reconciler: a binding's row and its roles live on different
servers.

**The worker-internal `env_vars` map is a disclosure channel by construction.**
`zeroship_runtime::core::init` copies every entry into `process.env`, so anything placed there is
readable by app JS and by any npm package that walks `Object.keys`. It may carry only identifiers
the app already possesses. Being unforgeable is not sufficient: that is a forgery property, and the
concern is disclosure.

**Cross-project database sharing is refused permanently.** Three independent reasons:

1. **The unmask policy is authored by the READING app.**
   `crates/zeroship-data-orm/src/protection/mask_policy.rs` states it: the policy comes from the
   creator's own source, at boot, and nowhere else on the PostgreSQL arm. A co-binding holder from
   another project could ship a permissive policy in its own bundle and unmask another project's
   PII. The catalog sentinels do not close this: they carry mask kind and classification, not an
   actor-to-classification policy.
2. **A PostgreSQL schema has exactly one owner, and ownership IS the migrator's authority.** Owner
   privileges are implicit and unrevokable, and there is no second owner slot.
3. **The apply lock and the journal have no second-tenant axis.**

Within one project, a co-binding holder mis-declaring a mask policy is not a boundary crossing.
Across projects it is, and no role fixes it. The composite foreign keys make this structural rather
than a check.

**What this design does NOT deliver is shared evolution across projects.** Two projects jointly
evolving one schema needs adjudicated multi-writer DDL, in which the migrator stops being
least-privilege-by-ownership and becomes a policy-adjudicated writer arbitrating per-table claims
between peer drafts. That puts creator-influenced policy inside the one service trusted precisely
because it does not execute creator code. If the requirement is cross-project shared evolution,
this is the wrong design and multi-writer DDL is the actual project.

**Three residual gaps are accepted, not solved here.** An app built against an older shape of a
shared database is not refused at deploy and not detected until a query touches something that is
not there, where it fails closed with the column named; the compatibility check that would move
that earlier is Open 11. Per-cluster admission control does not exist: spend limits throttle an
app's requests and cannot protect a shared cluster from an app under its limit. And a zone-local
`migrate-server` means the CLI needs one control round trip to find it.

---

## Open

1. **Is a dedicated cluster a plan tier?** NEEDS-DECISION, product-shaped. With relocation
   unsupported it must be chosen when a database is created or not offered, because an existing
   database can never move to its own cluster.

2. **Supply the schema epoch producer.** BUILDABLE. The consumer ships and is tested: `SchemaEpoch`
   (`crates/zeroship-data-orm/src/transaction/reducer/identity.rs`) and its comparison returning
   `Verdict::ReResolve`. The machine is a tautology in production: the single production
   construction site (`crates/zeroship-data-orm/src/transaction/driver.rs`) mints a zero epoch and
   echoes the expectation back as the observation, so `classify` can only return `Current` and the
   typed denial codes are unreachable outside tests. The work is the record, the migration-service
   write and the control-plane read that composes the binding.

3. **What bounds total transaction duration?** NEEDS-DECISION. Not `transaction_timeout`: it does
   not exist on 16 and does from 17, so in a mixed fleet the answer differs per cluster and a design
   assuming one answer is wrong on half of it. Blocks publishing a revocation-lag guarantee, not the
   mechanism.

   **The fact that would settle it per cluster is now read and thrown away.**
   `read_cluster_identity` (`crates/zeroship-migrate-server/src/datastore/identity.rs`) takes
   `server_version_num` alongside the system identifier on every pass, and uses it only to refuse a
   cluster below 16. `zeroship.datastores` has no column for it
   (`db/migrations-ts/20260919000200_database_entities.ts` declares `system_identifier`,
   `execution_zone_id`, `status`, `last_error` and the timestamps), so control cannot tell which of
   its clusters would honour `transaction_timeout` and which would ignore it. Persisting what the
   reconciler already reads is the cheap prerequisite to any per-cluster answer here, and it is a
   prerequisite rather than the answer: deciding what the bound SHOULD be is still open.

4. **Capacity-aware placement.** DEFERRED, named. Placement counts rows control already owns and the
   operator flips `draining`. Byte, connection and slot awareness needs a probe holding a privileged
   read connection - the cluster reconciler is the natural home, since it already holds one per
   cluster and executes no creator code. Blocks nothing today; becomes load-bearing when one cluster
   fills while under its database count, and more so because placement is a one-way door.

5. **Turn the behaviour claims into a test target.** BUILT:
   `crates/zeroship-data-orm/tests/postgres_tenant_fence.rs`, an arm per claim, in the ordinary
   `cargo xtask test data` suite. Each arm owns a throwaway container through the repository's
   PostgreSQL fixture and asserts `server_version_num` against the deploy pin before it measures
   anything, because `inherit_option` does not exist below 16 and an arm that ran on an older
   server would report that this design has no fence at all.

   The two shape requirements are met rather than merely stated. **Every arm carries its control**:
   the inherit case is three grant shapes of which two must SUCCEED, and the fenced login must
   still reach the database by assuming its binding, so nothing here passes over a fixture that
   granted nothing. And **the SQLSTATE arm runs as a real non-superuser**, with the same two
   statements repeated as a superuser to watch the split disappear.

   A throwaway container is not a convenience: `pg_authid` and `pg_auth_members` are shared across
   a cluster, so role DDL against a shared instance creates roles every database on it can see.

   The version floor and the per-datastore posture below are the other two thirds of this answer;
   the catalog-cost claims are deliberately not in the set, for the reason given in Why it is this
   way.

6. **Prove `compio-postgres` surfaces `42501` distinguishably from the multi-statement setup batch.**
   BUILT, as `the_driver_reports_the_setup_batch_s_first_failure_by_sqlstate` in the same target.
   The taxonomy splits `GRANT_REVOKED` from `SCHEMA_EPOCH_STALE` on the code, and the arm sends
   the setup batch's own shape inside an explicit transaction for both failures, requiring the
   server's own SQLSTATE and message from the FIRST statement and an aborted transaction after it.
   Its control is the same batch under an assumable role, with every setting it applied read back.

7. **Re-prove the pooled-connection reset against a narrowed role.** BUILDABLE.
    `crates/zeroship-data-orm/src/exec.rs` runs the role and timeout guards via `SET LOCAL` inside an
    explicit transaction so they revert at COMMIT and at the implicit ROLLBACK on drop. That
    reasoning does not change when the role names a database, but the residue it prevents does: a
    leaked role today is one app's own schema and under sharing it is a co-tenant's. Needs a test
    that checks out, narrows, cancels mid-flight, and asserts the next checkout cannot reach the
    first database.

8. **Write the mandatory regression tests.** BUILDABLE. Nothing today fails if the fence is
    refactored away, because nothing today can be revoked. (a) Bind, query succeeds; revoke from a
    separate connection; the same warm isolate on the same pooled connection fails with
    `GRANT_REVOKED`, no eviction, no restart. (b) A statement issued without the session-setup batch
    fails with `permission denied`, proving the `WITH INHERIT FALSE` posture rather than the presence
    of a call. (c) Rotate the epoch, reap `E-1`, and assert the stale isolate gets
    `SCHEMA_EPOCH_STALE`, with the read before the rotation and a binding resolved at `E+1` as its
    controls. (d) An app bound to two databases: a
    dispatch against the second inside a transaction on the first is refused rather than run on the
    first's connection. (e) Two databases each declaring `users`: a change event on one is never
    delivered to a subscription on the other. (f) A deploy naming a database the app holds no active
    binding to is refused, and the refusal names the binding command. (g) A migration applied to a
    shared database does not make any bound app's deploy fail.

9. **Make the project config plural without losing cross-target protection.** NEEDS-DECISION.
    `schema/project-v1.json` declares `app` as a single string, and the environments block requires
    `app` and `control` and makes them explicitly non-inheritable, because an environment that names
    a control and inherits the root app is exactly the silent cross-targeting that rule prevents.
    The creator surface above replaces that single key with a workspace-level `databases` map and an
    `apps` map, so the schema has to follow.

    **The rule that has to survive is the non-inheritance, and it extends to the database ids rather
    than being satisfied by them.** A `dbs_` in the workspace `databases` map is an identifier of a
    real database on a real cluster, exactly as an `app_` is. An environment that names a production
    control and inherits a development database id is the same cross-target the existing rule
    catches, one level worse: it lands writes in the wrong data rather than the wrong code. So the
    environments block must require `databases` alongside `apps` and `control`, all three
    non-inheritable, with everything else still inheriting per member.

    Two things this does NOT change. The label stays local: an environment overrides the `id` under
    a label, never the label itself, so the manifest and the generated client are the same artifact
    across environments and no code path branches on the environment name. And declaring a database
    here still grants nothing - deploy verifies an active binding exists, which is the arm named in
    the regression set.

10. **Does the CONFINED ceiling need a grant-authority key?** NEEDS-DECISION, blocking nothing now.
    Nothing in the ceiling vocabulary describes ACL authorship, so a creator draft cannot widen an
    ACL today by absence rather than by rule. It becomes load-bearing the moment any `access.*` key
    is granted to a creator draft.

11. **Worker-side schema compatibility check.** DEFERRED, named. Deleting the deploy gate's
    descriptor comparison leaves a build expecting a column the database lacks to fail at query time
    with `42703 undefined_column`. The check that moves it earlier must be a SUBSET test, never
    equality: refuse when the database lacks something the app requires, say nothing when it has
    grown things the app does not use. Equality is what coupled every app on a database to every
    other, and reintroducing it anywhere reintroduces that. Only a process holding both the
    descriptor and a connection can evaluate a subset, so this belongs at isolate build in the
    worker, as one catalog read producing a typed refusal that names the missing collection or
    column. It is a better error than the gate ever produced and it costs nothing at deploy.

12. **Where are zones beyond the default declared?** NEEDS-DECISION, and it is the one piece of
    deployment data with no home. Datastores register themselves because reaching a cluster proves
    it exists; a zone proves nothing and gates which join signers may mint workers, so it must stay
    declared. But the corpus seeds exactly one, and a deployment with three zones cannot say so
    without patching it - which is the self-hostability problem this design otherwise avoids.
    Blocks a multi-zone self-host, nothing else.

    **The join-signer file cannot be the answer as it stands, and the reason is a guard worth
    keeping.** `resolve_zones` in `crates/zeroship-control/src/worker_join.rs` reads each zone name
    the file uses out of `zeroship.execution_zones` and refuses the whole import when a name is not
    there, rolling back rather than importing the rest. So that file is today VALIDATED AGAINST the
    zone set; making it the authority for that set inverts the direction and deletes the refusal
    that catches a misspelt zone before a signer can mint for one nobody declared.

    The shape that keeps both properties is an explicit `zones` block in the same file, imported
    insert-only BEFORE the signers are resolved, with signer entries still resolving their names
    against the table. A zone then has one declaration site, a typo inside a signer entry still
    refuses because it is absent from the block, and nothing new is introduced: the file is already
    control config (`control.join_signers_file`), which is deployment data rather than source. The
    corpus keeps seeding `default`, so a single-host deployment still declares nothing.
---

## Do-not notes

Each records something that was tried or specified and broke.

- **Do not reuse `grt` or `zeroship.grants` for the data-access edge.** Both name the auth grant,
  one row per (person, audience). Two unrelated entities in one namespace is exactly what the prefix
  doc says the disjointness rule exists to prevent.

- **Do not put a cluster's DSN, or a reference to one, on a `datastores` row.** An earlier revision
  carried an `admin_secret_ref` and an operator document to populate it. Both are unnecessary: the
  service that can reach a cluster already holds its credential, and reaching it is the only proof
  of existence anything needs. A secret reference on the row adds a second place for the credential
  to be wrong and makes the control database a step closer to being worth compromising.

- **Do not reach for a new operator document when a service config already carries the fact.** The
  same revision introduced a topology file with five import rules and a runbook to declare clusters
  that were already named by `provision_database_url`. Self-hostability forbids the migration
  corpus; it does not require a document. Ask first whether the fact is already declared somewhere
  that is not source.

- **Do not put deployment data in the migration corpus.** An earlier revision of this design declared
  datastores in a migration, on the reasoning that it reuses machinery that already runs and keeps
  topology reviewed. Both are true and both are beside the point: this platform is self-hostable, and
  a migration is not overridable. A self-hoster naming their own clusters would have to fork an
  ordered, journalled, verified corpus and merge against it forever, while upstream shipped a
  stranger's hardware inventory as product. A product DEFAULT is a different thing and is fine where
  it is: `db/migrations-ts/20260914000450_execution_zones_default_zone.ts` seeds the single-host zone
  in the `data()` phase the corpus supports for exactly that, and it stays.

- **Do not gate a deploy on schema EQUALITY.** An earlier revision kept the descriptor-hash
  comparison and re-keyed it onto the database. A hash comparison is an equality test, so any
  migration - a purely additive one included - invalidates the build of every app bound to that
  database, and all of them stop being deployable while all of them keep running correctly. That
  makes every app on a database move together, which is the coupling this design exists to remove. If
  the check is ever reintroduced it must be a subset test evaluated by something that can see the
  schema, never a hash compared by something that cannot.

- **Do not treat `databases.schema_epoch` as a record of the schema.** It is an input to a role name
  and nothing else. Reading it as a description of shape puts the deleted deploy gate back under
  another name.

- **Do not put the workflow journal back in a creator schema.** It was there, and it gave the
  tenant unrevokable ownership of state the platform executes against, made a database drop
  destroy every bound app's runs, and left this design's column grants unable to cover it. Those
  are three separate defects with one cause. The migration journal staying creator-owned is not a
  precedent for it: corrupting that one breaks only the creator, which is why
  `docs/proposals/2026-08-28-migration-record-consolidation.md` accepts it there and why it does
  not transfer here.

- **Do not reintroduce a control-side record of what migrations ran.** The engine journal in the
  creator's own schema is the only record. A creator can destroy their own journal, and that is
  accepted: it is their database and corrupting it breaks only them. The platform's answer is to hold
  no dependency on it, not to keep a shadow copy whose only production reader was a comparison this
  design deletes.

- **Do not let deploy create or reconcile bindings.** Deploy verifies that an active binding exists
  and refuses naming the command that makes one. A deploy that reconciled bindings from the manifest
  would silently restore access somebody revoked, because the config file is a stale snapshot of an
  intent that has since changed.

- **Do not put the binding, the database id or the datastore handle in the worker-internal
  `env_vars` map.** An earlier draft prescribed it "on the same path as `APP_ID`", defended on the
  ground that creator `vars` cannot shadow a worker-internal entry. That is true and answers the
  wrong question: shadowing is a forgery concern and the requirement is disclosure. Two apps under
  one actor that read equal datastore handles have confirmed co-residency.

- **Do not interpose a per-app hub role between the worker and the database role.** Measured: the
  closure is only empty when no app on that worker holds the role, so two hops buy nothing one hop
  does not. Transitivity is why, not a workaround for it.

- **Do not use `ALTER ROLE <login> NOINHERIT` in place of `WITH INHERIT FALSE`.** The role attribute
  does nothing to an existing membership.

- **Do not `DROP ROLE` on revoke.** A dropped role yields `22023 role does not exist`; a revoked one
  yields `42501 permission denied to set role`. The error taxonomy rests on that split, and
  re-granting restores service on the same warm connection. `DROP ROLE` belongs to exactly two
  places: the epoch reaper and database teardown.

- **Do not bundle the `E-1` reap into the rotation transaction.** Bundled, a failed reap means N DDL
  transactions committed, the schema advanced, the epoch stuck at `E`, and every retry failing on the
  same `DROP ROLE` forever - worse than the leak it guards. At the front the identical refusal costs a
  clean 409 with zero side effects. The failure is real: a binding role that has been granted a
  privilege OF ITS OWN cannot be dropped, which is exactly the violation of "no privileges of its
  own" this design forbids.

- **Do not key the rotation on "did this run apply anything".** A retry after a crash between the last
  DDL and T4 finds every version already applied and must still rotate.

- **Do not ask the engine for one transaction covering the DDL.** The engine commits per IR file and
  its crash recovery is journal-driven on that basis.

- **Do not put any statement before `SET LOCAL ROLE` in the session-setup batch.** The simple-query
  batch aborts at the first failure and emits one `ErrorResponse`, so an earlier failure masks the
  role error and takes the epoch fence with it.

- **Do not describe granting as one control-plane transaction.** Control's database and the tenant
  cluster are different servers. Any design that claims that transaction is claiming a distributed
  transaction it does not have.

- **Do not split the reconciler by resource type.** Bootstrap, database provisioning and binding
  grants all converge control's declarations onto one cluster through one privileged connection.
  Three loops would mean three connections, three failure reports and three places for a disagreement
  with control to be resolved differently.

- **Do not make worker boot fatal on a bad datastore.** With clusters plural, boot success becomes the
  product of every cluster's health. Refusing the affected apps is the correct failure; killing the
  process refuses everyone.

- **Do not reuse the relay's publication for anything needing full fidelity.** Its column lists
  deliberately withhold plaintext columns, so it is lossy by design.

- **Do not port the deleted `__zeroship_admin` installer's statements.** It carried a `PUBLIC` write
  grant and a `GRANT USAGE ON SCHEMA` that was the reachability precondition for every public
  `EXECUTE` in it. An acceptance arm that audits routine grants while leaving `USAGE` in place is
  checking the lock and not the door.

- **Do not add a Rust epoch comparison on the SQLite arm only.** A fence that exists on one tier and
  not the other is how a divergence becomes a surprise. Whatever the dev-tier equivalent is, it
  belongs in `docs/reference/sqlite-divergences.md` - which has no grants row to sit beside, because
  that row is one this proposal itself adds and it has not been filed.

- **Do not write "publish the mask sibling, exclude the parent".** The field's own column holds the
  mask and `__zs_raw__<f>` holds the plaintext. Anything written against the inverted layout would
  publish the plaintext.

- **Do not trust a string-compared SQL test as evidence a statement executes.** Every PostgreSQL
  upsert in one subsystem was broken and survived because the upserts that EXECUTE run on SQLite
  while the PostgreSQL ones only COMPARE STRINGS, one of them asserting the broken literal.

- **Do not fold the database into a metric name.** `zeroship.billing_metrics` is keyed on `metric`
  alone and metric names are cluster-global.

- **Do not sweep `app_id -> database_id` mechanically over metering.** `db_reads`, `db_writes` and
  `db_rows_written` stay app-keyed and must be excluded by name.

- **Do not reintroduce a control-plane-internal id-bearing migration route.** An "internal" route
  invites the belief that it is a privileged plane, and the day one accepts the control key instead of
  the caller's bearer, every creator holding a `dbs_` inherits platform authority over that database.

- **Do not cite the physical schema name as a security leak.** It is public under id-addressing;
  mapping `db_<dbs>` to something readable in error text is a message-quality item and must not be
  argued as a boundary.

- **Do not let a creator-facing label reach a transaction lane key or a routing key.** Two co-resident
  apps both calling a database `main` would compare equal. Labels live in the manifest; keys use the
  database id.

- **Do not read `database_bindings_database_project_fkey` as a guard over the DATA.** It guards the
  ROW. `dbd-reconciler` measured this against a cluster: with the reconciler's teardown precondition
  mutated out, the pass runs `DROP SCHEMA db_<dbs> CASCADE` and the key then refuses the row removal
  with `23503` - over data that is already gone. The refusal arrives after the loss and reads in a
  diff exactly like protection. What protects the data is an in-process precondition re-read
  immediately before the DDL, plus the allowlist on `bind_database` that keeps a binding from
  arriving inside that window. `BINDABLE_STATUSES` is stated as what is ADMITTED rather than what is
  refused, so a value added to `databases_status_check` later is non-bindable until someone decides
  it should be; the refusing spelling named `deleting` and let `draining` through.

---

## History

Architecture: `docs/architecture/data-system.md`. Sibling designs whose decisions this one depends
on: `docs/proposals/2026-09-05-auth-foundation-redesign.md` for the audience sum that scopes end-user
subjects to the project, `docs/proposals/2026-08-28-migration-record-consolidation.md` for the rule
that the engine journal is the only record of what ran,
`docs/proposals/2026-09-19-workflow-journal-relocation.md` for the workflow journal leaving
creator schemas, which this design requires before its column grants land, and
`docs/proposals/2026-09-05-gateway-central-database-decoupling.md` for the edge's own
central-database coupling.
