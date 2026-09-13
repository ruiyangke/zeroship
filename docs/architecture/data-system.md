# The data system

How creator data is stored, reached, isolated and evolved.

**Read this first for orientation.** The contracts live elsewhere:
`docs/reference/db.md` for the creator surface, `docs/reference/migrate-op-dsl.md` for the
migration DSL, and `docs/proposals/2026-08-28-app-database-decoupling.md` for the decoupling
design in full.

## Runtime ORM

Crate responsibilities and driver contracts: `docs/architecture/data-orm.md`.

`zeroship-data-orm` exposes `Database`, `Collection`, and `EntityCollection` for
Rust callers. The worker's V8 adapter uses the same `PreparedOperation` path.

```text
Rust model -- derives / native codecs --+
                                       |
V8 values -- native capture ------------+--> PreparedOperation
                                             |
                                  protection passes + query compiler
                                             |
                                     native parameters / rows
                                             |
                                       PostgreSQL / SQLite
                                             |
                                  protected native result records
                                             |
                        +--------------------+--------------------+
                        |                                         |
                   Rust model                              V8 object / Uint8Array
```

The shared value layer preserves integers, booleans, text and binary buffers.
Rust model mapping does not require Serde. Rust collection and column metadata is
generated from the migration runtime descriptor by `schema!`. Focused `FromRow`,
`Insertable`, and `Changeset` derives check field names and logical types against
that metadata. Read projections and write inputs are independent. Typed handles
refuse metadata that differs from the host's installed field descriptor. The V8
adapter captures arguments before yielding and materializes results directly
when the runtime re-enters V8. JSON encoding belongs to JSON columns, persisted metadata, and explicit
wire contracts. The native path still allocates records and copies V8 inputs;
it does not promise allocation-free queries.

Preparation resolves the deployment's collection descriptor and captures the
request's actor, read set, and transaction route before execution can yield.
The ORM database handle is an execution context, separate from the persisted
Database entity described below.

`zeroship_data_orm::sql` owns runtime query compilation, typed predicates,
catalog metadata, and the sentinel codec used by introspection. The shared
physical schema identity lives in `zeroship_core::schema_name::SchemaName`.
The migration engine owns DDL, schema differencing, and schema changes. Runtime
code does not contain another schema emitter, and database fixtures use the
migration engine's emitter.

Writes apply declared assignment generators. Reads and writes use the masking,
encryption and result-decoding stages. Drivers bind binary values from their
native type; a text value cannot select binary binding by carrying a prefix.
Transaction statements report completion to the reducer, which rolls back a
poisoned transaction. Rust callback transactions use the same protocol as the
worker, including savepoints and cancellation cleanup. Handles returned from a
Rust transaction callback expire when that callback finishes.

PostgreSQL operations and transactions use `Pool::acquire()` to obtain an owned
`PoolConnection` from the same bounded pool. Pool handles clone shared state;
each lease keeps that state alive across callbacks. Warm-up and checkout have
a pool acquisition budget, including async hooks. A transaction keeps its
lease through settlement; withdrawal
consumes that lease with `discard()` so an uncertain session cannot be reused.
Pool shutdown interrupts pending acquisition without invalidating leases still
held by callers. The pool contract is documented in `libs/compio-postgres/README.md`.

Implementation: `crates/zeroship-data-orm/src/orm.rs`,
`crates/zeroship-data-orm/src/sql/filter.rs`, and
`crates/zeroship-data-v8/src/v8_classes/dispatch.rs`.

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

### Current identity boundary

Runtime bindings carry app identity separately from the physical schema. App identity
keys transaction lanes and metering; the schema selects SQL qualification and PostgreSQL
roles. The host supplies project encryption keys and authorized app bindings through
`crates/zeroship-data-orm/src/encryption/keys.rs`.

The independently managed Database and Grant records described below remain planned.

---

## Identity, and what a creator may see

Every entity carries a typed id: a UUIDv7 encoded as fixed-width lowercase
base36 with an entity prefix. The concrete newtypes in
`crates/zeroship-id/` enforce their own prefixes and the shared decoder
enforces the canonical body. Datastores use `ds_...`; databases use
`dbs_...`.

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

