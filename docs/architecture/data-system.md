# The data system

How creator data is stored, reached, isolated and evolved.

**Read this first for orientation.** The contracts live elsewhere:
`docs/reference/db.md` for the creator surface, `docs/reference/migrate-op-dsl.md` for the
migration DSL, and `docs/proposals/2026-08-28-app-database-decoupling.md` for the decoupling
design in full.

**What is DESIGNED AND NOT BUILT is marked *(designed)* throughout**: the Datastore/Database/Grant
entities, datastore placement, the per-grant role graph, the schema epoch's producer, and the
`__zeroship_admin` system schema. Everything else describes code in the tree. The distinction
matters - a reader who cannot tell them apart will look for a `Database` row that does not exist
yet, or rebuild something that does.


---

## The shape

```
PostgreSQL server
  |- Datastore    ds_...     one physical database on it      operator-owned   (designed)
     |- Database  dbs_...    one schema inside it             creator-owned    (designed)
        |- tables, and the app's data

app  --grant--  database        one app sees exactly ONE database
                                many apps may share ONE database
```

Three ideas, and the whole design is the consequence of separating them:

- A **Datastore** is *where* data lives. Physical placement - capacity, region, blast radius. An
  operational decision, never a creator's.
- A **Database** is *what a creator has*. Physically a PostgreSQL schema, but a creator never needs
  that word: it is their database, it has their tables, they migrate it.
- A **Grant** binds an app to a database with a capability. This is the edge that used to be an
  equality.

### What it replaces

Today an app id **is** the schema name, the role name, the encryption salt and the publication key -
one string playing five parts. `crates/zeroship-plugin-db/src/broker.rs:78-79` says so plainly:
"`schema` is conflated with `app_id` (every app has its own schema named after `app_id`)."

That conflation is why an app cannot have a database that outlives it, and why two apps cannot
share one. Giving the database its own identity is the whole change; everything below follows.

---

## Identity, and what a creator may see

Every entity carries a typed id - UUIDv7, base62, three-letter prefix
(`crates/zeroship-core/src/typed_id.rs`). `ds_...` for a Datastore, `dbs_...` for a Database.
(`dbs`, not `db`, for consistency: every prefix in `typed_id.rs` is three letters and its doc
comments state the shape `^[a-z]{3}_[A-Za-z0-9]{22}$`. Note that shape is a CONVENTION, not a
parser constraint - `parse` is `split_once('_')` plus a base62 decode of the remainder
(`crates/zeroship-core/src/typed_id.rs:139-145`), so `db_<22 chars>` would parse fine. Choose `dbs`
because the tree is
uniform, not because the parser refuses two letters.)

**Both ids are internal. Neither is exposed to creators.**

- A creator addresses a database by its **workspace-local name** - the key in the `databases` map
  of `zeroship.jsonc`. The control plane resolves `(workspace, name) -> dbs_...`.
- The **datastore id is stricter still**, because it names a *shared* resource. Two creators who
  could each read `ds_7n42...` would learn they are co-resident - a fact about someone else's
  deployment. It is a co-tenancy oracle, and it makes noisy-neighbour and resource-exhaustion
  attacks aimable rather than speculative.

What creators legitimately need are **attributes of placement, never its identity**: the region
their data sits in, and whether they are on shared or dedicated infrastructure. Both are actionable
- choose a region, buy a tier - without naming the resource or revealing who else is on it.

Two leak paths are easy to miss and both are real:

1. **PostgreSQL puts the physical schema name in its own error text.** The schema is `db_<dbsid>`,
   so a constraint violation or permission denial surfaces the id from the server itself. Anything
   relaying a database error to a creator must map it back to the workspace-local name.
2. **A URL that takes the id is an exposure.** The creator-facing apply route addresses a database
   by name; the id-bearing route is control-plane-internal. Authorization is still evaluated on the
   *resolved* database - a workspace-local label must never be the thing a permission is checked
   against.

This is the discipline `DbResourceKey` already applies to DSN passwords: a digest chosen so the
secret "cannot reach `Debug` or a log line"
(`crates/zeroship-plugin-db/src/service.rs:46`).

---

### Where the word "namespace" is still correct

