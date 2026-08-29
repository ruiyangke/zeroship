# The data system

How creator data is stored, reached, isolated and evolved.

**Read this first for orientation.** The contracts live elsewhere:
`docs/reference/db.md` for the creator surface, `docs/reference/migrate-op-dsl.md` for the
migration DSL, and `docs/proposals/2026-08-28-app-database-decoupling.md` for the decoupling
design in full.

**Two things here are DESIGNED AND NOT BUILT**, marked *(designed)* throughout: the
Datastore/Database/Grant entities, and the provisioning service. Everything else describes code in
the tree. The distinction matters - a reader who cannot tell them apart will look for a `Database`
row that does not exist yet.

---

## The shape

```
PostgreSQL server
  └── Datastore    ds_...    one physical database on it       operator-owned   (designed)
        └── Database dbs_...  one schema inside it             creator-owned    (designed)
              └── tables, and the app's data

app  --grant-->  database        one app sees exactly ONE database
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
(`dbs`, not `db`: the shape is `^[a-z]{3}_[A-Za-z0-9]{22}$` and two letters will not parse.)

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

## Provisioning: databases are created, never conjured *(designed)*

**A dedicated service owns database lifecycle**, on the D1-to-Workers model: create a database,
then bind an app to it. `zeroship deploy` does **not** bring a schema into existence.

This belongs to the decoupling rather than sitting beside it. As long as a deploy can create a
database, app identity and database identity are still welded together at the moment that matters
most - the moment of creation.

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
  (`sdks/bootstrap/src/install-schema.ts:1508`). With several bindings, a binding named `analytics`
  and a collection named `analytics` would be the same key.
- Database resolution stays `f(app_id)` rather than becoming `f(app_id, binding_name)` with the
  binding coming from creator code. A whole class of mismatched-pair bug - right role, wrong schema
  - never comes into existence. A security property absent by construction beats one bounded by an
  argument.

---

## Isolation: the database enforces it, not the process

Enforcement is **PostgreSQL role membership**. The worker connects once, holds no inherited
privilege over app data, and narrows per transaction with a single
`SET LOCAL ROLE "zs_db_<dbsid>_<cap>"`.

This follows the platform invariant that **privilege follows the process, not the function**: the
worker executes creator code, so any capability the worker holds is reachable by whatever reaches
the worker. A privileged call the worker can make is not a boundary. What the worker may do must be
what the tenant may do.

Two exceptions exist today and are deliberate, not oversights:

1. The worker holds `zeroship_workflow_owner` by a plain grant, boot *requires* that membership
   (`crates/zeroship-worker/src/db_posture.rs`), and the fence exempts it by name.
2. **The replication plane is not fenced at all.** Logical decoding consults no column ACL and no
   RLS - a role denied `SELECT ssn` still receives the plaintext in the decoded stream. The boot
   posture requires `REPLICATION` and `BYPASSRLS` on that same login.

### Column-level grants are the masking authority

A column a grant withholds is unreadable at the database, not merely absent from a descriptor.
Two consequences measured on live PostgreSQL:

- **Blanket table grants must go.** A table-level grant subsumes any column list, so column-level
  access control is impossible while `GRANT ... ON ALL TABLES` and the prospective
  `ALTER DEFAULT PRIVILEGES` rules stand. The default-privileges half is the easy one to miss:
  revoking existing grants leaves the standing rule, and every table created afterwards is
  re-granted in full.
- **`ctid` narrowing breaks four write verbs.** The single-row verbs narrow with
  `WHERE ctid = (SELECT ctid ...)`, and `ctid` is a system column that column-level `SELECT` does
  not cover. Narrowing on the primary key instead works - measured - and every collection carries
  `id TEXT PRIMARY KEY` by construction.

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

---

## Change streams

One publication per database, membership excluding the reserved `__zeroship_` namespace so platform
journals never enter the worker-visible WAL feed.

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

## What this costs

Two costs remain, and both are PostgreSQL constraints rather than choices:

1. **Classified columns lose plaintext reactivity for everyone, including the owner.** One published
   column set per table per decode stream is a database constraint. Withhold a column from one
   reader and it is withheld from the change stream for all of them.
2. **Every apply must regenerate explicit per-column grants inside the DDL transaction.** A
   migration that fails to do so leaves the database *unreadable* rather than *over-readable* - the
   right failure direction, but a new way for a deploy to break.

Two costs that earlier drafts carried have been **withdrawn**: `env.db.users` survives (one database
per app removed the binding level), and owner-migration-breaks-co-tenant was never a cost once the
creator, not an app, owns the schema.

**One scope limit ships with it:** `readwrite`/`readonly` grants are restricted to apps under the
same creator - which the workspace model satisfies by construction. Cross-creator sharing is blocked
on a real gap: the unmask authorization policy is authored by the *reading* app, so a co-grant
holder could ship a permissive policy in its own bundle and unmask the owner's classified data. No
server-side mechanism reaches that today.