The ORM's `ConnectionIdentity` follows this discipline for database configuration:
its digest is private, and Debug output exposes neither the digest nor the DSN.
See `crates/zeroship-data-orm/src/connection/factory.rs`.

---

### Where the word "namespace" is still correct

The entity is called **Database**. "Namespace" is PostgreSQL schema jargon, and what a creator
has is a database.

**Do not sweep the word out of the code.** It is still the right word in two places, and a
half-applied rename would be worse than none:

- **Where it names PostgreSQL's own object.** Catalog reads in
  `crates/zeroship-data-orm/src/backend/postgres/pg_introspect.rs` use `pg_namespace`
  to identify the physical schema. At this layer, a PostgreSQL database is the
  datastore, and a namespace is a schema within it.
- **Where it means something else entirely.** ES module namespace imports
  (`docs/reference/vite-plugin.md`), and the runtime's `env.*` plugin namespaces - a second plugin
  claiming the `db` namespace panics by design.

The rule: **Database** is the product noun, used for the entity, the id, the config and everything
creator-facing. **namespace** stays where it refers to the PostgreSQL object or to an unrelated
concept. The previously proposed `NamespaceManager` never shipped, so there is
no live type to rename.

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
  (`sdks/bootstrap/src/install-schema.ts:952`). With several bindings, a binding named `analytics`
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

The worker's database posture permits one ambient workflow-owner membership and rejects other
inherited data roles. CDC runs in a separate relay process: the worker login must not have
`REPLICATION` or `BYPASSRLS`, and workers receive value-free invalidations from the relay.

### Runtime role and descriptor authority

The runtime role receives ordinary data privileges on every table and sequence in its bound
creator schema. Prefixes such as `__zeroship_` do not change ORM visibility, privileges or CDC
publication. The role receives schema `USAGE` without `CREATE`, and it receives no authority on
another creator schema.

The runtime descriptor remains the ORM's logical schema authority. It defines the collections,
fields and physical storage mappings that creator code can express through the compiled query
API. The ORM exposes no raw SQL surface to creator code, and its protection passes keep raw storage
columns out of ordinary reads.

Consequences:

- **The unmask audit row separates the actor from the claim that was refused.** `actor_id` and
  `actor_role` carry identity the platform accepted; `claimed_actor` carries, verbatim and
  untrusted, an actor claim the DB-3 fence stripped. They are distinct columns because
  `sanitize_app_actor` discards a claim naming a reserved system kind, and discarding it also
  erased the evidence anyone tried: a forged `kind: "auto"` audited byte-for-byte like a caller
  who sent no actor at all. Never read `claimed_actor` as identity - it is what a handler SENT,
  which is precisely why it is recorded.
- **Bounded writes narrow through the primary key.** Update, soft-delete, restore and purge select
  the immutable `id` primary key and retain `FOR UPDATE`. The compiler derives this behavior from
  the descriptor rather than from table-name conventions.

---

## Schema authority: the descriptor, and nothing else

**The runtime descriptor is the data plane's sole schema authority, and the data plane never reads
the catalog.** It is generated from the creator's migration DSL, folded at build time, shipped in
the artifact, and immutable for the isolate's life.

`crates/zeroship-data-orm/src/descriptor.rs` is the whole surface: `collection_schema` returns the
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
`autocommit_local_session_setup_sql`, `tests/fixtures/data/roles.rs` and
`:226-233`, issued as one simple query at `crates/zeroship-data-orm/src/exec.rs`). Nothing has
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

**Schema-epoch comparison exists; a live authority source is still missing.**
`crates/zeroship-data-orm/src/transaction/reducer/identity.rs` compares observed and expected
epochs and can return `Verdict::ReResolve`. In
`crates/zeroship-data-orm/src/transaction/driver.rs`, `expected_authority` supplies a
placeholder and `observation_for` echoes it. This is not a live migration fence.

**Epoch retention must be bounded and fail closed (designed).** Applying a new epoch must
retire obsolete roles before advancing, retaining the current epoch and its predecessor
for in-flight isolates.
A failed retirement must refuse advancement to prevent unbounded catalog growth.