The entity is called **Database**. "Namespace" is PostgreSQL schema jargon, and what a creator
has is a database.

**Do not sweep the word out of the code.** It is still the right word in two places, and a
half-applied rename would be worse than none:

- **Where it names PostgreSQL's own object.** `crates/zeroship-plugin-db/src/drop_namespace.rs`
  sequences "the PG teardown order for deleting an app";
  `crates/zeroship-plugin-db/src/replication.rs:877-878` scopes a cluster-wide scan to "the calling
  app's namespace". Both mean the PostgreSQL schema, and PostgreSQL calls it a namespace
  (`pg_namespace`). Renaming those to "database" would make them say the wrong thing, because at
  that layer a database is the Datastore.
- **Where it means something else entirely.** ES module namespace imports
  (`docs/reference/vite-plugin.md`), and the runtime's `env.*` plugin namespaces - a second plugin
  claiming the `db` namespace panics by design.

The rule: **Database** is the product noun, used for the entity, the id, the config and everything
creator-facing. **namespace** stays where it refers to the PostgreSQL object or to an unrelated
concept. `NamespaceManager` from the archived `docs/archive/db-system-design.md` never shipped, so
there is no live type to rename.

## Ownership: the creator owns the schema, no app does

**The creator owns a database and authors its schema.** No app holds DDL authority. An app's grant
says only what DML it may do:

- `readwrite` - DML on granted columns. No DDL.
- `readonly` - SELECT on granted columns.

There is no `owner` capability an app can hold, and that removes a class of problem rather than
solving one: no ownership transfer, no ping-pong between apps, and no question of what happens to a
database when the app that "owned" it is deleted. The database outlives every app that binds to it,
which is the point of giving it an identity.

The migrator role is `zs_db_<dbsid>_mig` - named by the database, by no app, one migrator forever.

### The monorepo model

**One `zeroship.jsonc` for the workspace.** Databases are declared once; apps bind to them by name.

```jsonc
"databases": {
  "main":      { "migrations": "./db/main",      "out": "./generated/zeroship/main" },
  "analytics": { "migrations": "./db/analytics", "out": "./generated/zeroship/analytics" }
},
"apps": {
  "storefront": { "app": "<uuid>", "database": "main"      },
  "admin":      { "app": "<uuid>", "database": "main"      },   // shares with storefront
  "reporting":  { "app": "<uuid>", "database": "analytics" }
}
```

Declaring each database **once** is what makes shared generated types structural rather than
conventional: one migration source and one `out` directory per database, and nothing can drift two
apps onto different types from the same migrations.

**The creator may make breaking changes and owns the risk.** A schema change means *rebuild the
workspace* - the same mechanics as changing a shared library. The deploy gate still exists, because
an app built against schema v1 must not be **served** against v2 or it reads columns that moved,
but that is correctness and the remedy is an ordinary rebuild.

*(This shape requires the project schema to allow several apps per file. Today
`schema/project-v1.json:30-34` declares `app` as a single string, and environments make `app`
non-inheritable specifically to prevent silent cross-targeting (`:66-67`). Making `app` plural must
preserve that property.)*

---

## Provisioning: databases are created, never conjured

**A dedicated service owns database lifecycle**, on the D1-to-Workers model: create a database,
then bind an app to it. `zeroship deploy` does **not** bring a schema into existence.

This belongs to the decoupling rather than sitting beside it. As long as a deploy can create a
database, app identity and database identity are still welded together at the moment that matters
most - the moment of creation.

**The service already exists; it runs at the wrong time.** The migration service already issues
every statement provisioning needs, from a process that already does not execute creator code -
which is the boundary that matters, and the reason provisioning can never live in the worker:
`CREATE ROLE` at `crates/zeroship-migrate-server/src/provisioning.rs:121`, `ALTER SCHEMA ... OWNER`
at `:143`, and the workflow journal's own `CREATE SCHEMA ... AUTHORIZATION` at `:232`.

