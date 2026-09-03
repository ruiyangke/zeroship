# Decoupling app identity from database identity

**Status.** PROPOSED. No entity in this design exists in the tree. `Resource`
(`crates/zeroship-authz/src/resource.rs`) has only `App` and `Any`;
`crates/zeroship-migrate-server/src/apply.rs:284` still reads `let schema = app_id.to_string();`;
`ls crates/ | grep -i cdc` returns nothing. Four prerequisites HAVE landed and are relied on
below: the explicit write-verb projection and primary-key row narrowing
(`crates/zeroship-schema/src/query.rs`, live arms in
`crates/zeroship-plugin-db/tests/column_grants.rs`), the deploy-time schema precondition
(`crates/zeroship-control/src/api.rs:167`), and the edge split routing `/v1/*` to the
migration service (`deploy/ops/Caddyfile`). The stale-binding classifier is built and
reachable but tautological in production - see Open 2.

---

## What it is

An app id is a tenant. It is not a schema name, not a role name, not an encryption salt and
not a publication key. Today it is all five by string identity, which is what makes a
database that outlives its app, or one that two apps share, unrepresentable.

### Entities

```
ds_<base62 uuidv7>    Datastore  { engine, cluster_id, dsn_secret_ref, resource_key }
dbs_<base62 uuidv7>   Database   { datastore_id, owner = the CREATOR, physical schema "db_<dbsid>" }
grant  PK (app_id)               { database_id, capability, granted_to_principal, state }
                                   capability = readwrite | readonly
```

- **Datastore** - one physical PostgreSQL database (or one SQLite file directory).
  Operator-owned; creators never name one. Many Databases sit on one Datastore.
- **Database** - one schema inside a Datastore. This is the unit that is owned, migrated,
  granted, published and dropped. It replaces "the app's schema" everywhere. The Database
  id, not an app id, names the physical schema, the migrator role, the per-capability roles
  and the apply lock. The Datastore id names the shared publication and slot.
- **Grant** - the edge, keyed on `app_id` alone because an app binds to exactly one
  database. There is no `binding_name`; a binding name exists only to disambiguate among
  several databases an app can see, and an app never sees several.

Many apps may point at one database. One app pointing at several is closed by decision, not
deferred: it is what keeps `env.db.users` a collection rather than a binding, keeps database
resolution `f(app_id)`, and keeps the mismatched-pair bug class (right role, wrong schema)
out of existence rather than bounded by an argument.

The prefix is `dbs` for uniformity with `crates/zeroship-core/src/typed_id.rs`, whose every
prefix is three lowercase letters. That is convention only: `parse` is `split_once('_')` plus
a base62 decode and `parse_with_prefix` compares a string, so neither enforces length or
charset.

**A database is addressed by its id, always.** There is no `(workspace, name) -> database_id`
resolution anywhere. The creator writes the `dbs_...` id, the CLI sends it, the migration
service takes it in the URL. An id is not a capability: authorization is "principal may
migrate N iff principal owns N", evaluated on the database itself.

A DSN never leaves the control plane and the operator config. `Datastore.dsn_secret_ref`
names a platform secret. The worker is configured with a *set* of DSNs indexed by
`DbResourceKey` (`crates/zeroship-plugin-db/src/service.rs:193`), which already exists, is
already a SHA-256 digest chosen so a DSN password cannot reach `Debug` or a log line, and
needs no change - only its cardinality is wrong. Bring-your-own-datastore is out of scope:
"which physical database may an app reach" is a privileged decision.

### Enforcement: role membership, not an in-process check

```
zs_db_<dbsid>_mig     owns schema db_<dbsid>                     (replaces the per-app migrator)
zs_db_<dbsid>_rw      USAGE on db_<dbsid> + column-listed DML
zs_db_<dbsid>_ro      USAGE on db_<dbsid> + column-listed SELECT
zs_bind_<gid>_e<E>    NOLOGIN, no privileges of its own; inherits exactly ONE database role
zeroship_worker       LOGIN, member of zs_bind_<gid>_e<E>, per live (grant, epoch)
```

```
CREATE ROLE zs_bind_<gid>_e<E> NOLOGIN;
GRANT zs_db_<N>_<cap>    TO zs_bind_<gid>_e<E> WITH SET FALSE;    -- inherits, not assumable
GRANT zs_bind_<gid>_e<E> TO zeroship_worker    WITH INHERIT FALSE; -- assumable, never ambient
```

`<gid>` is the app id, because the grant table is keyed on it. `<E>` is the schema epoch.

The data plane narrows per transaction with `SET LOCAL ROLE "zs_bind_<gid>_e<E>"` - the first
statement of the setup batch that already exists
(`crates/zeroship-data-postgres/src/pg_session_sql.rs:36` and `:63`), same statement, same
batch position, no extra round trip. It replaces the current per-app role name
(`crates/zeroship-core/src/database_role.rs`) in that builder and nowhere else.

- **Granting** is the two edges above, in one control-plane transaction.
- **Revoking** is `REVOKE` on both edges, in one control-plane transaction. The role is NOT
  dropped.
- **Role count** grows to `apps + 3 x databases + grants x live epochs`, live epochs capped
  at two by the reaper. Every bind, unbind and rotation is shared-catalog DDL serialized
  through the control plane, which needs rate-limiting against grant-flapping.
- **Boot posture** gains two catalog-checkable arms on top of what
  `crates/zeroship-worker/src/db_posture.rs` already refuses: `inherit_option = false` on
  every `zs_bind_*` membership, and `pg_has_role(login, <database role>, 'SET') = false` for
  every database role. `db_posture` must run per datastore and refuse boot on any bad one; a
  partial pass would serve some apps and 500 others behind a gate that half-ran.

Two ambient exceptions survive, both deliberate. The worker holds `zeroship_workflow_owner`
(`db/migrations-ts/20260818000200_worker_database_authority.ts`), boot *requires* it and the
posture check exempts it by name (`crates/zeroship-worker/src/db_posture.rs:13`); that role
owns every app's journal schema and the workflow store never narrows
(`crates/zeroship-plugin-workflow/src/store/pg.rs`). And the replication plane is not fenced
at all: the boot posture requires `REPLICATION` and `BYPASSRLS` on the same login. So the
accurate claim is narrow: **role membership fences the SQL executor plane and nothing else.**

### Binding resolution

The control plane resolves the app's grant at deploy time and hands the worker one binding:

```
DbBinding { db: "dbs_01J...", schema: "db_01J...",
            ds: <DbResourceKey>, cap: "readwrite", epoch: 7 }
```

One binding, not a map. Nothing creator-supplied selects it. The vehicle is `DbServiceConfig`
plus `DbBinding` (`crates/zeroship-data-core/src/binding.rs`), which already carries the DSN
to the plugin without passing through V8 and is already the per-isolate identity every `Db`
and `Collection` wrapper travels with. App JS never needs these values: `env.db` methods are
native ops, so the plugin reads the binding in Rust when the op runs.

The worker does not compare `epoch` in Rust to authorize a transaction; it composes the role
name the setup batch sends. An app whose binding is absent is a hard refusal with the same
shape as `collection_not_declared` (`crates/zeroship-plugin-db/src/descriptor.rs`).

Per-thread resources become maps keyed by `DbResourceKey`. `ThreadDbContext`
(`crates/zeroship-plugin-db/src/context.rs`) holds one pool, one url, one resource key and
one backend today, and registering a second URL tears the first down. The target shape
already exists one module over as `OPERATOR_POOLS: HashMap<DbResourceKey, Rc<Pool>>`
(`crates/zeroship-plugin-db/src/service.rs:146`).

### Column-level GRANT is the masking authority

The migration service emits column-level grants from the owner's own IR, withholding every
column whose classification is not `none` and granting the column that holds the mask
instead. It already writes classification and mask kind as `COMMENT ON COLUMN` sentinels
(`crates/zeroship-schema/src/mask_codec.rs`,
`crates/zeroship-migrate-backend/src/mask_codec.rs`) and is the one process in the tree that
does not execute creator code, so this satisfies the AGENTS.md privilege invariant with no
`SECURITY DEFINER` wrapper and no system-schema state. A creator migration cannot widen the
ACL back open: the CONFINED ceiling grants exactly `schema.create_table`, `schema.rename` and
`safety.destructive_ops` and nothing else
(`crates/zeroship-migrate-server/policies/confined.policy.toml`, whole file).

Two deletions ship with it, in `runtime_role_provisioning_sql`
(`crates/zeroship-migrate-server/src/apply.rs:1384`): the blanket
`GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA` at `:1424` and the prospective
`ALTER DEFAULT PRIVILEGES` rules at `:1426-1428`. Every apply regenerates explicit per-column
grants inside the same transaction as the DDL.

The runtime descriptor is demoted from security boundary to shape declaration. That is its
correct altitude whether or not sharing ships.

### Ownership and migration

A database is owned by the **creator**, not by an app. The creator has full control including
breaking changes and takes the risk; migration is not coupled to an app. The model is a
monorepo: several apps in one workspace sharing one migration source and one set of generated
types. `Database` therefore carries the creator's ownership and no app column, and the
migrator role `zs_db_<dbsid>_mig` is named by no app. There is no `owner` capability an app
can hold, so there is no ownership transfer, no ping-pong between apps and nothing for an app
deletion to cascade into.

**One route, and the control plane is not on it.** The CLI calls the migration service
directly, addressing the database by id:

```
creator -> POST /v1/databases/{database_id}/migrations/apply    (zeroship-migrate-server)
```