**What is not measured, and must not be read as covered:** per-backend membership cache construction
at CONNECT time. The `SET ROLE` measurement above is on an established backend; a new backend still
pays to build its membership set. Pooling amortizes that; it does not remove it. Measure it before
the role graph ships.

### The system schema

`__zeroship_admin` **does not exist.** It was deleted on 2026-08-27 - six tables and 32
definer-rights routines - because the worker could call every one of them, and a privileged call the
worker can make is not a boundary. `tests/fixtures/data/roles.rs` records
that nothing replaced it, and `db/migrations-ts/` provisions no such schema. One live statement still
names it and therefore fails on every database: the PITR placeholder at
`crates/zeroship-data-orm/src/backend/postgres/implementation.rs`, whose own comment at `:839-844` says so.

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

The deployed runtime still uses app schemas in a shared PostgreSQL database.
Each app publication includes every top-level table in the bound app schema;
table names do not alter CDC visibility. The CDC protocol uses the actual app
schema rather than inventing grants or epochs.

`zeroship-data-cdc-server` owns logical decoding in a separate process. Workers
connect through the authenticated TLS client in `zeroship-data-orm::cdc::relay`.
The relay shares an app capture across worker connections, buffers changes until
commit, and sends collection invalidations without row values. Workers re-read
through the ORM's ordinary access controls. The worker login has neither
`REPLICATION` nor `BYPASSRLS`; the relay uses its own constrained login.

`zeroship-data-orm::cdc` owns subscription matching, readiness, cancellation and
the process broker. `zeroship-data-v8` supplies JavaScript subscription wrappers
and isolate cleanup. The driver supplies SQL sessions and wire decoding without
owning application subscriptions. SQLite captures committed changes in its
file-backed session and publishes into the same broker locally.

```text
PG WAL --> relay service -- TLS --> ORM broker --> Rust / V8 subscriptions
SQLite commit capture -----------> ORM broker
```

Relay admission and delivery queues are bounded. Overflow, truncate and
reconnection request a fresh snapshot rather than pretending to replay a durable
event log. The final connected subscriber stops capture and releases the app's
slot. A database advisory lock fences the relay singleton, while PostgreSQL's
finite `max_slot_wal_keep_size` bounds retention after a process crash.

Slots consume cluster resources even though each logical slot decodes only its
own database. Size slot, WAL-sender and connection budgets across relay captures
and other replication users. Worker scaling adds transport connections instead
of duplicate slots for the same app. See `docs/runbooks/cdc-relay.md` for
configuration and the coordinated role cutover.

## The reserved-column collision the database will not refuse

`REPLICA IDENTITY FULL` and a publication column list are individually valid and jointly broken.
PostgreSQL accepts both DDL steps in either order and then fails at **DML** time with "column list
used by the publication does not cover the replica identity", which silently makes the table
append-only. The migration service must refuse the combination at authoring, because the database
will not. This is a forced refusal rule rather than a design choice: there is no configuration in
which the pair is correct, and the failure surfaces arbitrarily later, on a write rather than on the
DDL that caused it.

## Physical names are opaque, not descriptive

The database id is internal (above), and every exposure channel must preserve that
boundary. A creator migration can persist `currentUser()` into an ordinary column
it then reads back; the runtime copies the worker's `env_vars` map into
`process.env`. A deterministic database-configuration digest would also let
creators compare values and learn they are co-tenants if exposed. The ORM keeps
its connection digest private and formats its identity as opaque.

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

## Operational consequences

CDC carries invalidation metadata without row values. A subscriber re-reads through the ORM, where
the descriptor and protection passes shape the result. PostgreSQL role memberships remain
cluster-shared catalog state, so datastore capacity planning must include the tenant role graph and
connection authentication caches.

**One scope limit ships with it:** `readwrite`/`readonly` grants are restricted to apps under the
same creator - which the workspace model satisfies by construction. Cross-creator sharing is blocked
on a real gap: the unmask authorization policy is authored by the *reading* app, so a co-grant
holder could ship a permissive policy in its own bundle and unmask the owner's classified data. No
server-side mechanism reaches that today.