What is wrong is the trigger. Provisioning is a **side effect of applying a migration**. The apply
path takes `let schema = app_id.to_string()`
(`crates/zeroship-migrate-server/src/apply.rs:257`) and issues `CREATE SCHEMA IF NOT EXISTS` over it
before any migration runs (`:268-271`); the journal schema follows from the same path, keyed
`format!("app_{schema}")` (`:1178`). That is the conflation this design removes, in executable
form - **a database exists because an app deployed.**

So the work is inversion, not construction:

```
  TODAY     deploy -> apply migration -> (side effect) CREATE SCHEMA app_<app_id>
  DESIGNED  create database -> CREATE SCHEMA db_<dbsid>      explicit, first-class
            bind app -> grant
            deploy -> apply migration -> FAILS if the database does not exist
```

Genuinely new, as opposed to re-keyed: **placement**. Choosing which Datastore does not exist at
all - the service holds one DSN, not a set - and neither does the grant table. That is where the
new surface is.

A fourth privileged service would be the wrong answer: it would hold datastore credentials and do
DDL, which is what this one already does, with the policy machinery, apply lock and journal already
built. "Dedicated" here means owning the lifecycle, not running a separate binary - Cloudflare
provisions D1 through its control plane, not a fourth process.

The control plane places a new Database on a Datastore. Bring-your-own-datastore is out of scope:
"which physical database may an app reach" is privileged, and exposing it would hand a DSN to the
app plane.

---

## What an app sees

**Exactly one database**, so the creator surface keeps its present shape:

```ts
await env.db.users.find({ where: { active: true } });
await env.db.transaction(async (tx) => { ... });
```

There is no binding level between `env.db` and a collection. That is a deliberate consequence of
one-database-per-app, and it is load-bearing in two ways:

- `installSchema` plants collections directly on the target with `Object.defineProperty`
  (`sdks/bootstrap/src/install-schema.ts:1509`). With several bindings, a binding named `analytics`
  and a collection named `analytics` would be the same key.
- Database resolution stays `f(app_id)` rather than becoming `f(app_id, binding_name)` with the
  binding coming from creator code. A whole class of mismatched-pair bug - right role, wrong schema
  - never comes into existence. A security property absent by construction beats one bounded by an
  argument.

---

## Isolation: the database enforces it, not the process

Enforcement is **PostgreSQL role membership**. The worker connects once, holds no inherited
privilege over app data, and narrows per transaction with a single `SET LOCAL ROLE`.

**The role it narrows to is per GRANT, not per database, and its name carries the schema epoch:**
`zs_bind_<gid>_e<E>`. Two independent properties ride on that one string.

**Revocation** is why the role is per grant. A role per database - `zs_db_<dbsid>_<cap>`, with apps
made members of it - is measured in the proposal, under "Why the role is per grant and not per
database", as **unrevocable under co-tenancy**:
`SET ROLE` authorizes against the transitive closure of the *login* role's memberships, so with two
apps holding the database role, revoking one leaves the closure non-empty and the other app's
membership still serves the first. The shape that works holds
`GRANT zs_db_<dbsid>_<cap> TO zs_bind_<gid>_e<E> WITH SET FALSE` - the `WITH SET FALSE` being what
stops the worker assuming the database role directly and bypassing the per-grant edge. Revoking one
grant then removes exactly one app's access, at the next transaction, on the same warm connection.

**The schema epoch** is why the name carries `_e<E>`. See below.

This follows the platform invariant that **privilege follows the process, not the function**: the
worker executes creator code, so any capability the worker holds is reachable by whatever reaches
the worker. A privileged call the worker can make is not a boundary. What the worker may do must be
what the tenant may do.

Two exceptions exist today and are deliberate, not oversights:

1. The worker holds `zeroship_workflow_owner` by a plain grant, boot *requires* that membership
   (`crates/zeroship-worker/src/db_posture.rs:101-103`), and the fence exempts it by name
   (`AMBIENT_MEMBERSHIP_EXEMPTION`, `crates/zeroship-worker/src/db_posture.rs:13`).
2. **The replication plane is not fenced at all.** Logical decoding consults no column ACL and no
   RLS - a role denied `SELECT ssn` still receives the plaintext in the decoded stream. The boot
   posture requires `REPLICATION` and `BYPASSRLS` on that same login
   (`crates/zeroship-worker/src/db_posture.rs:96-100`).