The CLI reuses the `control` URL and the edge routes the path
(`deploy/ops/Caddyfile:66-67`, already shipped). No new config key, flag or environment
variable. A shared hostname is not the control plane being in the path: Caddy hands the
request to `migrate-server` directly, no control code runs, no second authorization happens.
Two costs, taken deliberately: the edge config is load-bearing (a missing rule yields
control's 404), and the control plane must never define a `/v1/*` route - enforced by a gate
arm, not a convention. `crates/zeroship-migrate-server/src/api.rs` already authorizes against
its own `ControlPlaneAuthenticator` reading the creator's own bearer, so no authorization is
lost.

`crates/zeroship-authz/src/resource.rs` gains `Resource::Database { id }` with the policy
"principal may migrate N iff principal owns N", checked against the creator with no app
indirection. That is the load-bearing change in the re-key, not the mechanical `app_id`
occurrences across the migration service.

**The apply lock moves to the database and stays SESSION-scoped.** The engine already takes a
session lock around a whole plan and releases it explicitly
(`crates/zeroship-migrate-postgres/src/backend/session.rs`,
`crates/zeroship-migrate-core/src/engine.rs`); the host acquires once on its pinned session
and passes `LockMode::AlreadyHeld` for every IR file. Only the logical key changes, from the
app-as-project to the database. The publication reconciler's own
`pg_advisory_xact_lock(hashtextextended($1, 0))`
(`crates/zeroship-migrate-server/src/publication.rs:86`) is keyed on the app-derived
publication name and is replaced, not reused.

**The apply structure.** All SUBTRACTION from the catalog happens before any DDL commits; all
ADDITION happens after every DDL has committed:

```
L    host takes a SESSION advisory lock on the database key, held to U
P    preflight: lower every IR file, refuse a denied plan
S    record_submitted on the control connection - the audit row opens
T1   one transaction: head FOR UPDATE, reap E-1 roles, claim, shrink this Database's publication members
D1..DN  the DDL, engine-journalled, every file passing LockMode::AlreadyHeld
T4   one widen transaction: mint E+1 roles, widen those members, advance the head to E+1,
     emit the marker - iff the committed schema delta requires rotation
C    mark_applied / mark_failed on the control connection
U    release the lock
```

**A serving app is never fenced.** The apply mints `E+1` and drops `E-1`; it never touches
`E`. Every partial crash state leaves the app serving on `E`. Recovery is a plain retry: the
engine journal skips completed DDL, and the head's recorded journal state decides whether the
rotation is still owed. A retry after a crash between the last DDL and T4 finds every version
applied and must STILL rotate.

**The deploy gate is scalar and its row re-keys onto the database.**
`crates/zeroship-control/src/registry.rs` predicates the deploy UPDATE on one
`descriptor_sha256` matching the newest `applied` row, and `Manifest.runtime_descriptor`
(`crates/zeroship-bundle/src/manifest.rs:172`) keeps its shape. What changes is the subquery:
it reads `WHERE m.app_id = $3` today (`crates/zeroship-control/src/registry.rs:553`), and an
apply belongs to a database. So `zeroship.app_schema_applies` keys `(database_id,
migration_id)` and the predicate resolves the app's database through its grant first. That is
also what makes binding a second app to an existing database work at all: with a
database-keyed row the second app's first deploy matches the apply that already ran; with an
app-keyed row it finds none. The bundle carries a hash, never a database id.

### CDC

- **One relay-owned slot and one pgoutput stream per Datastore.** Slots replicate decode work,
  they do not partition it; the slot budget is a cluster budget with a stock ceiling of ten.
- **One relay-owned shared publication per Datastore**, its membership the union of every
  Database's safe table projections plus the heartbeat exception. Creating or migrating a
  Database edits only its member entries under the Datastore publication mutex. One Database
  must never run `ALTER PUBLICATION ... SET TABLE` over the shared object.
- **The published column set per table is the INTERSECTION over every grant on the database**,
  and the plaintext of any column with `classification != none` is never published to anyone.
  Concretely: publish `ssn` (which holds the mask) and exclude `__zs_raw__ssn` (which holds
  the plaintext).
- **The relay performs fan-out**, resolving
  `(datastore_id, physical_schema) -> database_id -> active Grant -> app_id` and sending an
  app-keyed frame to each active grantee. The current worker-local namespace filter and decode
  loop are deleted; the broker's `(app_id, collection)` routing table stays app-keyed.
- **Fan-out authority is control's Grant topology**, never `pg_auth_members` or a worker
  refresh map. Grant changes are revision barriers that purge old relay and worker queues
  before control exposes them.
- **`pg_logical_emit_message` is revoked from `PUBLIC`** on both four-argument overloads
  before the marker is trusted; no worker, app or relay role is granted it, and the migration
  service emits `(database_id, database_epoch)` in the widen transaction.

Two costs, both named. The owner loses plaintext reactivity on classified columns - a
subscription never carries the plaintext, for anybody, including the owning app. Reads still
do. And grant changes pay a relay-and-worker revision barrier, which can reconnect healthy
apps that shared a relay response.

### The stale-binding fence

A binding goes stale two ways and both are answered by whether
`SET LOCAL ROLE "zs_bind_<gid>_e<E>"` succeeds. Role membership answers "does this app still
hold a live grant"; the `_e<E>` answers "is the shape this isolate was built against still the
shape the database has". Neither is answered by anything the worker compares.

An apply that changes the schema advances the epoch `E -> E+1`, mints
`zs_bind_<gid>_e<E+1>` for every live grant and drops `zs_bind_<gid>_e<E-1>`. An isolate built
against `E-2` fails at `SET LOCAL ROLE`. Steady-state cost is zero: the epoch is a substring
of a role name the batch already sends. Live epochs are capped at two, fail-closed: **an apply
that cannot drop `E-1` refuses before any DDL commits and never advances to `E+1`.** The cost
is that an isolate at `E-1` loses its grace at the START of the apply and recovers by
re-resolving to `E`, so the front reap is correct only once the binding producer lands.

**Error taxonomy.** The classifier
(`crates/zeroship-data-postgres/src/pg_error.rs`, `is_missing_per_app_session_role`) today
matches SQLSTATE 22023 plus the exact role name and collapses it into `SCHEMA_NOT_PROVISIONED`
(`crates/zeroship-data-core/src/error.rs`). Under this design:

- `42501 permission denied to set role` -> `GRANT_REVOKED`. Terminal, 403-shaped, never
  retried, never falls back to the pool.
- `22023 role does not exist` **under a live grant** -> `SCHEMA_EPOCH_STALE`, retryable, the
  same condition `Verdict::ReResolve` already carries.
- `22023 role does not exist` with no live grant -> `SCHEMA_NOT_PROVISIONED`, as today.

Telling the second from the third needs no message sniffing: the classifier already composes
the exact role name it expects, and only needs to compose the epoch-bearing name and to know
from the injected binding whether a live grant exists.

**The `__zeroship_admin` schema is created**, in the one shape the privilege invariant permits
- state a separate service writes and the worker only reads:

- An installer in `db/migrations-ts/`. The deleted version was installed only by a
  `#[cfg(any(test, feature = "test-helpers"))]` helper, so this is new work, not a restoration.
- **Exactly one table**, holding the current schema epoch per database, written by the
  migration service inside the transaction that mints the new epoch's roles. Read-only to
  everyone but the migration service. The **data plane still reads nothing** - the role name
  carries the epoch precisely so no query has to. The control plane reads it to compose the
  binding it injects.
- **Zero worker-callable functions.** No `SECURITY DEFINER`, no `EXECUTE ... TO PUBLIC`, no
  `GRANT USAGE ON SCHEMA` to any app or grant role, no `PUBLIC` write grant on anything.

### Encryption

Today the key is `Hkdf::<Sha256>::new(Some(app_id.as_bytes()), root)`
(`crates/zeroship-data-core/src/encryption/keys.rs:391`) and
`canonical_aad(collection, column, row_pk)` binds a hardcoded `WIRE_VERSION_V1`
(`crates/zeroship-data-core/src/encryption/aad.rs:75`, `:93`) and nothing namespacing. That
fails in opposite directions on the two new axes: co-grant-holders derive different keys and
get an AEAD failure on data they are entitled to read, and one app across two databases
derives one key with no database in the AAD, so a ciphertext lifted from one database verifies
in the other. The target:

```
derive_key(root, database_id)
canonical_aad(WIRE_VERSION_V2, database_id, collection, column, row_pk)
```

Encryption stops fencing co-grant-holders and becomes purely at-rest, which is what it should
have been. If a column must be readable by the owner only, that is a column-level GRANT, and
on the subscription path a column the publication does not carry. Changing the salt changes
every derived key and changing the AAD changes every tag, so **this lands in the same change
that makes database ids exist**, not after.

### Metering

- **Op counts stay keyed on the app.** `db_reads` / `db_writes` / `db_rows_written` are
  emitted against the server-injected app id at the op boundary
  (`crates/zeroship-plugin-db/src/exec.rs`), and the app that issued the op consumed the
  compute.
- **The billing principal for a database is the creator**, because no app owns one. Any metric
  measuring the *resource* attributes to the creator; any metric measuring an *op* attributes
  to the app. Collapsing them is what makes one app pay for a co-tenant's bytes.
- **The database is a dimension with no producer today.** `UsageEvent.dims`
  (`crates/zeroship-core/src/usage_event.rs:37`) is constructed empty because
  `Meter::increment(app_id, metric, n)` (`crates/zeroship-metering/src/meter.rs:195`) and
  `MeterHandle::record(metric, n)` (`crates/zeroship-metering/src/lib.rs:80`) carry no third
  axis. Populating it means re-keying `AppCounters`: metering-core work, not filling in a
  field. The aggregate PK stays `(app_id, period, metric)`, so the dimension is observable and
  auditable but does not reach the spend engine - spend is an app-level control.
- **Two suppression holes close.** Failure is free today (every emit sits after the `?`, and a
  test asserts "the FAILED query did NOT bill"); emit `db_statement_us` in both arms, and
  change that regression test with it. Subscriptions are entirely unmetered; in the target the
  relay owns the shared slot, subscription time meters to the app and retained WAL to the
  creator.

### Creator surface

One `zeroship.jsonc` for the whole workspace. Databases are declared once at workspace level;
apps are declared beside them and bind to them.

```jsonc
"databases": {
  "main":      { "id": "dbs_...", "migrations": "./db/main",      "out": "./generated/zeroship/main" },
  "analytics": { "id": "dbs_...", "migrations": "./db/analytics", "out": "./generated/zeroship/analytics" }
},
"apps": {
  "storefront": { "app": "<uuid>", "database": "main"      },  // SINGULAR - one db per app
  "admin":      { "app": "<uuid>", "database": "main"      },  // two apps, one shared db
  "reporting":  { "app": "<uuid>", "database": "analytics" }
}
```

The map key is a **local label**, not a resolvable name: `"main"` refers to an entry a few
lines above it, nothing sends it to a server, and the CLI dereferences it locally before it
makes any request. Each entry carries its `dbs_...` id, absent on a fresh project and appended
by the first create - the same lifecycle `schema/project-v1.json` already specifies for `app`.

The `env.db` surface does not change at all:

```ts
await env.db.users.find({ where: { active: true } });     // UNCHANGED
await env.db.orders.insert({ ... });                      // refused at type level when readonly
await env.db.transaction(async (tx) => { ... });          // UNCHANGED
```

`installSchema` plants collections directly on the target with `Object.defineProperty`
(`sdks/bootstrap/src/install-schema.ts:1270`) alongside `transaction` and `live`, so with
several bindings a binding named `analytics` and a collection named `analytics` would be the
same key. With one database there is no binding level and no collision.
`RESERVED_ENV_DB_NAMES` (`sdks/bootstrap/src/install-schema.ts:984`) keeps its present
meaning. No call-site sweep, no regenerated types, no edits to `docs/reference/db.md`,
`examples/starter/` or `tests/golden_path.sh`.

### Transactions

`env.db.transaction()` keeps its present shape and covers exactly one database, because an app
sees exactly one. The multi-database transaction problem is closed by the entity model rather
than by a runtime check. `tx_conns`, `tx_claims`, `tx_waiters`, `savepoint_depths`,
`savepoint_emit_marks` and `pending_emits` (`crates/zeroship-plugin-db/src/context.rs`) stay
keyed on `app_id`, because `app_id` still determines the database. `TxRoute`
(`crates/zeroship-plugin-db/src/tx_route.rs`) is unchanged, and its continuation slot stays
keyed on the app id: SEC-1 is structural there, and the planted key must never become a
creator-facing name, or two co-resident apps both calling their database `main` would compare
equal.

### Teardown

**Delete an app** - `REVOKE` both edges of the app's grant role, then
`DROP ROLE zs_bind_<gid>_e<E>` for every live epoch. That is the complete teardown of an app's
access, and it is instant. No data is destroyed and no 409 is raised, because the app owns no
database. A deleted app never destroys data another app can still read.

**Delete a database** - only when its grant set is empty. The five-step order of
`crates/zeroship-plugin-db/src/drop_namespace.rs` is right; each step is re-keyed:

| step today | under the decoupling |
| --- | --- |
| 1. subscription gate, keyed on the app | keyed on the database: refuse while any subscription on any grant-holder is observable |
| 2. drain broker via `subscription_app_dropped` | the broker's routing table is unchanged, so this fans out to every grant holder |
| 3. consumer cancel **plus slot teardown** | **No shared CDC object is dropped.** The relay-owned slot, stream and publication live for the Datastore; remove only this Database's publication members and fan-out routes |
| 4. `DROP SCHEMA "<app_id>" CASCADE` | `DROP SCHEMA "db_<dbsid>" CASCADE` |
| 5. `DROP ROLE "app_<id>_role"` | `DROP ROLE zs_db_<dbsid>_{mig,rw,ro}`, still after the schema |

Step 3 is the one that fails silently if it is ported rather than re-keyed.

The workflow journal schema `app_<uuid>`
(`crates/zeroship-migrate-server/src/provisioning.rs`, duplicated deliberately at
`crates/zeroship-plugin-workflow/src/store/pg.rs`) **stays app-keyed** - a workflow run is app
state, not database state - and lives on the datastore holding that app's database.

### SQLite dev tier

One file per database, `zs-db-<dbsid>.sqlite`, ATTACHed under alias `db_<dbsid>`.
`attach_app_file` (`crates/zeroship-data-sqlite/src/lib.rs`) already is a per-database handle
under a different name, with an `app_id_cache` dedup set because SQLite errors on a duplicate
alias; the dedup key becomes the database id. Three fidelity gaps, all owed to
`docs/reference/sqlite-divergences.md`:

- **Grants are not enforceable.** SQLite has no roles and no column ACLs. The dev tier's grant
  fence is the Rust resolution layer only.
- **The schema epoch has no carrier**, since it rides a role name. A dev-tier equivalent is
  owed and is not specified here.
- **One writer per file**, so a shared database behaves *worse* in dev than in production -
  the inverse of the usual direction, and the one that gets filed as a bug.

---

## Why it is this way

**The role must be per grant, not per database.** `SET ROLE` authorizes against the transitive
closure of the memberships held by the role that *connected*, which here is always the shared
worker login, never the app. Revoking an app's edge therefore has no database consequence
while any parallel edge survives (measured on 17.11: with the worker serving two apps that
both hold the database role, the revoke leaves the read returning one row unchanged). A grant
role inherits exactly one database role, so per-statement confinement stays one database while
revocation still bites. Confinement and revocation stop being a trade.

**`WITH SET FALSE` on the grant-to-database edge is load-bearing.** Without it the worker
assumes the database role directly and the chain is decorative. Measured on 17.11 with two
grants on one database: the worker assuming the database role gets
`permission denied to set role`; narrowed to grant A it reads one row; after
`REVOKE zs_db_1_rw` from A's role it gets `permission denied for schema`; co-tenant grant B is
unaffected.

**`WITH INHERIT FALSE` on the worker-to-grant edge is required, and the role attribute is not
a substitute.** Measured on 18.4, three arms differing in one variable:

| Configuration | Bare `SELECT` as the login role |
| --- | --- |
| plain `GRANT` of the intermediate to `w`, `w` INHERIT | **1 row returned** |
| same grant, `ALTER ROLE w NOINHERIT` | **1 row returned** |
| `GRANT ... TO w WITH INHERIT FALSE`, `w` INHERIT | `ERROR: permission denied for schema` |

PostgreSQL 16+ records `inherit_option` per membership at grant time, and a pre-existing
membership stays inheriting when the attribute is flipped. Confirmed across 16.14, 17.11 and
18.4; `SET LOCAL ROLE` still works in every arm. Below 16 the option does not exist and this
design has no fence.

**The worker may never hold a direct membership in a database role.** One such grant, added
for convenience or by a provisioning path that predates this rule, restores the unrevocable
behaviour above, and nothing in PostgreSQL will complain because both memberships are
individually legal. It is catalog-checkable, and `db_posture`'s boot check already counts
every inheriting membership row rather than checking a pair
(`INHERITED_MEMBERSHIPS_SQL`, `crates/zeroship-worker/src/db_posture.rs:42`).

**No CRUD code may reach a raw connection, and this must stay a compile-time property.** The
role fence is applied by two functions
(`crates/zeroship-data-postgres/src/pg_autocommit.rs` and
`crates/zeroship-data-postgres/src/postgres.rs`, `apply_per_app_role`); anything issuing SQL
outside them is unfenced. That hole is closed TODAY, and only because
`PgSqlExecutor::pool_handle` is `#[cfg(any(test, feature = "test-helpers"))]`
(`crates/zeroship-data-postgres/src/postgres.rs:513-515`) - its remaining callers are in
`crates/zeroship-plugin-db/src/crud/mask_drift.rs`, itself test-gated with zero production
callers. Do not ungate it. Under a private database the blast radius of one unfenced statement
was a single app's schema; under sharing it is every database in the datastore, and
`WITH INHERIT FALSE` only converts such a statement from succeeding to failing at runtime. The
compile-time route is what stops the site existing.

**The irreducible limit.** PostgreSQL has no server-side notion of which app a shared-login
session is acting for; `session_user` is fixed at authentication, and `SET ROLE` is a
**lateral move inside the closure**, never a one-way narrowing - a session narrowed to one
grant role can assume a sibling. So every available fence decides whether a grant is *alive*,
never whether the worker picked the grant matching the dispatch. That binding is worker-side,
enforced by Rust provenance and the absence of a raw-SQL surface. Per-app logins would not
change it: the process would then hold every tenant's credential.

**Column grants add, they never subtract.** A table-level `GRANT SELECT` alongside a column
list returns the plaintext. `ALTER DEFAULT PRIVILEGES` has no column-list form. Both must
therefore be deleted rather than supplemented. Schema evolution then fails closed by a
PostgreSQL property, not by a reconciler: a column added after the grant carries no ACL entry
and is unreadable, so the migration service carries no correctness obligation beyond emitting
the list.

**Logical decoding consults no ACL and no RLS.** The same role refused `SELECT ssn` receives
the plaintext in the decoded stream when the publication has no column list. The decode path
runs on the worker's own login (`crates/zeroship-plugin-db/src/change_stream_pg.rs`), which
the boot posture requires to hold `REPLICATION` and `BYPASSRLS`. A publication column list
*does* filter decoded output and is a genuine server-side fence - and PostgreSQL **refuses
conflicting column lists for one table across the publications named on one decode stream**,
so a table has exactly one published column set per stream and the shared stream serves the
weakest reader.

**The column list is incompatible with `REPLICA IDENTITY FULL`, and neither DDL step refuses.**
Measured on 17.11: adding `REPLICA IDENTITY FULL` to a table that already has a column list is
accepted, and creating a column list on a table already `FULL` is accepted; then every `UPDATE`
and `DELETE` fails with `cannot update table "t" ... Column list used by the publication does
not cover the replica identity`. The symptom is not a CDC fault but a table that has silently
become append-only on the creator's write path. `crates/zeroship-plugin-db/src/wal_consumer.rs`
records that fixing delete-filtering on non-key columns *needs* `REPLICA IDENTITY FULL`, so the
two features are mutually exclusive as designed and whichever is given up must be given up
explicitly, with a refusal at the authoring boundary.

**`BYPASSRLS` does not follow through `SET ROLE`.** Measured on 18.4, the same login sees one
row as itself and zero after narrowing. RLS is not required by this design and is not proposed
here, but it becomes usable on the query path, which it is not today. It stays void on the
decode path.

**Scope facts that decide cardinality.** Measured on 18.4: `pg_publication` and
`pg_publication_rel` have `relisshared = f`, so publications are datastore-scoped and the same
name in two databases of one cluster is two independent objects. `pg_authid` and
`pg_auth_members` have `relisshared = t`, so every grant role and every live epoch competes in
one cluster namespace. Replication slots are cluster-scoped with `max_replication_slots`
defaulting to **10** at `context = postmaster` (raising it is a restart) and `SQLSTATE 53400`
past it - so one slot per Datastore means at most ten Datastores on a stock cluster.
`max_slot_wal_keep_size` measures `-1`, unbounded, at `context = sighup`: bounding it is a
blast-radius cap, not a fix, and does not clean up an abandoned slot.

**The catalog costs the role graph was suspected of both measure at zero.** `CREATE ROLE`
inside an apply bracket does not serialize applies in other databases of the same cluster
(session 1 holding an uncommitted `CREATE ROLE`, session 2 with `lock_timeout = '3s'` in
another database: `CREATE ROLE` in 108 ms). `SET ROLE` is flat across a 2000x growth in
`pg_auth_members` - 2000 `SET ROLE` + `RESET ROLE` server-side in 6 ms at 3, 103, 1103 and
6103 membership rows, about 1.5 us per statement at both ends. That is why the epoch rides the
role name instead of an extra `SELECT epoch` in the setup batch: the name form cannot be
removed without removing the connection.

**`SET LOCAL ROLE` must remain the FIRST statement of the setup batch.** PostgreSQL aborts a
simple-query batch at the first failing statement and emits exactly one `ErrorResponse`. If
anything is placed before it, that statement's failure masks the role failure, the SQLSTATE
taxonomy silently collapses, and because the epoch rides the same role name, a rotated epoch
surfaces as whatever the earlier statement failed with.

**The two role failures split by SQLSTATE alone**, measured on 16.14 from a real non-superuser
session (invisible to a superuser, whose `SET ROLE` permission is checked against
`session_user`): role does not exist is `22023 invalid_parameter_value`; role exists but the
session is not a member is `42501 insufficient_privilege`. `22023` is the generic bad-GUC code
shared with `SET statement_timeout = 'yes'`, which is why the classifier pins the exact role
name alongside it; `42501` needs no such qualifier.

**Revocation lands within one in-flight transaction, not one statement.** Measured on 18.4 on
one held connection with the backend pid printed on both sides: after a `REVOKE` from a second
connection, the next `SET LOCAL ROLE` on the same backend fails `42501`, no reconnect. A
session running as a role that *inherits* the schema privilege fails on the very next
statement; a session that has *assumed* a role holding it directly continues to the end of the
transaction. This design assumes the grant role, so the bound is one transaction, capped only
by `DB_IDLE_IN_TX_TIMEOUT_MS` and `DB_STATEMENT_TIMEOUT_MS`
(`crates/zeroship-data-core/src/budgets.rs`), neither of which bounds total transaction
duration. Re-granting restores service on the same warm connection - which a monotonic
incarnation id with permanent tombstones cannot express, and revoke-then-regrant is a
legitimate state while "same app id, different app" is not reachable at all, typed ids being
UUIDv7.

**`max_identifier_length` is 63 and PostgreSQL truncates past it silently.** `zs_bind_` (8) +
a typed app id (26) + `_e` + the epoch digits leaves headroom, but the epoch sits at the END of
the name, so a future prefix change that pushes past 63 would collapse two epochs onto one role
rather than error. The composer must refuse a name it would have truncated.

**The apply cannot be one transaction.** `crates/zeroship-migrate-core/src/engine.rs` states
its contract - everything ahead of it commits in its own transaction - and the host loops the
engine once per IR file, with earlier files already committed when a later one fails. The
engine's crash recovery is journal-driven on exactly that basis. A wrapping transaction would
have to swallow the journal bootstrap and destroy that recovery model.

**The apply ledger cannot close exactly once across a crash, and that is structural.** The
audit row and the schema live in different databases - the store opens its own connection on
the control DSN - so no transaction spans both. The ledger is an audit projection with
at-least-once closure. The authority for "what schema does this database have" is the app
database's journal plus its head row, which do move together.

**The worker-internal `env_vars` map is a disclosure channel by construction.**
`crates/zeroship-runtime/src/core/init.rs:3516` copies every entry into `process.env`, so
anything placed there is readable by app JS and by any npm package that walks `Object.keys`. It
may carry only identifiers the app already possesses - today exactly `APP_ID` and
`ZEROSHIP_DEPLOY_ID` (`crates/zeroship-worker/src/cache.rs`). The `ds` field is the sharpest
edge: `DbResourceKey` is stable per datastore, so two apps under one actor that read equal
digests have confirmed co-residency, which makes noisy-neighbour and resource-exhaustion
attacks aimable rather than speculative. Being unforgeable is not sufficient; the map is
readable.

**Cross-creator table sharing is refused permanently.** Three independent reasons:

1. **The unmask policy is authored by the READING app.**
   `crates/zeroship-plugin-db/src/crud/mask_policy.rs` states it: the policy comes from the
   creator's own source, at boot, and nowhere else on the PG arm; the app declares
   `defineMaskPolicy()`, `installSchema` flushes it into that isolate's cache, and there is no
   durable policy store. A co-grant-holder ships a permissive policy in its own bundle and
   unmasks the owner's PII/PHI/PCI. The owner has no artifact anywhere that the co-tenant's
   isolate consults. The catalog sentinels do not close this: they carry mask kind and
   classification, not an actor-to-classification policy.
2. **A PostgreSQL schema has exactly one owner, and ownership IS the migrator's authority**
   (`ALTER SCHEMA ... OWNER TO`, `crates/zeroship-migrate-server/src/provisioning.rs:198`).
   Owner privileges are implicit and unrevokable, and there is no second owner slot.
3. **The apply lock is on the wrong axis and the journal has no tenant column.** The only
   apply-time lock over the publication name hashes the app id, so two apps in one database
   take different keys and their DDL interleaves with no mutual exclusion.

Within one creator, a co-grant-holder mis-declaring a mask policy is not a boundary crossing.
Across creators it is, and no role fixes it. `readwrite` and `readonly` grants are therefore
restricted to apps under the same creator, which the workspace model satisfies by
construction.

**What this design does NOT deliver is shared evolution across creators.** Two creators
jointly evolving one schema needs adjudicated multi-writer DDL, in which the migrator stops
being least-privilege-by-ownership and becomes a policy-adjudicated writer arbitrating
per-table claims between peer drafts, with no merge rule for two peers under
escalation-reject. That puts creator-influenced policy inside the one service trusted
precisely because it does not execute creator code. If the requirement is cross-creator shared
evolution, this is the wrong design and multi-writer DDL is the actual project. Confirm that
before anything is built.

**Two costs are PostgreSQL constraints rather than choices, and are accepted:** classified
columns lose plaintext reactivity for everyone including the owner (one published column set
per table per decode stream); and blanket table grants plus prospective default privileges are
deleted, so a migration that fails to regenerate explicit per-column grants leaves the database
unreadable rather than over-readable - the right failure direction, and still a failure.

**Three residual gaps are accepted, not solved here.** A schema change means rebuilding the
workspace, and apps deploy independently, so there is a window in which one is rebuilt and
another is not - ordinary shared-dependency mechanics, the creator's risk, and the platform's
job is to make the mismatch loud rather than prevent it. Worker boot becomes fatal on any bad
datastore, an availability regression traded for a boundary. And per-datastore admission
control does not exist: spend limits throttle an app's requests and cannot protect a shared
datastore from an app under its limit.

---

## Open

1. **Do the Datastore, Database and Grant entities ship, and in what order?**
   NEEDS-DECISION. Nothing in this design exists in the tree, and this item heads the longest
   dependency chain in the proposal set: the encryption re-salt, the epoch's database keying,
   the deploy-gate re-key, the CDC relay's fan-out topology and the creator-facing config
   shape all sit behind it. Until it is answered the rest of this list cannot be scheduled.

2. **Supply the schema epoch producer.**
   BUILDABLE, 8h. The consumer ships and is tested: `SchemaEpoch`
   (`crates/zeroship-plugin-db/src/transaction/reducer/identity.rs:97`), the comparison at
   `:265` returning `Verdict::ReResolve`, and the adapter from a classified session-setup
   outcome into that verdict. **The machine is a tautology in production.** The single
   production construction site
   (`crates/zeroship-plugin-db/src/transaction/driver.rs:143-145`) mints incarnation 0, domain
   `(0,0)`, epoch 0, `Stable` and an empty ceiling, and echoes the expectation back as the
   observation, so `classify` can only return `Current` and the three typed denial codes are
   unreachable outside tests. The work is the record, the migration-service write and the
   control-plane read that composes the binding.

3. **Where does the owner-side unmask policy live, and what reads it?**
   NEEDS-DECISION. Column grants fence the plaintext but not the platform's own privileged
   unmask path, which is gated by a document the READER wrote. Blocks cross-creator co-grants
   and nothing else; the same-creator restriction ships without it.

4. **What bounds total transaction duration?**
   NEEDS-DECISION. `DB_STATEMENT_TIMEOUT_MS` bounds one statement and
   `DB_IDLE_IN_TX_TIMEOUT_MS` one idle gap; neither bounds the transaction, and
   `transaction_timeout` occurs zero times in `crates/`, `db/` and `deploy/`.
   `env.db.transaction()` holds a dedicated connection for a whole JS callback, which is the
   shape that defeats both. Blocks publishing a numeric revocation-lag guarantee, not the
   mechanism.

5. **Who runs the resource-measuring producer?**
   NEEDS-DECISION. Per-database `db_bytes_stored` (summed `pg_total_relation_size`) and
   per-datastore `db_wal_retained_bytes` need a privileged connection in a process that does
   not execute creator code. **No CDC relay crate exists**, and the process holding the
   replication connection today IS the worker, so this is a new privileged service, not a free
   rider. Nothing in production reads `pg_stat_database`, `pg_stat_statements` or any
   relation-size function today, so decoupled apps would post identical `db_reads` while one
   holds 400 GB and the other 40 MB. Blocks fair billing on a shared datastore, not isolation.

6. **What re-validates "same creator" after issuance?**
   NEEDS-DECISION. The predicate is "same owning user id", evaluated once at grant issuance.
   `Resource` has only `App` and `Any`, and the sole production write to `app_members` is one
   INSERT at app creation. The first app-transfer or team feature silently turns every existing
   co-grant into a cross-creator grant. Either the grant keys on the principal and re-checks on
   every binding-table refresh, or a transfer enumerates and refuses-or-revokes outstanding
   co-grants.

7. **What identity scope does a shared database imply?**
   NEEDS-DECISION. Two apps sharing one database write DIFFERENT pairwise subjects for the SAME
   human, because the sector is per-app: `sector_identifier` is `NOT NULL` with one row per app
   (`db/migrations-ts/20260702000200_control_tables.ts:56`). A shared `users` table gets two
   rows per person and a foreign key written by app A does not join to a row app B created.
   Nothing raises; the data is simply wrong. The window is open now and closes at the first
   post-launch login - the sector is immutable by trigger because `oauth_refresh_tokens.sub`
   stores a snapshot for refresh-family kill markers, so changing it later de-aligns stored
   markers and revocation silently stops matching. Today there are no stored refresh tokens, so
   this is a derivation change plus a schema move with no data migration. Blocks the headline
   row of this design: N apps reaching the same tables under one creator. Entangled with the
   workspace/team container decision, since "project", "workspace" and "explicit grant" are
   three different answers.

8. **Make `app` plural in the project config without losing cross-target protection.**
   NEEDS-DECISION. `schema/project-v1.json` declares `app` as a single string, and the
   environments block requires `app` and `control` and makes them explicitly NON-inheritable,
   because an environment that names a control and inherits the root app is exactly the silent
   cross-targeting that rule prevents. With N apps an environment must name a deploy target per
   app and that property has to survive. Getting it wrong points a production environment at a
   staging app id, silently.

9. **Which of the column list and `REPLICA IDENTITY FULL` is given up?**
   NEEDS-DECISION. They are mutually exclusive, ordering does not save it, and the losing
   combination is silently configurable with the symptom landing on the creator's write path.
   Whichever survives, the migration service must refuse the other at the authoring boundary,
   because PostgreSQL will not. Not reachable until publication column lists ship.

10. **Re-measure the probe set on the PostgreSQL major the platform deploys.**
    BUILDABLE, 4h. Every measurement in "Why it is this way" is 18.4 or 17.11 except the
    inherit-option arm (also 16.14) and the SQLSTATE split (16.14).
    `deploy/compose/docker-compose.yml:73` pins `postgres:16`. Nine probes in a throwaway
    container; roles and memberships are cluster-shared, so role DDL must never run against a
    shared instance.

11. **Measure per-backend membership cache construction at CONNECT time.**
    BUILDABLE, 2h. `SET ROLE` is measured flat on an established backend; a NEW backend still
    pays to build its membership set, and the role graph multiplies exactly the catalog that
    cost reads. Pooling amortizes it; it does not remove it. One probe, two scales.

12. **Prove `compio-postgres` surfaces `42501` distinguishably from the multi-statement
    session-setup batch.**
    BUILDABLE, 2h. The taxonomy splits `GRANT_REVOKED` from `SCHEMA_NOT_PROVISIONED` on the
    code. If the driver collapses or reorders errors from a simple-query batch whose FIRST
    statement fails, the split does not exist. One live test.

13. **Re-prove the pooled-connection reset against a narrowed role.**
    BUILDABLE, 3h. `crates/zeroship-plugin-db/src/exec.rs` already runs the role and timeout
    guards via `SET LOCAL` inside an explicit transaction so they auto-revert at COMMIT and at
    the implicit ROLLBACK on drop, covering a setup error, a query error, or a cancellation
    between setup and the would-be reset. That reasoning does not change when the role names a
    database, but the residue it prevents does: a leaked role today is one app's own schema and
    under sharing it is a co-tenant's. Needs a test that checks out, narrows, cancels
    mid-flight, and asserts the next checkout cannot reach the first database.

14. **Write the three mandatory `SET LOCAL ROLE` regression tests.**
    BUILDABLE, 6h. Nothing today fails if the fence is refactored away, because nothing today
    can be revoked. (a) Grant, query succeeds; REVOKE from a separate connection; the same warm
    isolate on the same pooled connection fails with `GRANT_REVOKED`, no eviction, no restart.
    (b) A statement issued without the session-setup batch fails with `permission denied`,
    proving the `WITH INHERIT FALSE` posture rather than the presence of a call. (c) Rotate the
    epoch, reap `E-1`, and assert the stale isolate gets `SCHEMA_EPOCH_STALE` rather than
    `SCHEMA_NOT_PROVISIONED`.

15. **Does the CONFINED ceiling need a grant-authority key?**
    NEEDS-DECISION, blocking nothing now. Nothing in the ceiling vocabulary describes ACL
    authorship, so a creator draft cannot widen an ACL today by absence rather than by rule. It
    becomes load-bearing the moment any `access.*` key is granted to a creator draft.

---

## History

Deliberation for this design lives in
`docs/proposals/2026-08-26-runtime-db-binding-decision-log.md` and the index at
`docs/proposals/2026-08-26-runtime-db-binding-00-index.md`. Sibling specs:
`docs/proposals/2026-08-28-cdc-service.md`,
`docs/proposals/2026-08-28-deploy-schema-precondition.md`,
`docs/proposals/2026-08-31-data-crate-shape.md`. Architecture:
`docs/architecture/data-system.md`.

Do-not notes, each recording something that was tried or specified and broke:

- **Do not put the binding in the worker-internal `env_vars` map.** An earlier draft
  prescribed it "on the same path as `APP_ID`", defended on the ground that creator `vars`
  cannot shadow a worker-internal entry. That is true and answers the wrong question:
  shadowing is a forgery concern, and the requirement is disclosure. `init.rs` copies the map
  into `process.env`.
- **Do not interpose a per-app hub role between the worker and the database role.** Measured:
  the closure is only empty when NO app on that worker holds the role, so two hops buy nothing
  one hop does not. Transitivity is why, not a workaround for it.
- **Do not use `ALTER ROLE <login> NOINHERIT` in place of `WITH INHERIT FALSE`.** Measured on
  three majors: the role attribute does nothing to an existing membership.
- **Do not `DROP ROLE` on revoke.** A dropped role yields `22023 role does not exist`; a
  revoked one yields `42501 permission denied to set role`. The error taxonomy rests on that
  split, and re-granting restores service on the same warm connection. `DROP ROLE` belongs to
  exactly two places: the epoch reaper and database teardown.
- **Do not bundle the `E-1` reap into the rotation transaction.** Bundled, a failed reap means
  N DDL transactions committed, the schema at `v2`, the epoch stuck at `E`, and every retry
  failing on the same `DROP ROLE` forever - worse than the leak it guards. At the front the
  identical refusal costs a clean 409 with zero side effects. The failure is real: a bind role
  that has been granted a privilege OF ITS OWN cannot be dropped, which is exactly the
  violation of "no privileges of its own" this design forbids.
- **Do not key the rotation on "did this run apply anything".** A retry after a crash between
  the last DDL and T4 finds every version already applied and must still rotate; keying on
  work-done strands the database at `E` with a `v2` schema forever.
- **Do not ask the engine for one transaction covering the DDL.** An earlier version of the
  apply bullet did; the engine commits per IR file and its crash recovery is journal-driven on
  that basis.
- **Do not put any statement before `SET LOCAL ROLE` in the session-setup batch.** The
  simple-query batch aborts at the first failure and emits one `ErrorResponse`, so an earlier
  failure masks the role error and takes the epoch fence with it.
- **Do not port the deleted `__zeroship_admin` installer's statements.** It carried
  `GRANT INSERT, UPDATE, SELECT ON ... pitr_targets TO PUBLIC`, and a `GRANT USAGE ON SCHEMA`
  that was the reachability precondition for every public `EXECUTE` in it. An acceptance arm
  that audits routine grants while leaving `USAGE` in place is checking the lock and not the
  door.
- **Do not add a Rust epoch comparison on the SQLite arm only.** A fence that exists on one
  tier and not the other is how a divergence becomes a surprise; whatever the dev-tier
  equivalent is, it belongs in `docs/reference/sqlite-divergences.md` beside the grants entry.
- **Do not write "publish the mask sibling, exclude the parent".** The masking storage flip
  deleted the `_masked` sibling: the field's own column now holds the mask and `__zs_raw__<f>`
  holds the plaintext. Anything written against the old layout is inverted and would publish
  the plaintext. Probe transcripts in the deliberation docs use columns literally named
  `ssn_masked`; that is the probe's own naming, and the PostgreSQL grant and publication
  semantics they measure do not depend on it.
- **Do not trust a string-compared SQL test as evidence a statement executes.** Every
  PostgreSQL upsert was broken - `DO UPDATE SET "version" = COALESCE("version", 0) + 1` has
  target and `excluded` both in scope, so PostgreSQL refuses it `42702 ambiguous` - and it
  survived because the upserts that EXECUTE run on SQLite while the PostgreSQL ones only
  COMPARE STRINGS, one of them asserting the broken literal.
- **Do not fold the database into a metric name.** `zeroship.billing_metrics` is PK'd on
  `metric` alone and metric names are cluster-global.
- **Do not sweep `app_id -> database_id` mechanically over metering.** `db_reads`,
  `db_writes` and `db_rows_written` stay app-keyed and must be excluded by name.
- **Do not reintroduce `crates/zeroship-plugin-db/src/cross_app_fk.rs` (DELETED).** It had no
  production call site and its predicate was wrong in both directions: it blanket-refused
  cross-schema refs that a shared datastore makes legal, and permitted same-app refs that
  cross a database boundary. The live rule is `reject_cross_app_ref` in the engine plus
  schema-qualified REFERENCES rendering, restated as "a foreign key stays inside one database".
- **Do not treat the apply ledger as authoritative.** Any design that does is claiming a
  distributed transaction it does not have.
- **Do not reintroduce a control-plane-internal id-bearing migration route.** The earlier
  two-route split resolved a creator-facing `(project, name)` path to an id and forwarded it;
  both the split and the forward are deleted, along with
  `crates/zeroship-control/src/migrations_api.rs (DELETED)`. An "internal" route invites the
  belief that it is a privileged plane, and the day one accepts the control key instead of the
  caller's bearer, every creator holding a `dbs_...` inherits platform authority over that
  database.
- **Do not cite the physical schema name as a security leak.** It is public under
  id-addressing; mapping `db_<dbsid>` to something readable in error text is a
  message-quality item and must not be argued as a boundary.