### Column-level grants are the masking authority

A column a grant withholds is unreadable at the database, not merely absent from a descriptor.
Two consequences measured on live PostgreSQL:

- **The runtime role receives no blanket table grants.** A table-level grant subsumes any column
  list, so neither production provisioning nor the plugin-db test provisioner grants DML on all
  tables or installs prospective table default privileges. Bindings grant their columns
  explicitly. The sole reserved-table exception is `__zeroship_audit_unmask`: the runtime role
  receives table `INSERT` plus `USAGE` on its owned serial sequence, and nothing else.
- **Bounded writes narrow through the primary key.** PostgreSQL refuses `SELECT ctid` with 42501
  under column-scoped SELECT, which made update, soft-delete and restore unusable and also blocked
  purge once its separately required table DELETE privilege was present. Those paths now select
  the immutable, readable `id TEXT PRIMARY KEY` and retain `FOR UPDATE`; live tests execute all four
  shipped builders and the replacement data-plan's bounded update/delete with column-scoped read
  authority. PostgreSQL has no column-level DELETE privilege, so readwrite bindings necessarily
  grant DELETE at table scope; it does not confer SELECT on any column.

---

## Schema authority: the descriptor, and nothing else

**The runtime descriptor is the data plane's sole schema authority, and the data plane never reads
the catalog.** It is generated from the creator's migration DSL, folded at build time, shipped in
the artifact, and immutable for the isolate's life.

`crates/zeroship-plugin-db/src/descriptor.rs` is the whole surface: `collection_schema` returns the
field map or a typed error, and **there is no third state**. An absent schema used to mean "carry
on", which is how the read path came to fail open.

Masked fields follow the storage flip, which has shipped: **the field's own column holds the mask**
and `__zs_raw__<field>` holds the plaintext. Adding `.mask()` to a column that already holds data is
a real engine backfill for unencrypted columns; the encrypted case is refused by decision, because
the backfill is structured SQL and the engine holds no key material.

### The schema epoch, and why PostgreSQL enforces it rather than Rust

A descriptor is the shape an isolate was **built** against. The epoch is how the database says which
shape it currently **has**, so an isolate holding a descriptor from before an apply is refused rather
than served the wrong columns.

**The epoch is a substring of the role name, so it is enforced by PostgreSQL.** The per-grant role
is `zs_bind_<gid>_e<E>`; an apply that advances the epoch mints the roles for `E+1` and drops those
for `E-1`. An isolate carrying a stale epoch therefore fails at `SET LOCAL ROLE`, which is the
**first statement of the setup batch that already exists** (`tx_session_setup_sql` and
`autocommit_local_session_setup_sql`, `crates/zeroship-plugin-db/src/auth/bootstrap.rs:204-212` and
`:226-233`, issued as one simple query at `crates/zeroship-plugin-db/src/exec.rs:322`). Nothing has
to remember to check: the batch is the only route to a usable connection, and a stale epoch never
gets one.

The steady-state cost is **zero**. The epoch rides a string the batch already sends.

**The rejected alternative was to append `SELECT epoch ...` to the setup batch and compare in Rust.**
It costs one index lookup inside an existing round trip, which is cheap, and it puts the fence in a
statement the worker *issues* rather than a condition it *fails*. Its author nominated two objections
that would have sunk the role-name form, and both were measured away:

- **Does `CREATE ROLE` inside the apply bracket serialize applies across other databases?** Roles are
  cluster-shared (`pg_authid` and `pg_auth_members` both carry `relisshared = t`), so this was the
  live worry. Method: session 1 holds an uncommitted `CREATE ROLE` in one database, session 2 issues
  `CREATE ROLE` in another database of the same cluster with `lock_timeout = '3s'` so blocking
  surfaces as an error rather than a hang, against a control with no holder. Control and case both
  returned `CREATE ROLE`; the case took 108 ms. No cross-database serialization.
- **Is `SET ROLE` superlinear in `pg_auth_members`?** If it were, the role graph would tax every
  query on the platform. Method: grow the shared catalog, then time 2000 `SET ROLE` plus 2000
  `RESET ROLE` server-side in a plpgsql loop so client round trips are excluded and the same N runs
  at every scale. At 3, 103, 1103 and 6103 membership rows the loop measured 6 ms throughout - about
  1.5 us per statement, flat across a 2000x growth.

So the comparison form buys nothing the name does not, and costs a statement in every setup batch
forever.

**Only the producer is missing; the consumer ships.**
`crates/zeroship-plugin-db/src/transaction/reducer/identity.rs:97` defines `SchemaEpoch`, and `:313-315`
compares the observed epoch against the expected one and returns `Verdict::ReResolve` - retryable,
distinct from the terminal denials above it. What has no input is
`crates/zeroship-plugin-db/src/transaction/driver.rs:106`, which mints `SchemaEpoch::new(0)` on both
sides and says so at `:100-101`: "The wiring is real; the *input* is not yet." Building the epoch is
supplying one input to a classifier that already ships.

**Live epochs are capped at two, fail-closed.** The reaper is part of the apply, not a sweep: an
apply that cannot drop epoch `E-1`'s roles refuses to advance to `E+1`. That bounds an otherwise
unbounded leak into the cluster-shared catalog, and it is what gives an in-flight isolate one epoch
of grace rather than none.

**What is not measured, and must not be read as covered:** per-backend membership cache construction
at CONNECT time. The `SET ROLE` measurement above is on an established backend; a new backend still
pays to build its membership set. Pooling amortizes that; it does not remove it. Measure it before
the role graph ships.

### The system schema

`__zeroship_admin` **does not exist.** It was deleted on 2026-08-27 - six tables and 32
definer-rights routines - because the worker could call every one of them, and a privileged call the
worker can make is not a boundary. `crates/zeroship-plugin-db/src/auth/bootstrap.rs:15-18` records
that nothing replaced it, and `db/migrations-ts/` provisions no such schema. One live statement still
names it and therefore fails on every database: the PITR placeholder at
`crates/zeroship-plugin-db/src/backend/postgres.rs:1286`, whose own comment at `:770-776` says so.

**This work creates it**, and the shape is the invariant's one permitted use - state a separate
service writes and the worker only reads:

- **An installer**, in `db/migrations-ts/`, so the schema is a provisioned platform object rather
  than something a test helper conjures. The deleted version was `#[cfg]`-gated to tests, which is
  why deleting it cost nothing and why recreating it is genuinely new work.
- **Exactly one table**, holding the current schema epoch per database. The migration service writes
  it inside the apply transaction that mints the new epoch's roles, so the recorded epoch and the
  roles in the catalog cannot disagree. Its grant posture is write-to-the-migration-service,
  read-only-to-everyone-else - and **the data plane still reads nothing**: the role name carries the
  epoch precisely so no query has to. The control plane reads it to compose the binding it injects.
  A data-plane read of this table would reintroduce the catalog dependency the descriptor decision
  removed.
- **Zero worker-callable functions.** No `SECURITY DEFINER`, no `EXECUTE ... TO PUBLIC`, and no
  `GRANT USAGE ON SCHEMA` to an app or grant role - that `USAGE` was the reachability precondition
  for every public `EXECUTE` in the deleted version, so a checklist that audits the routine grants
  while leaving `USAGE` in place is checking the lock and not the door.
- **No `PUBLIC` write grant on anything.** The deleted schema had exactly one, on its PITR table, and
  a provisioner written by porting the old installer's statements would port it.

The name is the drawer problem the deletion diagnosed: "admin" named a collection of platform powers,
and a name that survives its contents is how the next reader concludes there is a drawer to put
things in. It is reinstated for exactly one row shape and nothing else.

---

## Change streams

One publication per database, membership excluding the reserved `__zeroship_` namespace so platform
journals never enter the worker-visible WAL feed.

**Publications and slots sit at different scopes, and the difference decides what has to be
rationed.** Measured on PostgreSQL 18.4:

- **A publication is DATASTORE-scoped.** `pg_publication` and `pg_publication_rel` both carry
  `relisshared = f`, and the same publication name created in two databases of one cluster coexists,
  each database's `pg_publication` showing only its own. One publication per database therefore
  costs nothing cluster-wide and no name can collide across datastores.
- **A replication slot is CLUSTER-scoped, and the stock ceiling is 10.** `max_replication_slots`
  defaults to 10 and is `context = postmaster`, so raising it is a restart. Ten slots created in one
  database are all visible from another database of the same cluster, and the eleventh - created
  **from that other database** - fails `SQLSTATE 53400`, "all replication slots are in use". Slot
  cardinality is a cluster budget shared by every datastore tenant, which is why the design takes one
  slot per (datastore, worker) and fans out in process rather than one slot per database.
- **`max_slot_wal_keep_size` measures as `-1`** - unbounded retention - on a stock server, so one
  abandoned slot can grow `pg_wal` until the cluster dies. It is `context = sighup`, so bounding it
  is a reload rather than a restart. The worker now refuses to boot against a cluster where it is
  unlimited (`crates/zeroship-worker/src/db_posture.rs`, landed 2026-08-29). The finite VALUE is not
  chosen here: too small and a legitimately slow consumer loses its slot and must resynchronise, too
  large and the protection is theoretical. It belongs with the CDC relay, whose lag characteristics
  set the floor.

**SLOT CARDINALITY IS ONE PER CLUSTER, VIA THE RELAY, AND THE DECOUPLING SEQUENCES BEHIND IT.**
Settled 2026-08-29 by the measurements above rather than by preference. Three cardinalities were
live across the document set - per (app, worker) in code today, per (datastore, worker) in this
design, and one per cluster in the CDC relay design - against a hard ceiling of 10 that only a
restart moves. The relay's number is the only one that fits stock configuration with headroom, and
the binding term in the other two is the WORKER count: the documented deployment scales workers to
10, so one datastore times ten workers already exhausts the cluster.

Two consequences follow, and both are load-bearing:

- **No creator-reachable operation may mint a cluster-scoped object.** Opening a subscription
  attaches to the datastore's existing stream; creating a database creates schema and publication,
  which are datastore-scoped and therefore free. Nothing a creator does may create a slot, a WAL
  sender, or WAL retention. Roles are the one deliberate exception - the enforcement model IS
  cluster-global rows - so they are quota'd rather than multiplexed.
- **A publication is not decoded unless the running stream NAMES it.** Measured: one slot, one data
  set, two decodes differing only in `publication_names` - 4 change records vs 8. PostgreSQL's own
  warning explains the mechanism: "The publication does not exist at this point in the WAL." The name
  list is fixed when the stream starts (`libs/compio-postgres/src/replication.rs` interpolates it into
  START_REPLICATION once), so a per-database publication under one shared slot strands every database
  created after the stream began, silently. That is why the relay must own the slot: adding a database
  is then a fan-out change in one process, not a stream restart every co-tenant feels.

## The reserved-column collision the database will not refuse

`REPLICA IDENTITY FULL` and a publication column list are individually valid and jointly broken.
PostgreSQL accepts both DDL steps in either order and then fails at **DML** time with "column list
used by the publication does not cover the replica identity", which silently makes the table
append-only. The migration service must refuse the combination at authoring, because the database
will not. This is a forced refusal rule rather than a design choice: there is no configuration in
which the pair is correct, and the failure surfaces arbitrarily later, on a write rather than on the
DDL that caused it.

## Physical names are opaque, not descriptive

The database id is internal (above), and that claim is only as strong as the number of channels that
can leak it. Three were found: a creator migration persisting `currentUser()` into an ordinary column
it then reads back; the worker `env_vars` map, every entry of which the runtime copies into
`process.env`; and `DbResourceKey`, a deterministic unsalted digest of the database URL that lets two
creators compare values and learn they are co-tenants.

**So the schema and role names are derived from the id by a keyed one-way function rather than
embedding it.** Settled 2026-08-29. The alternative considered was refusing `currentUser()` in
managed migrations, which closes exactly one channel and leaves the class open; opaque names make
every channel - including any found later - leak a value that means nothing. A fix that does not
require enumerating the leaks first is the correct shape when the enumeration is the part nobody can
guarantee.

**The publication column list is the only server-side fence on the decode path** - grants and RLS
are executor-side and logical decoding consults neither. That fence collides with something the
DELETE path wants: `REPLICA IDENTITY FULL`. PostgreSQL accepts both DDL steps in either order and
then fails at **DML** time with "column list used by the publication does not cover the replica
identity", which silently makes the table append-only. The migration service must refuse the
combination, because PostgreSQL will not.

---

## The dev tier

`pnpm dev` runs against SQLite, with contract parity rather than the same adversarial posture - the
developer owns the bytes on their own machine. A Postgres DSN is refused in dev on purpose.
Intentional divergences are catalogued in `docs/reference/sqlite-divergences.md`.

---

## Multi-region: NOT DESIGNED, and the shape it is forced into

There is **no region or datacenter concept anywhere in the tree** (checked 2026-08-29: zero
occurrences in the gateway or the route registry). What exists is CHWBL within one location - a
consistent hash ring with load bounded at 125% of average, overflow spilling to the next worker
(`crates/zeroship-gateway/src/proxy.rs`). This section records the constraint rather than a design,
so that whoever takes it on does not start by rediscovering it.

**CHWBL works today because workers are stateless with respect to data.** Any worker can serve any
app. A Database is not stateless: it lives on one Datastore, on one PostgreSQL server, in one
building, and PostgreSQL is not multi-master. So compute is freely balanceable and data is not.

**The arithmetic settles it.** One `find` costs four network round trips - BEGIN, the `SET LOCAL`
setup, the query, COMMIT - and a query execution measured about 555us locally
(2026-08-29, loopback, cache off):

```
  same DC           RTT ~0.1ms  ->  4 x 0.1  =   0.4 ms    query cost dominates
  cross-DC regional RTT ~10ms   ->  4 x 10   =  40   ms
  cross-continent   RTT ~70ms   ->  4 x 70   = 280   ms    per find
```

Cross-region data access is not a tuning problem. **An app must be co-located with its database**,
which makes routing DATA-BOUND rather than load-bound: you do not balance across regions, you
*place* across them and balance within. Two layers answering different questions - placement is a
slow control-plane decision fixed at provisioning; balancing stays exactly the CHWBL it is today,
over local workers only.

It also makes the round-trip count strategic rather than cosmetic. Collapsing four trips matters
far more in a multi-region world than a single-region one.

**The hook already exists:** `Datastore` carries `cluster_id`, so adding a region attribute makes
placement expressible without touching the entity model. And the rule that creators see *attributes*
of placement and never its identity already covers the creator half - they choose a region, they
never name a datastore.

**Two consequences worth stating before anyone designs this:**

- **Two apps sharing a Database are pinned to the same region.** Co-tenancy and geo-distribution
  pull against each other.
- **Read replicas elsewhere are the obvious answer and are not obviously safe.** Replication lag
  breaks the ordering the masking and deploy-gate story rests on: an app must not read a schema
  older than the descriptor it was built against. That is a design problem, not a config flag.

## What this costs

Three costs remain, and the first two are PostgreSQL constraints rather than choices:

1. **Classified columns lose plaintext reactivity for everyone, including the owner.** One published
   column set per table per decode stream is a database constraint. Withhold a column from one
   reader and it is withheld from the change stream for all of them.
2. **Every apply must regenerate explicit per-column grants inside the DDL transaction.** A
   migration that fails to do so leaves the database *unreadable* rather than *over-readable* - the
   right failure direction, but a new way for a deploy to break.
3. **Every grant and every live epoch is a row in the cluster-shared catalog.** `pg_authid` and
   `pg_auth_members` are `relisshared = t`, so the role graph is the one resource a datastore's
   tenants cannot be partitioned away from each other in. Two things bound it: `SET ROLE` measures
   flat across a 2000x growth in membership rows, and the apply-time reaper caps live epochs at two.
   What is not bounded by measurement is per-backend membership cache construction at connect time,
   which nothing has measured.

**One scope limit ships with it:** `readwrite`/`readonly` grants are restricted to apps under the
same creator - which the workspace model satisfies by construction. Cross-creator sharing is blocked
on a real gap: the unmask authorization policy is authored by the *reading* app, so a co-grant
holder could ship a permissive policy in its own bundle and unmask the owner's classified data. No
server-side mechanism reaches that today.
